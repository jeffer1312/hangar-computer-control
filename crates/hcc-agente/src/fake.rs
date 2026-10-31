//! Scripted desktop for tests: no real screen, mouse or keyboard is touched.

use hcc_protocolo::{Action, Delivery, Desktop, DesktopError, ErrorKind, Observation, Scope};
use serde::Deserialize;
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Smallest valid PNG: 1×1 RGBA transparent.
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
    0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63,
    0x60, 0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x05, 0x00, 0x01, 0x7A, 0xA8, 0x57, 0x50, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44,
    0xAE, 0x42, 0x60, 0x82,
];

#[derive(Deserialize)]
struct Script {
    observations: Vec<Observation>,
    #[serde(default)]
    fail_screenshot: bool,
    /// One per act, in order; once used up, acts succeed as `delivered_unverified`.
    #[serde(default)]
    act_results: Vec<ActResultado>,
}

#[derive(Deserialize)]
struct ActResultado {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    delivery: Delivery,
}

pub struct FakeDesktop {
    script: Script,
    proxima: usize,
    acoes: PathBuf,
    chamadas: PathBuf,
}

impl FakeDesktop {
    /// Done acts are appended to `<script>.acts.jsonl`; observe and screenshot calls to `<script>.calls.jsonl`.
    pub fn load(script: &Path) -> Result<Self, String> {
        let texto = std::fs::read_to_string(script).map_err(|e| format!("{}: {e}", script.display()))?;
        let mut script_lido: Script = serde_json::from_str(&texto).map_err(|e| format!("{}: {e}", script.display()))?;
        script_lido.act_results.reverse();
        Ok(Self {
            script: script_lido,
            proxima: 0,
            acoes: script.with_extension("acts.jsonl"),
            chamadas: script.with_extension("calls.jsonl"),
        })
    }

    fn anotar(caminho: &Path, linha: serde_json::Value) -> Result<(), DesktopError> {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(caminho)
            .and_then(|mut f| writeln!(f, "{linha}"))
            .map_err(|e| Self::erro(format!("{}: {e}", caminho.display())))
    }

    /// Script path from `HCC_FAKE_DESKTOP`.
    pub fn from_env() -> Result<Self, String> {
        let caminho = std::env::var_os("HCC_FAKE_DESKTOP").ok_or("HCC_FAKE_DESKTOP não definido")?;
        Self::load(Path::new(&caminho))
    }

    fn corrente(&self) -> Option<&Observation> {
        let obs = &self.script.observations;
        obs.get(self.proxima.saturating_sub(1).min(obs.len().saturating_sub(1)))
    }

    fn erro(msg: String) -> DesktopError {
        DesktopError::new(ErrorKind::RuntimeError, msg)
    }
}

impl Desktop for FakeDesktop {
    fn session_id(&self) -> u32 {
        self.script.observations.first().map_or(1, |o| o.session_id)
    }

    fn available(&mut self) -> Result<(), DesktopError> {
        Ok(())
    }

    fn foreground(&mut self) -> Result<String, DesktopError> {
        Ok(self.corrente().map_or_else(|| "desktop".into(), |o| o.foreground.clone()))
    }

    fn has_window(&mut self, _: &str) -> Result<bool, DesktopError> {
        Ok(true)
    }

    /// In order; the last one repeats. `scope.window` becomes the foreground.
    fn observe(&mut self, scope: &Scope) -> Result<Observation, DesktopError> {
        Self::anotar(&self.chamadas, json!({"observe": scope}))?;
        if self.script.observations.is_empty() {
            return Err(Self::erro("roteiro sem observações".into()));
        }
        self.proxima = (self.proxima + 1).min(self.script.observations.len());
        let mut o = self.corrente().cloned().expect("roteiro não vazio");
        if let Some(janela) = &scope.window {
            o.foreground.clone_from(janela);
        }
        Ok(o)
    }

    fn act(&mut self, observed: &Observation, action: &Action) -> Result<Delivery, DesktopError> {
        let resultado = self.script.act_results.pop().unwrap_or(ActResultado { error: None, delivery: Delivery::DeliveredUnverified });
        if let Some(msg) = resultado.error {
            return Err(DesktopError { delivery: resultado.delivery, ..Self::erro(msg) });
        }
        Self::anotar(&self.acoes, json!({"observation_id": observed.observation_id, "action": action}))?;
        Ok(resultado.delivery)
    }

    fn screenshot_png(&mut self, window: Option<&str>) -> Result<Vec<u8>, DesktopError> {
        Self::anotar(&self.chamadas, json!({"screenshot": window}))?;
        if self.script.fail_screenshot {
            // Simulates the agent process dying mid-objective.
            std::process::exit(3);
        }
        Ok(PNG_1X1.to_vec())
    }
}
