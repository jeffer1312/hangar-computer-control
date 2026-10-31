//! Which input mode each action goes with, and the actions each app showed need real input.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use hcc_protocolo::{Action, InputMode, Observation};
use serde::{Deserialize, Serialize};

/// In the scripts folder; one line per app: `{"app.exe": ["check:CheckBox"]}`.
pub const ARQUIVO: &str = "entrada-real.json";

#[derive(Default, Serialize, Deserialize)]
pub struct Aprendido {
    #[serde(default)]
    versao: u32,
    #[serde(default)]
    apps: BTreeMap<String, BTreeSet<String>>,
}

/// The app is its executable (Windows) or Hyprland class; without one, the window class.
fn app(obs: &Observation) -> String {
    obs.windows.iter().find(|w| w.id == obs.foreground)
        .map(|w| if w.process.is_empty() { w.class_name.clone() } else { w.process.clone() }).unwrap_or_default()
}

/// `check:CheckBox`: the action type on that role.
fn chave(acao: &Action, obs: &Observation) -> Option<String> {
    let alvo = obs.elements.iter().find(|e| Some(&e.id) == acao.target.as_ref())?;
    Some(format!("{}:{}", acao.kind, alvo.role))
}

impl Aprendido {
    /// An unreadable file teaches nothing: the actions go background and learn again.
    pub fn ler(pasta: Option<&Path>) -> Self {
        pasta.and_then(|p| std::fs::read_to_string(p.join(ARQUIVO)).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn gravar(&self, pasta: &Path) -> Result<(), String> {
        let texto = serde_json::to_string_pretty(&serde_json::json!({"versao": 1, "apps": &self.apps})).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(pasta).and_then(|()| std::fs::write(pasta.join(ARQUIVO), texto)).map_err(|e| format!("{}: {e}", pasta.display()))
    }

    /// True when it is new.
    pub fn aprender(&mut self, obs: &Observation, acao: &Action) -> bool {
        let (app, Some(chave)) = (app(obs), chave(acao, obs)) else { return false };
        !app.is_empty() && self.apps.entry(app).or_default().insert(chave)
    }

    fn precisa(&self, obs: &Observation, acao: &Action) -> bool {
        chave(acao, obs).is_some_and(|c| self.apps.get(&app(obs)).is_some_and(|s| s.contains(&c)))
    }
}

/// Watching (`ver`): real input as always. Otherwise an action on a control goes through accessibility, unless the
/// app showed it does not work there; everything else needs real input and waits for the user to be idle.
pub fn modo_entrada(acao: &Action, obs: &Observation, aprendido: &Aprendido, ver: bool) -> Option<InputMode> {
    if ver { return None; }
    Some(if acao.kind.sobre_controle() && acao.target.is_some() && !aprendido.precisa(obs, acao) { InputMode::Background } else { InputMode::Polite })
}
