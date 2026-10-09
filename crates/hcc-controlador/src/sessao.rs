use crate::config::{AgentConfig, Transport};
use crate::local::LocalTransport;
use crate::servidor::Server;
use crate::ssh::{NativeRunner, SshTransport};
use base64::Engine;
use hcc_protocolo::{ActResult, Action, Command, Connector, Observation, Session, SessionError, ScreenshotResult};
use serde::de::DeserializeOwned;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub struct AgentConnector {
    pub config_path: Option<PathBuf>,
    pub default_command: Vec<String>,
}

impl Connector for AgentConnector {
    type S = AgentSession;

    async fn connect(&self, cancel: &CancellationToken) -> Result<AgentSession, SessionError> {
        let path = self.config_path.clone().or_else(|| std::env::var_os("HCC_AGENT_CONFIG").map(PathBuf::from))
            .ok_or_else(|| SessionError::Config("HCC_AGENT_CONFIG não definido".into()))?;
        let config = tokio::task::spawn_blocking(move || AgentConfig::load(&path)).await
            .map_err(|error| SessionError::Config(error.to_string()))?
            .map_err(SessionError::Config)?;
        AgentSession::start(config, &self.default_command, cancel.clone()).await
    }
}

enum AgentTransport {
    None,
    Local(LocalTransport),
    Ssh(SshTransport),
}

pub struct AgentSession {
    server: Server,
    transport: AgentTransport,
    timeout: Duration,
    cancel: CancellationToken,
    closed: bool,
}

impl AgentSession {
    pub async fn start(config: AgentConfig, default_command: &[String], cancel: CancellationToken) -> Result<Self, SessionError> {
        if cancel.is_cancelled() {
            return Err(SessionError::Cancelled("controle cancelado ou encerrado".into()));
        }
        let server = Server::start(cancel.clone()).await?;
        let mut session = Self { server, transport: AgentTransport::None, timeout: config.request_timeout, cancel, closed: false };
        session.transport = match config.transport {
            Transport::Local => AgentTransport::Local(LocalTransport::start(
                config.command.as_deref().unwrap_or(default_command), session.server.connection()).await?),
            Transport::Ssh => {
                let result = tokio::select! {
                    biased;
                    () = session.cancel.cancelled() => Err(SessionError::Cancelled("controle cancelado ou encerrado".into())),
                    result = SshTransport::start(&config, session.server.connection(), Arc::new(NativeRunner)) => result,
                };
                AgentTransport::Ssh(result?)
            }
        };
        session.wait_ready().await?;
        Ok(session)
    }

    pub fn is_closed(&self) -> bool { self.closed }

    async fn wait_ready(&mut self) -> Result<(), SessionError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if self.cancel.is_cancelled() {
                return Err(SessionError::Cancelled("controle cancelado ou encerrado".into()));
            }
            if self.server.startup_status()? { return Ok(()); }
            let status = match &mut self.transport {
                AgentTransport::None => None,
                AgentTransport::Local(local) => local.try_wait().map_err(SessionError::Startup)?,
                AgentTransport::Ssh(ssh) => ssh.try_wait().map_err(SessionError::Startup)?,
            };
            if let Some(code) = status {
                let detail = match &self.transport {
                    AgentTransport::Local(local) => local.last_error().await.map(|line| format!(": {line}")).unwrap_or_default(),
                    AgentTransport::None | AgentTransport::Ssh(_) => String::new(),
                };
                return Err(SessionError::Startup(format!("processo de conexão encerrou com código {code}{detail}")));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SessionError::StartupTimeout("agente não respondeu; confira usuário logado, tarefa e canal de retorno".into()));
            }
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => return Err(SessionError::Cancelled("controle cancelado ou encerrado".into())),
                () = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
    }

    async fn rpc<T: DeserializeOwned>(&mut self, id: String, command: Command, operation: &str) -> Result<T, SessionError> {
        self.server.check()?;
        let (pending, reply) = self.server.register(id);
        let result = tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(SessionError::Cancelled("controle cancelado ou encerrado".into())),
            result = async {
                self.server.send(command).await?;
                let post = reply.await.map_err(|_| SessionError::Cancelled("controle cancelado ou encerrado".into()))?;
                self.server.check()?;
                if let Some(error) = post.error { Err(SessionError::Agent(error)) }
                else {
                    post.result.ok_or_else(|| SessionError::Agent("resposta do agente sem result".into()))
                        .and_then(|value| serde_json::from_value(value).map_err(|error| SessionError::Agent(error.to_string())))
                }
            } => result,
            () = tokio::time::sleep(self.timeout) => {
                if let Err(error) = self.close().await { eprintln!("não foi possível encerrar controle: {error}"); }
                Err(SessionError::Timeout(format!("agente não respondeu a {operation}; execução encerrada")))
            }
        };
        drop(pending);
        result
    }

    fn ttl_ms(&self) -> u64 { self.timeout.as_millis().min(u128::from(u64::MAX)) as u64 }

    fn close_sync(&mut self) {
        if self.closed { return; }
        self.closed = true;
        self.server.begin_close();
        let pid = self.server.pid();
        let result = match &mut self.transport {
            AgentTransport::None => Ok(()),
            AgentTransport::Local(local) => local.close_sync(),
            AgentTransport::Ssh(ssh) => ssh.close_sync(pid),
        };
        if let Err(error) = result { eprintln!("não foi possível encerrar controle: {error}"); }
        self.server.close_sync();
    }
}

impl Session for AgentSession {
    async fn observe(&mut self) -> Result<Observation, SessionError> {
        let id = Uuid::new_v4().simple().to_string();
        self.rpc(id.clone(), Command::Observe { id, ttl_ms: self.ttl_ms() }, "observe").await
    }

    async fn act(&mut self, observation_id: &str, action: &Action) -> Result<ActResult, SessionError> {
        let id = Uuid::new_v4().simple().to_string();
        self.rpc(id.clone(), Command::Act { id, ttl_ms: self.ttl_ms(), action: action.clone(), observation_id: observation_id.into() }, "act").await
    }

    async fn screenshot(&mut self) -> Result<Vec<u8>, SessionError> {
        let id = Uuid::new_v4().simple().to_string();
        let result: ScreenshotResult = self.rpc(id.clone(), Command::Screenshot { id, ttl_ms: self.ttl_ms() }, "screenshot").await?;
        base64::engine::general_purpose::STANDARD.decode(result.png)
            .map_err(|error| SessionError::Agent(error.to_string()))
    }

    async fn close(&mut self) -> Result<(), String> {
        if self.closed { return Ok(()); }
        self.server.begin_close();
        let pid = self.server.pid();
        let result = match &mut self.transport {
            AgentTransport::None => Ok(()),
            AgentTransport::Local(local) => local.close().await,
            AgentTransport::Ssh(ssh) => ssh.close(pid).await,
        };
        self.server.close().await;
        self.closed = true;
        if let Err(error) = result && matches!(&self.transport, AgentTransport::Local(_)) {
            eprintln!("não foi possível encerrar agente: {error}");
        }
        Ok(())
    }
}

pub(crate) fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return -signal;
    }
    status.code().unwrap_or(-1)
}

impl Drop for AgentSession {
    fn drop(&mut self) { self.close_sync(); }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ssh::{ProcessCommand, RunFuture, Runner};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output, Stdio};

    struct FailingStopRunner;

    impl Runner for FailingStopRunner {
        fn run(&self, command: ProcessCommand) -> RunFuture<'_> {
            Box::pin(async move { self.run_sync(command) })
        }

        fn spawn(&self, _: ProcessCommand) -> Result<tokio::process::Child, String> {
            NativeRunner.spawn(ProcessCommand { program: "/bin/sh".into(),
                args: vec!["-c".into(), "exec sleep 60".into()], stdin: vec![],
                timeout: Duration::from_secs(60) })
        }

        fn run_sync(&self, command: ProcessCommand) -> Result<Output, String> {
            let payload: serde_json::Value = serde_json::from_slice(&command.stdin).unwrap();
            let (code, stdout, stderr) = match payload["operation"].as_str().unwrap() {
                "prepare" => (0, br#"{"executable":"agent.exe","valid":true}"#.to_vec(), vec![]),
                "start" => (0, b"{}".to_vec(), vec![]),
                "stop" => (1, vec![], b"cleanup failed".to_vec()),
                other => panic!("unexpected operation: {other}"),
            };
            Ok(Output { status: ExitStatus::from_raw(code << 8), stdout, stderr })
        }
    }

    #[tokio::test]
    async fn close_reports_remote_stop_failure_only_on_stderr() {
        const MARKER: &str = "HCC_SESSION_CLOSE_STDERR_TEST";
        if std::env::var_os(MARKER).is_some() {
            let artifact = tempfile::NamedTempFile::new().unwrap();
            let cancel = CancellationToken::new();
            let server = Server::start(cancel.clone()).await.unwrap();
            let config = AgentConfig { transport: Transport::Ssh, host: Some("vm-a".into()),
                agent_path: Some(artifact.path().into()), ..AgentConfig::default() };
            let transport = SshTransport::start(&config, server.connection(), Arc::new(FailingStopRunner)).await.unwrap();
            let mut session = AgentSession { server, transport: AgentTransport::Ssh(transport),
                timeout: Duration::from_secs(15), cancel, closed: false };
            assert_eq!(session.close().await, Ok(()));
            assert!(session.is_closed());
            assert_eq!(session.close().await, Ok(()));
            return;
        }
        let output = tokio::time::timeout(Duration::from_secs(5),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "sessao::tests::close_reports_remote_stop_failure_only_on_stderr", "--nocapture"])
                .env(MARKER, "1").stdout(Stdio::null()).stderr(Stdio::piped())
                .kill_on_drop(true).output()).await.unwrap().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{stderr}");
        assert!(stderr.contains("não foi possível remover tarefa HangarComputerControl-"), "{stderr}");
        assert!(stderr.contains("inicialização Windows falhou: cleanup failed"), "{stderr}");
    }
}
