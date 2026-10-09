#![cfg(feature = "test-fake")]

use hcc_controlador::{AgentConfig, AgentConnector, AgentSession};
use hcc_protocolo::{Action, Connector, Session, SessionError};
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn config(mode: &str, extra: &[String]) -> AgentConfig {
    let mut command = vec![env!("CARGO_BIN_EXE_fake-agent").into(), mode.into()];
    command.extend_from_slice(extra);
    AgentConfig { command: Some(command), request_timeout: Duration::from_millis(150), ..AgentConfig::default() }
}

async fn session(mode: &str, cancel: &CancellationToken) -> AgentSession {
    AgentSession::start(config(mode, &[]), &[], cancel.clone()).await.unwrap()
}

#[tokio::test]
async fn roundtrip_observe_act() {
    let cancel = CancellationToken::new();
    let mut session = session("normal", &cancel).await;
    let obs = session.observe().await.unwrap();
    assert_eq!(obs.observation_id, "observation-a");
    assert_eq!(obs.elements[0].name, "ação");
    let action: Action = serde_json::from_value(json!({"type":"text","value":"ação ç"})).unwrap();
    assert!(session.act(&obs.observation_id, &action).await.unwrap().ok);
    assert_eq!(session.screenshot().await.unwrap(), b"png");
    session.close().await.unwrap();
    assert!(session.is_closed());
}

#[tokio::test]
async fn rpc_timeout_closes_session_and_never_returns_cached() {
    let cancel = CancellationToken::new();
    let mut session = session("exit-on-screenshot", &cancel).await;
    session.observe().await.unwrap();
    let error = session.screenshot().await.unwrap_err();
    assert!(matches!(error, SessionError::Timeout(_)));
    assert_eq!(error.to_string(), "agente não respondeu a screenshot; execução encerrada");
    assert!(session.is_closed());
    assert!(matches!(session.observe().await, Err(SessionError::Cancelled(_))));
}

#[tokio::test]
async fn cancel_prevents_sending_action() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("act.marker");
    let cancel = CancellationToken::new();
    let mut session = AgentSession::start(config("record-act", &[marker.display().to_string()]), &[], cancel.clone()).await.unwrap();
    let obs = session.observe().await.unwrap();
    cancel.cancel();
    let action = serde_json::from_value(json!({"type":"text","value":"não enviar"})).unwrap();
    let error = session.act(&obs.observation_id, &action).await.unwrap_err();
    assert_eq!(error, SessionError::Cancelled("controle cancelado ou encerrado".into()));
    session.close().await.unwrap();
    assert!(!marker.exists());
}

#[tokio::test]
async fn agent_crash_reason_reaches_caller() {
    let error = match AgentSession::start(config("crash", &[]), &[], CancellationToken::new()).await {
        Ok(_) => panic!("startup should fail"),
        Err(error) => error,
    };
    assert!(matches!(error, SessionError::Startup(_)));
    assert_eq!(error.to_string(), "processo de conexão encerrou com código 1: boom");
}

#[tokio::test]
async fn local_child_is_reaped_on_close_and_drop() {
    for explicit in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("pid");
        let mut session = AgentSession::start(config("record-pid", &[pid_path.display().to_string()]), &[], CancellationToken::new()).await.unwrap();
        let pid = std::fs::read_to_string(&pid_path).unwrap();
        if explicit { session.close().await.unwrap(); }
        drop(session);
        #[cfg(unix)]
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "child {pid} still alive");
        #[cfg(not(unix))]
        assert!(!pid.trim().is_empty());
    }
}

#[tokio::test]
async fn agent_error_keeps_python_exception_name() {
    let mut session = session("agent-error", &CancellationToken::new()).await;
    let error = session.observe().await.unwrap_err();
    assert_eq!(error, SessionError::Agent("ValueError: observação inválida".into()));
    session.close().await.unwrap();
}

#[tokio::test]
async fn cancellation_during_startup_reaps_child() {
    let directory = tempfile::tempdir().unwrap();
    let pid_path = directory.path().join("pid");
    let cancel = CancellationToken::new();
    let pending = tokio::spawn(AgentSession::start(
        config("no-hello", &[pid_path.display().to_string()]), &[], cancel.clone()));
    let pid = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match tokio::fs::read_to_string(&pid_path).await {
                Ok(pid) if !pid.is_empty() => break pid,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("{error}"),
            }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    cancel.cancel();
    let error = match pending.await.unwrap() {
        Ok(_) => panic!("startup should fail"),
        Err(error) => error,
    };
    assert_eq!(error, SessionError::Cancelled("controle cancelado ou encerrado".into()));
    #[cfg(unix)]
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    #[cfg(not(unix))]
    assert!(!pid.is_empty());
}

#[tokio::test]
async fn connector_loads_config_at_connect_and_uses_default_command() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.json");
    let connector = AgentConnector { config_path: Some(path.clone()), default_command: vec![env!("CARGO_BIN_EXE_fake-agent").into(), "normal".into()] };
    std::fs::write(&path, "{").unwrap();
    let error = match connector.connect(&CancellationToken::new()).await {
        Ok(_) => panic!("bad config should fail"),
        Err(error) => error,
    };
    assert!(matches!(error, SessionError::Config(_)));
    assert!(error.to_string().contains("agent.json"));
    std::fs::write(&path, "{}").unwrap();
    let mut session = connector.connect(&CancellationToken::new()).await.unwrap();
    assert!(session.observe().await.unwrap().connected);
    session.close().await.unwrap();
}

#[test]
fn config_defaults_unknown_fields_and_invalid_input() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.json");
    std::fs::write(&path, r#"{"unknown":true}"#).unwrap();
    let config = AgentConfig::load(&path).unwrap();
    assert_eq!(config.request_timeout, Duration::from_secs(15));
    std::fs::write(&path, r#"{"transport":"other"}"#).unwrap();
    assert_eq!(AgentConfig::load(&path).unwrap_err(), "transport deve ser local ou ssh");
    for timeout in ["-1", "1e100", "null", "\"2\""] {
        std::fs::write(&path, format!("{{\"request_timeout\":{timeout}}}")).unwrap();
        assert!(AgentConfig::load(&path).is_err());
    }
}

async fn paused_session(directory: &Path, cancel: &CancellationToken) -> AgentSession {
    let mut session = AgentSession::start(config("pause-polls", &[directory.display().to_string()]), &[], cancel.clone()).await.unwrap();
    session.observe().await.unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        while !tokio::fs::try_exists(directory.join("paused")).await.unwrap() {
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    session
}

async fn poll_pending<T>(future: &mut std::pin::Pin<Box<impl std::future::Future<Output = T>>>) {
    std::future::poll_fn(|context| {
        assert!(future.as_mut().poll(context).is_pending());
        std::task::Poll::Ready(())
    }).await;
}

#[tokio::test]
async fn abandoned_rpc_does_not_deliver_action() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = paused_session(directory.path(), &CancellationToken::new()).await;
    let action: Action = serde_json::from_value(json!({"type":"text","value":"não executar"})).unwrap();
    let mut pending = Box::pin(session.act("observation-a", &action));
    poll_pending(&mut pending).await;
    drop(pending);
    tokio::fs::write(directory.path().join("release"), b"resume").await.unwrap();
    session.observe().await.unwrap();
    session.close().await.unwrap();
    assert!(!directory.path().join("act.marker").exists(), "abandoned action reached agent");
}

async fn full_queue_case(cancelled: bool) {
    let directory = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let mut session = paused_session(directory.path(), &cancel).await;
    let action: Action = serde_json::from_value(json!({"type":"text","value":"não executar"})).unwrap();
    for _ in 0..64 {
        let mut pending = Box::pin(session.act("observation-a", &action));
        poll_pending(&mut pending).await;
        drop(pending);
    }
    let mut pending = Box::pin(session.act("observation-a", &action));
    poll_pending(&mut pending).await;
    if cancelled { cancel.cancel(); }
    let error = tokio::time::timeout(Duration::from_secs(1), pending).await
        .expect("RPC blocked outside its timeout/cancellation").unwrap_err();
    if cancelled {
        assert!(matches!(error, SessionError::Cancelled(_)));
        session.close().await.unwrap();
    } else {
        assert_eq!(error, SessionError::Timeout("agente não respondeu a act; execução encerrada".into()));
        assert!(session.is_closed());
    }
}

#[tokio::test]
async fn full_queue_obeys_rpc_timeout() { full_queue_case(false).await; }

#[tokio::test]
async fn full_queue_obeys_cancellation() { full_queue_case(true).await; }

#[cfg(unix)]
#[tokio::test]
async fn agent_killed_by_signal_reports_negative_code() {
    let directory = tempfile::tempdir().unwrap();
    let pid_path = directory.path().join("pid");
    let pending = tokio::spawn(AgentSession::start(
        config("no-hello", &[pid_path.display().to_string()]), &[], CancellationToken::new()));
    let pid = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match tokio::fs::read_to_string(&pid_path).await {
                Ok(pid) if !pid.is_empty() => break pid,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("{error}"),
            }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let pid: u32 = pid.parse().unwrap();
    assert!(std::process::Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap().success());
    let error = match pending.await.unwrap() {
        Ok(_) => panic!("startup should fail"),
        Err(error) => error,
    };
    assert_eq!(error, SessionError::Startup("processo de conexão encerrou com código -9".into()));
}
