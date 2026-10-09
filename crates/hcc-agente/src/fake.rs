//! Scripted desktop for tests: no real screen, mouse or keyboard is touched.

use hcc_protocolo::{Action, Desktop, DesktopError, Observation};
use serde::Deserialize;
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
}

pub struct FakeDesktop {
    script: Script,
    proxima: usize,
    acoes: PathBuf,
}

impl FakeDesktop {
    /// Acts are appended to `<script>.acts.jsonl`.
    pub fn load(script: &Path) -> Result<Self, String> {
        let texto = std::fs::read_to_string(script).map_err(|e| format!("{}: {e}", script.display()))?;
        let script_lido = serde_json::from_str(&texto).map_err(|e| format!("{}: {e}", script.display()))?;
        Ok(Self { script: script_lido, proxima: 0, acoes: script.with_extension("acts.jsonl") })
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
        DesktopError { kind: hcc_protocolo::ErrorKind::RuntimeError, msg }
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

    /// In order; the last one repeats.
    fn observe(&mut self) -> Result<Observation, DesktopError> {
        if self.script.observations.is_empty() {
            return Err(Self::erro("roteiro sem observações".into()));
        }
        self.proxima = (self.proxima + 1).min(self.script.observations.len());
        Ok(self.corrente().cloned().expect("roteiro não vazio"))
    }

    fn act(&mut self, observed: &Observation, action: &Action) -> Result<(), DesktopError> {
        let linha = serde_json::json!({"observation_id": observed.observation_id, "action": action});
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.acoes)
            .and_then(|mut f| writeln!(f, "{linha}"))
            .map_err(|e| Self::erro(format!("{}: {e}", self.acoes.display())))
    }

    fn screenshot_png(&mut self) -> Result<Vec<u8>, DesktopError> {
        if self.script.fail_screenshot {
            // Simulates the agent process dying mid-objective.
            std::process::exit(3);
        }
        Ok(PNG_1X1.to_vec())
    }
}
