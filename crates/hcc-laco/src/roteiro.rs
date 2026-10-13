//! Scripts: the actions of a confirmed goal, replayed by selector without asking Jev at each step.

use std::path::{Path, PathBuf};

use hcc_protocolo::{Action, ActionType, Observation};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::candidatos::py_str;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Janela {
    pub classe: String,
    pub titulo: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Alvo {
    Janela(Janela),
    /// `ordem` tells apart controls with the same role and name (VCL edits often have no name).
    Elemento { role: String, name: String, ordem: usize },
}

/// A value from `dados` travels by key: the file never holds it, and the next run may pass another one.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Valor {
    Dados(String),
    Literal(String),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Passo {
    pub janela: Janela,
    pub alvo: Option<Alvo>,
    /// Without `target` and `value`; both are resolved again on replay.
    pub acao: Action,
    pub valor: Option<Valor>,
    pub mudou_tela: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Roteiro {
    pub objetivo: String,
    pub passos: Vec<Passo>,
}

fn normal(texto: &str) -> String {
    texto.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Same target, same goal text and same `dados` keys: values may change, the steps do not.
pub fn arquivo(pasta: &Path, config: Option<&Path>, texto: &str, dados: &Map<String, Value>) -> PathBuf {
    let mut chaves: Vec<&String> = dados.keys().collect();
    chaves.sort();
    let semente = json!([config.map(|c| c.to_string_lossy()), normal(texto), chaves]).to_string();
    let hash: String = Sha256::digest(semente.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect();
    pasta.join(format!("{hash}.json"))
}

fn janela(obs: &Observation) -> Janela {
    let w = obs.windows.iter().find(|w| w.id == obs.foreground);
    Janela {
        classe: w.map(|w| w.class_name.clone()).unwrap_or_default(),
        titulo: w.map(|w| w.name.clone()).unwrap_or_default(),
    }
}

pub fn passo(acao: &Action, obs: &Observation, dados: &Map<String, Value>) -> Passo {
    let alvo = acao.target.as_deref().and_then(|id| {
        if let Some(i) = obs.elements.iter().position(|e| e.id == id) {
            let e = &obs.elements[i];
            let ordem = obs.elements[..i].iter().filter(|o| o.role == e.role && o.name == e.name).count();
            return Some(Alvo::Elemento { role: e.role.clone(), name: e.name.clone(), ordem });
        }
        obs.windows.iter().find(|w| w.id == id)
            .map(|w| Alvo::Janela(Janela { classe: w.class_name.clone(), titulo: w.name.clone() }))
    });
    let valor = acao.value.as_ref().map(|v| {
        dados.iter().find(|(_, d)| py_str(d) == *v).map_or_else(|| Valor::Literal(v.clone()), |(k, _)| Valor::Dados(k.clone()))
    });
    Passo { janela: janela(obs), alvo, acao: Action { target: None, value: None, ..acao.clone() }, valor, mudou_tela: None }
}

/// The action for this screen, or `None` when the screen diverged from the recording.
pub fn resolver(passo: &Passo, obs: &Observation, dados: &Map<String, Value>) -> Option<Action> {
    let agora = janela(obs);
    // A click by coordinate cannot tell whether the same thing is still under it: Jev decides it again.
    if passo.acao.kind == ActionType::Mouse { return None; }
    let mesma_janela = agora.classe == passo.janela.classe && agora.titulo == passo.janela.titulo;
    // Launching an app does not depend on the window in front.
    if passo.acao.kind != ActionType::Launch && !mesma_janela {
        return None;
    }
    let target = match &passo.alvo {
        None => None,
        Some(Alvo::Elemento { role, name, ordem }) => {
            Some(obs.elements.iter().filter(|e| &e.role == role && &e.name == name).nth(*ordem)?.id.clone())
        }
        Some(Alvo::Janela(j)) => Some(obs.windows.iter().find(|w| w.class_name == j.classe && w.name == j.titulo)?.id.clone()),
    };
    let value = match &passo.valor {
        None => None,
        Some(Valor::Dados(k)) => Some(py_str(dados.get(k)?)),
        Some(Valor::Literal(v)) => Some(v.clone()),
    };
    Some(Action { target, value, ..passo.acao.clone() })
}

/// A launch whose app (the window class of the next step) is already open would start a second instance.
pub fn ja_aberto(passos: &[Passo], n: usize, obs: &Observation) -> bool {
    passos[n].acao.kind == ActionType::Launch && passos.get(n + 1).is_some_and(|p| {
        !p.janela.classe.is_empty() && obs.windows.iter().any(|w| w.class_name == p.janela.classe)
    })
}

/// `Ok(None)` only when there is no script yet; anything else unreadable is an error the result shows.
pub fn carregar(arquivo: &Path) -> Result<Option<Roteiro>, String> {
    match std::fs::read(arquivo) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| e.to_string()),
    }
}

pub fn salvar(arquivo: &Path, roteiro: &Roteiro) -> std::io::Result<()> {
    if let Some(pasta) = arquivo.parent() { std::fs::create_dir_all(pasta)?; }
    let texto = serde_json::to_string_pretty(roteiro).map_err(std::io::Error::other)?;
    // Write then rename, so an interrupted save never leaves half a script behind.
    let temporario = arquivo.with_extension("json.tmp");
    std::fs::write(&temporario, texto)?;
    std::fs::rename(temporario, arquivo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tela(classe: &str) -> Observation {
        serde_json::from_value(json!({
            "observation_id": "o", "connected": true, "session_id": 1, "foreground": "w1",
            "windows": [{"id": "w1", "name": classe, "process_id": 1, "class_name": classe, "rect": [0,0,10,10]}],
            "elements": [], "truncated": false, "timestamp": 1.0, "screen": {"width": 10, "height": 10},
        })).unwrap()
    }

    #[test]
    fn launch_runs_whatever_window_is_in_front() {
        let abrir = Action { kind: ActionType::Launch, application: Some("gnome-calculator".into()), ..Default::default() };
        let gravado = passo(&abrir, &tela("Hangar"), &Map::new());
        assert_eq!(resolver(&gravado, &tela("Desktop"), &Map::new()), Some(abrir));
        let enter = Action { kind: ActionType::Keys, keys: Some(vec!["Enter".into()]), ..Default::default() };
        assert_eq!(resolver(&passo(&enter, &tela("Hangar"), &Map::new()), &tela("Desktop"), &Map::new()), None);
    }

    #[test]
    fn mouse_click_is_never_replayed() {
        let clique = Action { kind: ActionType::Mouse, x: Some(1), y: Some(1), button: Some(hcc_protocolo::Button::Left),
            mode: Some(hcc_protocolo::MouseMode::Click), ..Default::default() };
        assert_eq!(resolver(&passo(&clique, &tela("TMS"), &Map::new()), &tela("TMS"), &Map::new()), None);
    }

    #[test]
    fn launch_is_skipped_when_the_app_it_opened_is_already_open() {
        let abrir = Action { kind: ActionType::Launch, application: Some("gnome-calculator".into()), ..Default::default() };
        let enter = Action { kind: ActionType::Keys, keys: Some(vec!["Enter".into()]), ..Default::default() };
        let passos = vec![passo(&abrir, &tela("Hangar"), &Map::new()), passo(&enter, &tela("Calculator"), &Map::new())];
        assert!(ja_aberto(&passos, 0, &tela("Calculator")));
        assert!(!ja_aberto(&passos, 0, &tela("Hangar")));
        assert!(!ja_aberto(&passos, 1, &tela("Calculator")));
    }
}
