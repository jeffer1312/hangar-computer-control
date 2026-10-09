//! Windows transport over ssh.

use std::ffi::OsString;
use std::future::Future;
use std::pin::Pin;
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use base64::{Engine, engine::general_purpose::STANDARD};
use hcc_protocolo::{Connection, SessionError};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

const BOOTSTRAP: &str = include_str!("bootstrap.ps1");
const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(20);
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct ProcessCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
}

impl std::fmt::Debug for ProcessCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessCommand").field("program", &self.program)
            .field("args", &self.args).field("stdin", &"<oculto>")
            .field("timeout", &self.timeout).finish()
    }
}

pub type RunFuture<'a> = Pin<Box<dyn Future<Output = Result<Output, String>> + Send + 'a>>;

pub trait Runner: Send + Sync {
    fn run(&self, command: ProcessCommand) -> RunFuture<'_>;
    fn spawn(&self, command: ProcessCommand) -> Result<Child, String>;
    fn run_sync(&self, command: ProcessCommand) -> Result<Output, String>;
}

pub struct NativeRunner;

impl Runner for NativeRunner {
    fn run(&self, command: ProcessCommand) -> RunFuture<'_> {
        Box::pin(async move {
            let child = Command::new(&command.program).args(&command.args)
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
                .kill_on_drop(true).spawn().map_err(|e| e.to_string())?;
            let mut child = ReapingChild(child);
            let mut stdin = child.0.stdin.take().ok_or("processo sem entrada padrão")?;
            let mut stdout = child.0.stdout.take().ok_or("processo sem saída padrão")?;
            let mut stderr = child.0.stderr.take().ok_or("processo sem saída de erro")?;
            let io = async {
                let input = async move {
                    let result = async {
                        stdin.write_all(&command.stdin).await?;
                        stdin.shutdown().await
                    }.await;
                    drop(stdin);
                    // A failed remote command can close stdin before reporting its error.
                    match result {
                        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                        result => result,
                    }
                };
                let out = async move {
                    let mut bytes = Vec::new();
                    stdout.read_to_end(&mut bytes).await?;
                    Ok::<_, std::io::Error>(bytes)
                };
                let err = async move {
                    let mut bytes = Vec::new();
                    stderr.read_to_end(&mut bytes).await?;
                    Ok::<_, std::io::Error>(bytes)
                };
                let (status, (), stdout, stderr) = tokio::try_join!(child.0.wait(), input, out, err)?;
                Ok::<_, std::io::Error>(Output { status, stdout, stderr })
            };
            let result = match tokio::time::timeout(command.timeout, io).await {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(_) => Err("tempo limite excedido ao executar processo".into()),
            };
            if result.is_err() && let Err(error) = child.stop().await {
                eprintln!("falha ao encerrar processo: {error}");
            }
            result
        })
    }

    fn spawn(&self, command: ProcessCommand) -> Result<Child, String> {
        Command::new(&command.program).args(&command.args)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped())
            .kill_on_drop(true).spawn().map_err(|e| e.to_string())
    }

    fn run_sync(&self, command: ProcessCommand) -> Result<Output, String> {
        // Drop may run inside a Tokio runtime, where block_on would panic.
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all()
                .build().map_err(|e| e.to_string())?;
            runtime.block_on(NativeRunner.run(command))
        }).join().map_err(|_| "executor de processo encerrou inesperadamente".to_string())?
    }
}

struct ReapingChild(Child);

impl ReapingChild {
    async fn stop(&mut self) -> Result<(), String> {
        if self.0.try_wait().map_err(|e| e.to_string())?.is_some() { return Ok(()); }
        self.0.start_kill().map_err(|e| e.to_string())?;
        tokio::time::timeout(REAP_TIMEOUT, self.0.wait()).await
            .map_err(|_| "tempo limite ao aguardar encerramento do processo".to_string())?
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn stop_sync(&mut self) -> Result<(), String> {
        if self.0.try_wait().map_err(|e| e.to_string())?.is_some() { return Ok(()); }
        self.0.start_kill().map_err(|e| e.to_string())?;
        let deadline = Instant::now() + REAP_TIMEOUT;
        loop {
            if self.0.try_wait().map_err(|e| e.to_string())?.is_some() { return Ok(()); }
            if Instant::now() >= deadline {
                return Err("tempo limite ao aguardar encerramento do processo".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ReapingChild {
    fn drop(&mut self) {
        if let Err(error) = self.stop_sync() {
            eprintln!("falha ao encerrar processo: {error}");
        }
    }
}

pub struct SshTransport {
    runner: Arc<dyn Runner>,
    options: Vec<OsString>,
    host: String,
    task: String,
    executable: Option<String>,
    tunnel: Option<ReapingChild>,
    closed: bool,
}

#[derive(Deserialize)]
struct Prepared {
    executable: String,
    valid: bool,
}

impl SshTransport {
    pub async fn start(config: &crate::config::AgentConfig, connection: &Connection,
                       runner: Arc<dyn Runner>) -> Result<Self, SessionError> {
        let host = config.host.as_ref().ok_or_else(|| SessionError::Config("host ausente".into()))?;
        let artifact = config.agent_path.as_ref()
            .ok_or_else(|| SessionError::Config("agent_path ausente".into()))?;
        let artifact = tokio::fs::canonicalize(artifact).await
            .map_err(|e| SessionError::Startup(format!("não foi possível localizar agente: {e}")))?;
        let bytes = tokio::fs::read(&artifact).await
            .map_err(|e| SessionError::Startup(format!("não foi possível ler agente: {e}")))?;
        let digest: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        let port = reqwest::Url::parse(&connection.url).ok().and_then(|url| url.port_or_known_default())
            .ok_or_else(|| SessionError::Config("URL de conexão sem porta válida".into()))?;
        let mut options: Vec<OsString> = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"]
            .into_iter().map(OsString::from).collect();
        if let Some(proxy) = config.proxy_command.as_ref().filter(|p| !p.is_empty()) {
            options.extend(["-o".into(), format!("ProxyCommand={proxy}").into()]);
        }
        let mut args = options.clone();
        args.extend(["-N".into(), "-o".into(), "ExitOnForwardFailure=yes".into(), "-R".into(),
            format!("127.0.0.1:{port}:127.0.0.1:{port}").into(), host.into()]);
        let tunnel = runner.spawn(ProcessCommand { program: "ssh".into(), args,
            stdin: vec![], timeout: REMOTE_TIMEOUT }).map_err(SessionError::Startup)?;
        let mut transport = Self { runner, options, host: host.clone(),
            task: format!("HangarComputerControl-{}", uuid::Uuid::new_v4().simple()),
            executable: None, tunnel: Some(ReapingChild(tunnel)), closed: false };
        let result = async {
            let prepared = transport.remote(&json!({"operation": "prepare", "digest": digest,
                "shared_executable": config.shared_executable}), REMOTE_TIMEOUT).await?;
            let prepared: Prepared = serde_json::from_value(prepared)
                .map_err(|e| format!("resposta de preparação inválida: {e}"))?;
            transport.executable = Some(prepared.executable.clone());
            if !prepared.valid {
                let mut args = transport.options.clone();
                args.extend([artifact.as_os_str().to_owned(),
                    format!("{}:{}", transport.host, prepared.executable.replace('\\', "/")).into()]);
                let copied = transport.runner.run(ProcessCommand { program: "scp".into(), args,
                    stdin: vec![], timeout: Duration::from_secs(120) }).await
                    .map_err(|e| format!("falha ao enviar agente: {}", tail(&e, 1000)))?;
                if !copied.status.success() {
                    return Err(format!("falha ao enviar agente: {}", tail(&decode_stderr(&copied.stderr), 1000)));
                }
            }
            transport.remote(&json!({"operation": "start", "executable": prepared.executable,
                "digest": digest, "task": transport.task, "connection": connection}), REMOTE_TIMEOUT).await?;
            Ok::<_, String>(())
        }.await;
        if let Err(error) = result {
            // close reports its own failures without replacing the startup error.
            let _ = transport.close(None).await;
            return Err(SessionError::Startup(error));
        }
        Ok(transport)
    }

    pub fn try_wait(&mut self) -> Result<Option<i32>, String> {
        match self.tunnel.as_mut() {
            Some(tunnel) => tunnel.0.try_wait().map(|s| s.map(crate::sessao::exit_code))
                .map_err(|e| e.to_string()),
            None => Ok(None),
        }
    }

    fn remote_command(&self, payload: &Value, timeout: Duration) -> Result<ProcessCommand, String> {
        let encoded: Vec<u8> = BOOTSTRAP.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut args = self.options.clone();
        args.extend([self.host.clone().into(), "powershell".into(), "-NoProfile".into(),
            "-NonInteractive".into(), "-EncodedCommand".into(), STANDARD.encode(encoded).into()]);
        Ok(ProcessCommand { program: "ssh".into(), args,
            stdin: serde_json::to_vec(payload).map_err(|e| e.to_string())?, timeout })
    }

    async fn remote(&self, payload: &Value, timeout: Duration) -> Result<Value, String> {
        let result = self.runner.run(self.remote_command(payload, timeout)?).await
            .map_err(|e| format!("inicialização Windows falhou: {}", tail(&e, 1500)))?;
        parse_remote(result)
    }

    fn stop_command(&self, pid: Option<u32>) -> Result<ProcessCommand, String> {
        self.remote_command(&json!({"operation": "stop", "task": self.task,
            "executable": self.executable, "pid": pid}), STOP_TIMEOUT)
    }

    pub async fn close(&mut self, pid: Option<u32>) -> Result<(), String> {
        if self.closed { return Ok(()); }
        let remote = if self.executable.is_some() {
            match self.stop_command(pid) {
                Ok(command) => self.runner.run(command).await
                    .map_err(|e| format!("inicialização Windows falhou: {}", tail(&e, 1500)))
                    .and_then(parse_remote).map(|_| ()),
                Err(error) => Err(error),
            }
        } else { Ok(()) };
        let tunnel = match self.tunnel.take() {
            Some(mut tunnel) => tunnel.stop().await,
            None => Ok(()),
        };
        self.closed = true;
        self.cleanup_result(remote, tunnel)
    }

    pub fn close_sync(&mut self, pid: Option<u32>) -> Result<(), String> {
        if self.closed { return Ok(()); }
        let remote = if self.executable.is_some() {
            self.stop_command(pid).and_then(|command| self.runner.run_sync(command))
                .map_err(|e| format!("inicialização Windows falhou: {}", tail(&e, 1500)))
                .and_then(parse_remote).map(|_| ())
        } else { Ok(()) };
        let tunnel = match self.tunnel.take() {
            Some(mut tunnel) => tunnel.stop_sync(),
            None => Ok(()),
        };
        self.closed = true;
        self.cleanup_result(remote, tunnel)
    }

    fn cleanup_result(&self, remote: Result<(), String>, tunnel: Result<(), String>) -> Result<(), String> {
        let result = match (remote, tunnel) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(remote), Err(tunnel)) => Err(format!("{remote}; túnel SSH: {tunnel}")),
        };
        if let Err(error) = &result {
            eprintln!("não foi possível remover tarefa {}: {error}", self.task);
        }
        result
    }
}

impl Drop for SshTransport {
    fn drop(&mut self) {
        if !self.closed {
            // close_sync reports cleanup failures on stderr.
            let _ = self.close_sync(None);
        }
    }
}

fn parse_remote(result: Output) -> Result<Value, String> {
    if !result.status.success() {
        return Err(format!("inicialização Windows falhou: {}", tail(&decode_stderr(&result.stderr), 1500)));
    }
    let bytes = result.stdout.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&result.stdout);
    serde_json::from_slice(bytes).map_err(|e| format!("resposta Windows inválida: {e}"))
}

fn tail(text: &str, count: usize) -> &str {
    let start = text.char_indices().rev().nth(count.saturating_sub(1)).map(|(i, _)| i).unwrap_or(0);
    &text[start..]
}

fn decode_stderr(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) { return text.to_owned(); }
    const CP850: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜø£Ø×ƒáíóúñÑªº¿®¬½¼¡«»░▒▓│┤ÁÂÀ©╣║╗╝¢¥┐└┴┬├─┼ãÃ╚╔╩╦╠═╬¤ðÐÊËÈıÍÎÏ┘┌█▄¦Ì▀ÓßÔÒõÕµþÞÚÛÙýÝ¯´\u{00ad}±‗¾¶§÷¸°¨·¹³²■\u{00a0}";
    let table: Vec<char> = CP850.chars().collect();
    bytes.iter().map(|b| if *b < 128 { char::from(*b) } else { table[usize::from(*b) - 128] }).collect()
}
