//! Agent runtime: long-poll client loop shared by the Linux and Windows agents.

pub mod cliente;
#[cfg(feature = "fake")]
pub mod fake;
pub mod registro;

use base64::Engine as _;
use cliente::Cliente;
use hcc_protocolo::{ActResult, AgentPost, Command, Connection, Desktop, DesktopError, ErrorKind, PROTOCOL, ScreenshotResult};
use registro::Registro;
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

const OPERACOES: [&str; 5] = ["heartbeat", "stop", "observe", "act", "screenshot"];

/// Reads the connection file (BOM tolerated), deletes it and serves commands until `stop`.
/// On error: message to stderr and to `<config>.error.log`, returns 1.
pub fn run<D: Desktop>(desktop: D, config: &Path) -> i32 {
    match servir(desktop, config) {
        Ok(()) => 0,
        Err(msg) => {
            eprintln!("{msg}");
            let log = config.with_extension("error.log");
            if let Err(e) = std::fs::write(&log, &msg) {
                eprintln!("{}: {e}", log.display());
            }
            1
        }
    }
}

fn ler_conexao(config: &Path) -> Result<Connection, String> {
    let erro = |e: &dyn std::fmt::Display| format!("{}: {e}", config.display());
    let texto = std::fs::read_to_string(config).map_err(|e| erro(&e))?;
    // PowerShell's Set-Content -Encoding UTF8 writes a BOM.
    let conexao = serde_json::from_str(texto.strip_prefix('\u{feff}').unwrap_or(&texto)).map_err(|e| erro(&e))?;
    // The start credential must not stay on disk once loaded.
    std::fs::remove_file(config).map_err(|e| erro(&e))?;
    Ok(conexao)
}

fn servir<D: Desktop>(mut desktop: D, config: &Path) -> Result<(), String> {
    let cliente = Cliente::new(&ler_conexao(config)?)?;
    let base = AgentPost {
        hello: false,
        boot: uuid::Uuid::new_v4().simple().to_string(),
        session_id: desktop.session_id(),
        pid: std::process::id(),
        protocol: None,
        id: None,
        result: None,
        error: None,
    };
    let mut resposta = AgentPost { hello: true, protocol: Some(PROTOCOL), ..base.clone() };
    let mut registro = Registro::default();
    loop {
        let bruto = cliente.enviar(&resposta)?;
        let recebido = Instant::now();
        resposta = base.clone();
        let (id, resultado) = match serde_json::from_value::<Command>(bruto.clone()) {
            Ok(Command::Stop) => return Ok(()),
            Ok(Command::Heartbeat) => continue,
            Ok(Command::Observe { id, ttl_ms }) => {
                let r = no_prazo(recebido, ttl_ms)
                    .and_then(|()| desktop.available())
                    .and_then(|()| registro.observar(&mut desktop))
                    .and_then(|o| para_json(&o));
                (id, r)
            }
            Ok(Command::Act { id, ttl_ms, action, observation_id }) => {
                let r = no_prazo(recebido, ttl_ms)
                    .and_then(|()| desktop.available())
                    .and_then(|()| registro.validar_e_consumir(&mut desktop, &observation_id))
                    .and_then(|observada| desktop.act(&observada, &action))
                    .and_then(|()| para_json(&ActResult { ok: true }));
                (id, r)
            }
            Ok(Command::Screenshot { id, ttl_ms }) => {
                let r = no_prazo(recebido, ttl_ms)
                    .and_then(|()| desktop.available())
                    .and_then(|()| desktop.screenshot_png())
                    .and_then(|png| para_json(&ScreenshotResult { png: base64::engine::general_purpose::STANDARD.encode(png) }));
                (id, r)
            }
            Err(e) => {
                // Missing or unknown operation is what Python's dispatch rejects (windows_uia.py:380).
                let operacao = bruto.get("operation").and_then(Value::as_str).unwrap_or_default();
                // Only the operation: the body may carry typed text (a password).
                let id = bruto.get("id").and_then(Value::as_str).ok_or_else(|| format!("comando sem id do controlador: operation={operacao:?}"))?;
                let msg = if OPERACOES.contains(&operacao) { format!("comando inválido: {e}") } else { "operação inválida".into() };
                let invalido = DesktopError { kind: ErrorKind::ValueError, msg };
                let r = if operacao == "act" {
                    // Python validates and spends the observation before rejecting the action (windows_uia.py:244-248).
                    let observation_id = bruto.get("observation_id").and_then(Value::as_str).unwrap_or_default();
                    desktop.available().and_then(|()| registro.validar_e_consumir(&mut desktop, observation_id)).and(Err(invalido))
                } else {
                    Err(invalido)
                };
                (id.to_owned(), r)
            }
        };
        resposta.id = Some(id);
        match resultado {
            Ok(v) => resposta.result = Some(v),
            Err(e) => resposta.error = Some(e.to_string()),
        }
    }
}

/// Relative to receipt on the agent's own clock: no skew between controller and desktop.
fn no_prazo(recebido: Instant, ttl_ms: u64) -> Result<(), DesktopError> {
    if recebido.elapsed() >= Duration::from_millis(ttl_ms) {
        return Err(DesktopError { kind: ErrorKind::TimeoutError, msg: "comando expirou antes de chegar ao desktop".into() });
    }
    Ok(())
}

fn para_json(v: &impl serde::Serialize) -> Result<Value, DesktopError> {
    serde_json::to_value(v).map_err(|e| DesktopError { kind: ErrorKind::ValueError, msg: e.to_string() })
}
