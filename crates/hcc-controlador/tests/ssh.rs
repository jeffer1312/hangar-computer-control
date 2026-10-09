#![cfg(feature = "test-fake")]

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use base64::{Engine, engine::general_purpose::STANDARD};
use hcc_controlador::config::{AgentConfig, Transport};
use hcc_controlador::ssh::{NativeRunner, ProcessCommand, RunFuture, Runner, SshTransport};
use hcc_protocolo::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::process::Child;

const BOOTSTRAP: &str = include_str!("../src/bootstrap.ps1");
const REMOTE_EXE: &str = "C:\\Users\\Administrator\\AppData\\Local\\HangarComputerControl\\abc\\windows-agent.exe";

#[derive(Default)]
struct FakeRunner {
    calls: Mutex<Vec<(String, ProcessCommand)>>,
    results: Mutex<VecDeque<Result<Output, String>>>,
    tunnel_pid: Mutex<Option<u32>>,
    #[cfg(target_os = "linux")]
    tunnel_reaped_before_sync_stop: Mutex<Option<bool>>,
}

impl FakeRunner {
    fn new(results: Vec<Output>) -> Arc<Self> {
        Arc::new(Self { results: Mutex::new(results.into_iter().map(Ok).collect()), ..Self::default() })
    }

    fn record(&self, kind: &str, command: ProcessCommand) -> Result<Output, String> {
        self.calls.lock().unwrap().push((kind.into(), command));
        self.results.lock().unwrap().pop_front().expect("unexpected process invocation")
    }

    fn commands(&self) -> Vec<ProcessCommand> {
        self.calls.lock().unwrap().iter().map(|(_, c)| c.clone()).collect()
    }

    fn payloads(&self) -> Vec<Value> {
        self.commands().iter().filter(|c| !c.stdin.is_empty())
            .map(|c| serde_json::from_slice(&c.stdin).unwrap()).collect()
    }
}

impl Runner for FakeRunner {
    fn run(&self, command: ProcessCommand) -> RunFuture<'_> {
        Box::pin(async move { self.record("run", command) })
    }

    fn spawn(&self, command: ProcessCommand) -> Result<Child, String> {
        self.calls.lock().unwrap().push(("spawn".into(), command));
        let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_fake-agent"))
            .arg("--tunnel").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .kill_on_drop(true).spawn().map_err(|e| e.to_string())?;
        *self.tunnel_pid.lock().unwrap() = child.id();
        Ok(child)
    }

    fn run_sync(&self, command: ProcessCommand) -> Result<Output, String> {
        #[cfg(target_os = "linux")]
        if serde_json::from_slice::<Value>(&command.stdin).unwrap()["operation"] == "stop" {
            let pid = self.tunnel_pid.lock().unwrap().unwrap();
            *self.tunnel_reaped_before_sync_stop.lock().unwrap() = Some(!Path::new(&format!("/proc/{pid}")).exists());
        }
        self.record("run_sync", command)
    }
}

fn output(code: i32, stdout: &[u8], stderr: &[u8]) -> Output {
    #[cfg(unix)]
    let status = { use std::os::unix::process::ExitStatusExt; ExitStatus::from_raw(code << 8) };
    #[cfg(windows)]
    let status = { use std::os::windows::process::ExitStatusExt; ExitStatus::from_raw(code as u32) };
    Output { status, stdout: stdout.to_vec(), stderr: stderr.to_vec() }
}

fn prepared(valid: bool) -> Output {
    output(0, serde_json::to_string(&json!({"executable": REMOTE_EXE, "valid": valid})).unwrap().as_bytes(), b"")
}

fn success() -> Output { output(0, b"{}", b"") }

fn config(path: &Path) -> AgentConfig {
    AgentConfig {
        transport: Transport::Ssh, command: None, host: Some("vm-a".into()),
        agent_path: Some(path.to_path_buf()), shared_executable: Some("\\\\share\\agent.exe".into()),
        proxy_command: Some("proxy --host %h --port %p".into()), request_timeout: Duration::from_secs(15),
    }
}

fn artifact() -> tempfile::NamedTempFile {
    let artifact = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(artifact.path(), b"agent artifact").unwrap();
    artifact
}

fn connection() -> Connection {
    Connection { url: "http://127.0.0.1:43210/next".into(), token: "secret-not-for-logs".into() }
}

fn arguments(command: &ProcessCommand) -> Vec<String> {
    command.args.iter().map(|a| a.to_str().unwrap().into()).collect()
}

#[test]
fn bootstrap_begins_with_assignment() {
    assert!(BOOTSTRAP.starts_with("$ErrorActionPreference = 'Stop'\n"));
}

#[test]
fn bootstrap_matches_reference_bytes_and_starts_without_agent_subcommand() {
    let digest: String = Sha256::digest(BOOTSTRAP.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(digest, "3d50d74170e36c1377f910aee130e05802caa7f8e31323ca6687646de8845c5d");
    assert!(BOOTSTRAP.contains("-Argument ('--config \"' + $config + '\"')"));
    assert!(!BOOTSTRAP.contains("agent --config"));
}

#[tokio::test]
async fn encoded_command_is_utf16le_base64() {
    let artifact = artifact();
    let runner = FakeRunner::new(vec![prepared(true), success(), success()]);
    let mut transport = SshTransport::start(&config(artifact.path()), &connection(), runner.clone()).await.unwrap();
    let commands = runner.commands();
    let args = arguments(&commands[1]);
    assert_eq!(&args[..11], &["-o", "BatchMode=yes", "-o", "ConnectTimeout=8", "-o",
        "ProxyCommand=proxy --host %h --port %p", "vm-a", "powershell", "-NoProfile",
        "-NonInteractive", "-EncodedCommand"]);
    let decoded = STANDARD.decode(&args[11]).unwrap();
    let expected: Vec<u8> = BOOTSTRAP.encode_utf16().flat_map(u16::to_le_bytes).collect();
    assert_eq!(decoded, expected);
    assert_eq!(commands[1].timeout, Duration::from_secs(60));
    assert_eq!(serde_json::from_slice::<Value>(&commands[1].stdin).unwrap()["operation"], "prepare");
    assert!(!format!("{:?}", commands[2]).contains(&connection().token));
    transport.close(None).await.unwrap();
}

#[tokio::test]
async fn scp_only_when_prepare_says_invalid() {
    let artifact = artifact();
    for valid in [true, false] {
        let mut outputs = vec![prepared(valid)];
        if !valid { outputs.push(success()); }
        outputs.extend([success(), success()]);
        let runner = FakeRunner::new(outputs);
        let mut transport = SshTransport::start(&config(artifact.path()), &connection(), runner.clone()).await.unwrap();
        let commands = runner.commands();
        let scps: Vec<_> = commands.iter().filter(|c| c.program == "scp").collect();
        assert_eq!(scps.len(), usize::from(!valid));
        assert_eq!(arguments(&commands[0]), vec!["-o", "BatchMode=yes", "-o", "ConnectTimeout=8",
            "-o", "ProxyCommand=proxy --host %h --port %p", "-N", "-o", "ExitOnForwardFailure=yes",
            "-R", "127.0.0.1:43210:127.0.0.1:43210", "vm-a"]);
        if let Some(scp) = scps.first() {
            assert_eq!(scp.timeout, Duration::from_secs(120));
            assert!(scp.stdin.is_empty());
            assert_eq!(arguments(scp), vec!["-o".into(), "BatchMode=yes".into(), "-o".into(),
                "ConnectTimeout=8".into(), "-o".into(), "ProxyCommand=proxy --host %h --port %p".into(),
                artifact.path().canonicalize().unwrap().to_str().unwrap().into(),
                format!("vm-a:{}", REMOTE_EXE.replace('\\', "/"))]);
        }
        let payloads = runner.payloads();
        let digest: String = Sha256::digest(b"agent artifact").iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(payloads[0], json!({"operation": "prepare", "digest": digest,
            "shared_executable": "\\\\share\\agent.exe"}));
        assert_eq!(payloads[1]["digest"], digest);
        assert_eq!(payloads[1]["executable"], REMOTE_EXE);
        assert_eq!(payloads[1]["connection"], serde_json::to_value(connection()).unwrap());
        let task = payloads[1]["task"].as_str().unwrap();
        let suffix = task.strip_prefix("HangarComputerControl-").unwrap();
        assert_eq!(suffix.len(), 32);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert!(transport.try_wait().unwrap().is_none());
        transport.close(Some(12345)).await.unwrap();
        let payloads = runner.payloads();
        assert_eq!(payloads[2], json!({"operation": "stop", "task": task,
            "executable": REMOTE_EXE, "pid": 12345}));
        let count = runner.commands().len();
        transport.close(None).await.unwrap();
        transport.close_sync(None).unwrap();
        drop(transport);
        assert_eq!(runner.commands().len(), count);
        assert_reaped(*runner.tunnel_pid.lock().unwrap());
    }
}

#[tokio::test]
async fn cp850_stderr_decoded() {
    let artifact = artifact();
    let runner = FakeRunner::new(vec![output(1, b"", b"Acesso negado: n\xc6o")]);
    let err = SshTransport::start(&config(artifact.path()), &connection(), runner.clone()).await.err().unwrap();
    assert_eq!(err.to_string(), "inicialização Windows falhou: Acesso negado: não");
    assert_reaped(*runner.tunnel_pid.lock().unwrap());
}

#[tokio::test]
async fn utf8_bom_is_stripped_before_json_parsing() {
    let artifact = artifact();
    let mut response = prepared(true);
    response.stdout.splice(0..0, [0xef, 0xbb, 0xbf]);
    let runner = FakeRunner::new(vec![response, success(), success()]);
    let mut transport = SshTransport::start(&config(artifact.path()), &connection(), runner).await.unwrap();
    transport.close(None).await.unwrap();
}

#[tokio::test]
async fn stop_runs_on_close_even_after_start_failure() {
    let artifact = artifact();
    let runner = FakeRunner::new(vec![prepared(true), output(1, b"", b"start failed"), success()]);
    let err = SshTransport::start(&config(artifact.path()), &connection(), runner.clone()).await.err().unwrap();
    assert_eq!(err.to_string(), "inicialização Windows falhou: start failed");
    let payloads = runner.payloads();
    assert_eq!(payloads.len(), 3);
    assert_eq!(payloads[2], json!({"operation": "stop", "task": payloads[1]["task"],
        "executable": REMOTE_EXE, "pid": null}));
    assert_reaped(*runner.tunnel_pid.lock().unwrap());
}

#[tokio::test]
async fn failure_messages_keep_only_the_last_characters_and_cleanup_is_visible() {
    let artifact = artifact();
    let stderr = format!("ignored{}", "á".repeat(1600));
    let runner = FakeRunner::new(vec![output(1, b"", stderr.as_bytes())]);
    let err = SshTransport::start(&config(artifact.path()), &connection(), runner).await.err().unwrap();
    assert_eq!(err.to_string(), format!("inicialização Windows falhou: {}", "á".repeat(1500)));
    let stderr = format!("ignored{}", "ç".repeat(1100));
    let runner = FakeRunner::new(vec![prepared(false), output(1, b"", stderr.as_bytes()), success()]);
    let err = SshTransport::start(&config(artifact.path()), &connection(), runner).await.err().unwrap();
    assert_eq!(err.to_string(), format!("falha ao enviar agente: {}", "ç".repeat(1000)));
    let runner = FakeRunner::new(vec![prepared(true), success(), output(1, b"", b"cleanup failed")]);
    let mut transport = SshTransport::start(&config(artifact.path()), &connection(), runner.clone()).await.unwrap();
    let err = transport.close(None).await.unwrap_err();
    assert_eq!(err, "inicialização Windows falhou: cleanup failed");
    assert_reaped(*runner.tunnel_pid.lock().unwrap());
    transport.close(None).await.unwrap();
}

#[tokio::test]
async fn drop_runs_stop_synchronously_with_twenty_second_limit_and_reaps_tunnel() {
    let artifact = artifact();
    let runner = FakeRunner::new(vec![prepared(true), success(), success()]);
    let transport = SshTransport::start(&config(artifact.path()), &connection(), runner.clone()).await.unwrap();
    drop(transport);
    let calls = runner.calls.lock().unwrap();
    let (kind, command) = calls.last().unwrap();
    assert_eq!(kind, "run_sync");
    assert_eq!(command.timeout, Duration::from_secs(20));
    assert_eq!(serde_json::from_slice::<Value>(&command.stdin).unwrap()["operation"], "stop");
    #[cfg(target_os = "linux")]
    assert_eq!(*runner.tunnel_reaped_before_sync_stop.lock().unwrap(), Some(true));
    assert_reaped(*runner.tunnel_pid.lock().unwrap());
}

#[tokio::test]
async fn missing_host_or_agent_path_is_rejected_before_external_io() {
    let artifact = artifact();
    for host_missing in [true, false] {
        let mut config = config(artifact.path());
        if host_missing { config.host = None; } else { config.agent_path = None; }
        let runner = Arc::new(FakeRunner::default());
        assert!(SshTransport::start(&config, &connection(), runner.clone()).await.is_err());
        assert!(runner.commands().is_empty());
    }
}

fn assert_reaped(pid: Option<u32>) {
    #[cfg(target_os = "linux")]
    assert!(!Path::new(&format!("/proc/{}", pid.unwrap())).exists(), "child is still running or zombie");
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
}

#[cfg(target_os = "linux")]
fn waiting_command(pid_file: &Path, timeout: Duration) -> ProcessCommand {
    ProcessCommand {
        program: "/usr/bin/python3".into(),
        args: vec!["-I".into(), "-c".into(),
            "import os, pathlib, signal, sys; pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); signal.pause()".into(),
            pid_file.as_os_str().to_owned()], stdin: vec![], timeout,
    }
}

#[cfg(target_os = "linux")]
async fn read_pid(path: PathBuf) -> u32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(text) = tokio::fs::read_to_string(&path).await && let Ok(pid) = text.parse() {
                return pid;
            }
            tokio::task::yield_now().await;
        }
    }).await.unwrap()
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_timeout_kills_and_reaps_process() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    let result = NativeRunner.run(waiting_command(&pid_file, Duration::from_millis(500))).await;
    assert!(result.unwrap_err().contains("tempo limite"));
    assert_reaped(Some(std::fs::read_to_string(pid_file).unwrap().parse().unwrap()));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_cancelled_future_kills_and_reaps_process() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    let command = waiting_command(&pid_file, Duration::from_secs(60));
    let handle = tokio::spawn(async move { NativeRunner.run(command).await });
    let pid = read_pid(pid_file).await;
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
    assert_reaped(Some(pid));
}

#[cfg(target_os = "linux")]
#[test]
fn native_sync_timeout_kills_and_reaps_process() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    let err = NativeRunner.run_sync(waiting_command(&pid_file, Duration::from_millis(500))).unwrap_err();
    assert!(err.contains("tempo limite"));
    assert_reaped(Some(std::fs::read_to_string(pid_file).unwrap().parse().unwrap()));
}

#[tokio::test]
async fn no_proxy_command_means_no_extra_ssh_option() {
    let artifact = artifact();
    let mut config = config(artifact.path());
    config.proxy_command = None;
    config.shared_executable = None;
    let runner = FakeRunner::new(vec![prepared(true), success(), success()]);
    let mut transport = SshTransport::start(&config, &connection(), runner.clone()).await.unwrap();
    let command = runner.commands().remove(1);
    assert_eq!(&arguments(&command)[..5], &["-o", "BatchMode=yes", "-o", "ConnectTimeout=8", "vm-a"]);
    assert_eq!(runner.payloads()[0]["shared_executable"], Value::Null);
    transport.close(None).await.unwrap();
}

struct BlockingRunner {
    fake: Arc<FakeRunner>,
    operation: &'static str,
    entered: tokio::sync::Notify,
}

impl Runner for BlockingRunner {
    fn run(&self, command: ProcessCommand) -> RunFuture<'_> {
        Box::pin(async move {
            let payload: Value = serde_json::from_slice(&command.stdin).unwrap();
            let blocked = payload["operation"] == self.operation;
            let result = self.fake.record("run", command);
            if blocked {
                self.entered.notify_one();
                std::future::pending::<()>().await;
            }
            result
        })
    }

    fn spawn(&self, command: ProcessCommand) -> Result<Child, String> { self.fake.spawn(command) }
    fn run_sync(&self, command: ProcessCommand) -> Result<Output, String> { self.fake.run_sync(command) }
}

#[tokio::test]
async fn cancelled_start_stops_remote_task_and_reaps_tunnel() {
    let artifact = artifact();
    let config = config(artifact.path());
    let fake = FakeRunner::new(vec![prepared(true), success(), success()]);
    let runner = Arc::new(BlockingRunner { fake: fake.clone(), operation: "start",
        entered: tokio::sync::Notify::new() });
    let task_runner = runner.clone();
    let task = tokio::spawn(async move { SshTransport::start(&config, &connection(), task_runner).await });
    tokio::time::timeout(Duration::from_secs(5), runner.entered.notified()).await.unwrap();
    task.abort();
    assert!(task.await.err().unwrap().is_cancelled());
    assert_eq!(fake.payloads().last().unwrap()["operation"], "stop");
    assert_eq!(fake.calls.lock().unwrap().last().unwrap().0, "run_sync");
    assert_reaped(*fake.tunnel_pid.lock().unwrap());
}

#[tokio::test]
async fn close_kills_tunnel_before_slow_remote_stop() {
    let artifact = artifact();
    let fake = FakeRunner::new(vec![prepared(true), success(), success(), success()]);
    let runner = Arc::new(BlockingRunner { fake: fake.clone(), operation: "stop",
        entered: tokio::sync::Notify::new() });
    let mut transport = SshTransport::start(&config(artifact.path()), &connection(), runner.clone()).await.unwrap();
    let task = tokio::spawn(async move { transport.close(None).await });
    tokio::time::timeout(Duration::from_secs(5), runner.entered.notified()).await.unwrap();
    let pending = !task.is_finished();
    let pid = *fake.tunnel_pid.lock().unwrap();
    #[cfg(target_os = "linux")]
    let reaped = !Path::new(&format!("/proc/{}", pid.unwrap())).exists();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(pending, "remote stop should still be pending");
    #[cfg(target_os = "linux")]
    assert!(reaped, "tunnel was still alive while remote stop was pending");
    #[cfg(not(target_os = "linux"))]
    assert_reaped(pid);
}

#[tokio::test]
async fn cancelled_close_retries_stop_on_drop_and_reaps_tunnel() {
    let artifact = artifact();
    let fake = FakeRunner::new(vec![prepared(true), success(), success(), success()]);
    let runner = Arc::new(BlockingRunner { fake: fake.clone(), operation: "stop",
        entered: tokio::sync::Notify::new() });
    let mut transport = SshTransport::start(&config(artifact.path()), &connection(), runner.clone()).await.unwrap();
    let task = tokio::spawn(async move { transport.close(None).await });
    tokio::time::timeout(Duration::from_secs(5), runner.entered.notified()).await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(fake.payloads().iter().filter(|p| p["operation"] == "stop").count(), 2);
    assert_eq!(fake.calls.lock().unwrap().last().unwrap().0, "run_sync");
    assert_reaped(*fake.tunnel_pid.lock().unwrap());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_io_is_concurrent_and_local_arguments_are_not_shell_commands() {
    let literal = "quoted value; $(not-a-command) \" ç";
    let command = ProcessCommand {
        program: "/usr/bin/python3".into(),
        args: vec!["-I".into(), "-c".into(),
            "import sys; sys.stdout.write('o' * 131072); sys.stdout.flush(); sys.stderr.write('e' * 131072); sys.stderr.flush(); data = sys.stdin.read(); sys.stdout.write(sys.argv[1]); sys.stderr.write(str(len(data)))".into(),
            literal.into()], stdin: vec![b'i'; 262144], timeout: Duration::from_secs(5),
    };
    let result = NativeRunner.run(command).await.unwrap();
    assert!(result.status.success());
    assert_eq!(&result.stdout[..131072], vec![b'o'; 131072]);
    assert_eq!(&result.stdout[131072..], literal.as_bytes());
    assert_eq!(&result.stderr[..131072], vec![b'e'; 131072]);
    assert_eq!(&result.stderr[131072..], b"262144");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_broken_stdin_pipe_preserves_remote_stderr_and_exit_code() {
    let command = ProcessCommand {
        program: "/usr/bin/python3".into(),
        args: vec!["-I".into(), "-c".into(),
            "import os, sys; os.close(0); sys.stderr.write('access denied'); sys.exit(7)".into()],
        stdin: vec![b'i'; 262144], timeout: Duration::from_secs(5),
    };
    let result = NativeRunner.run(command).await.unwrap();
    assert_eq!(result.status.code(), Some(7));
    assert_eq!(result.stderr, b"access denied");
}

#[tokio::test]
async fn cleanup_failure_is_written_to_stderr_without_stdin_secrets() {
    const MARKER: &str = "HCC_SSH_CLEANUP_STDERR_TEST";
    if std::env::var_os(MARKER).is_some() {
        let artifact = artifact();
        let runner = FakeRunner::new(vec![prepared(true), success(), output(1, b"", b"cleanup failed")]);
        let transport = SshTransport::start(&config(artifact.path()), &connection(), runner).await.unwrap();
        drop(transport);
        return;
    }
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "cleanup_failure_is_written_to_stderr_without_stdin_secrets", "--nocapture"])
        .env(MARKER, "1").stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true)
        .spawn().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let io = async {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        let (_, status) = tokio::try_join!(stderr.read_to_end(&mut bytes), child.wait()).unwrap();
        (bytes, status)
    };
    let (bytes, status) = tokio::time::timeout(Duration::from_secs(5), io).await.unwrap();
    assert!(status.success());
    let stderr = String::from_utf8(bytes).unwrap();
    assert!(stderr.contains("não foi possível remover tarefa HangarComputerControl-"));
    assert!(stderr.contains("inicialização Windows falhou: cleanup failed"));
    assert!(!stderr.contains(&connection().token));
}

#[test]
fn process_command_debug_hides_stdin() {
    let command = ProcessCommand { program: OsString::from("ssh"), args: vec![],
        stdin: connection().token.into_bytes(), timeout: Duration::from_secs(60) };
    assert!(!format!("{command:?}").contains("secret-not-for-logs"));
}
