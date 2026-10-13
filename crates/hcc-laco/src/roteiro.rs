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
    /// `text`, `placeholder` and `test_id` exist for the browser recorder sharing this format; UIA leaves them empty.
    Elemento {
        role: String,
        name: String,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        placeholder: Option<String>,
        #[serde(default)]
        test_id: Option<String>,
        ordem: Ordem,
    },
}

/// Which of the `de` twins the step acted on; another count of twins on replay means the screen changed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Ordem {
    pub indice: usize,
    pub de: usize,
}

/// Format shared with the browser recorder in the hangar repo; bump both when it changes.
pub const VERSAO: u32 = 2;

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

/// One way the goal was done; installations differ (app version in a title, another build), so a file keeps several.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Variante {
    pub passos: Vec<Passo>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Roteiro {
    pub versao: u32,
    pub objetivo: String,
    /// Latest success first.
    pub variantes: Vec<Variante>,
}

const MAX_VARIANTES: usize = 5;

/// On disk: version 1 had a single `passos` list.
#[derive(Deserialize)]
struct Lido {
    versao: u32,
    objetivo: String,
    #[serde(default)]
    passos: Option<Vec<Passo>>,
    #[serde(default)]
    variantes: Vec<Variante>,
}

fn normal(texto: &str) -> String {
    texto.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Same goal text and same `dados` keys: values may change, the steps do not. No machine or target in the key,
/// so a script committed to the repository serves whoever runs the same goal on the same app.
pub fn arquivo(pasta: &Path, texto: &str, dados: &Map<String, Value>) -> PathBuf {
    let mut chaves: Vec<&String> = dados.keys().collect();
    chaves.sort();
    let semente = json!([normal(texto), chaves]).to_string();
    let hash: String = Sha256::digest(semente.as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect();
    // The browser recorder writes to the same folder: the prefix keeps the two apart.
    let slug = Some(slug(texto)).filter(|s| !s.is_empty()).unwrap_or_else(|| "roteiro".to_owned());
    pasta.join(format!("desktop-{slug}-{hash}.json"))
}

fn slug(texto: &str) -> String {
    let mut s = String::new();
    for c in texto.to_lowercase().chars() {
        let c = match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            c => c,
        };
        if c.is_ascii_alphanumeric() { s.push(c); } else if !s.ends_with('-') { s.push('-'); }
    }
    let s = s.trim_matches('-');
    s[..s.len().min(60)].trim_end_matches('-').to_owned()
}

/// `.hangar/roteiros` at the root of the git repository holding `cwd`, so scripts travel with the project; `cache` outside one.
pub fn pasta(cwd: &Path, cache: &Path) -> PathBuf {
    cwd.ancestors().find(|d| d.join(".git").exists())
        .map_or_else(|| cache.to_path_buf(), |raiz| raiz.join(".hangar/roteiros"))
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
            let gemeo = |o: &&hcc_protocolo::Element| o.role == e.role && o.name == e.name;
            let ordem = Ordem { indice: obs.elements[..i].iter().filter(gemeo).count(), de: obs.elements.iter().filter(gemeo).count() };
            return Some(Alvo::Elemento {
                role: e.role.clone(), name: e.name.clone(), text: None, placeholder: None, test_id: None, ordem,
            });
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
        Some(Alvo::Elemento { role, name, ordem, .. }) => {
            let gemeos: Vec<_> = obs.elements.iter().filter(|e| &e.role == role && &e.name == name).collect();
            if gemeos.len() != ordem.de { return None; }
            Some(gemeos.get(ordem.indice)?.id.clone())
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

/// What a save merges with. A corrupt file is replaced; one of another format version may belong to a newer
/// program or to the browser recorder, so it is never overwritten.
pub fn anteriores(arquivo: &Path) -> Result<Option<Roteiro>, String> {
    match carregar(arquivo) {
        Err(e) if e.starts_with("versão ") => Err(format!("arquivo em {e}")),
        Err(_) => Ok(None),
        lido => lido,
    }
}

/// `Ok(None)` only when there is no script yet; anything else unreadable is an error the result shows.
pub fn carregar(arquivo: &Path) -> Result<Option<Roteiro>, String> {
    match std::fs::read(arquivo) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
        Ok(bytes) => {
            let lido: Lido = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            let variantes = match (lido.versao, lido.passos) {
                (1, Some(passos)) => vec![Variante { passos }],
                (VERSAO, _) => lido.variantes,
                (1, None) => return Err("versão 1 sem `passos`".to_owned()),
                (v, _) => return Err(format!("versão {v} do formato, esperada {VERSAO}")),
            };
            Ok(Some(Roteiro { versao: VERSAO, objetivo: lido.objetivo, variantes }))
        }
    }
}

/// The variant to replay: the first whose step 1 fits the screen already read. Chosen once, before acting,
/// because switching after a step ran would replay on top of it.
/// A variant that fits what is on screen wins over one that only launches an app, which fits any screen.
pub fn escolher<'a>(roteiro: &'a Roteiro, obs: &Observation, dados: &Map<String, Value>) -> Option<(usize, &'a [Passo])> {
    let primeira = |f: &dyn Fn(&[Passo]) -> bool| {
        roteiro.variantes.iter().enumerate().find(|(_, v)| !v.passos.is_empty() && f(&v.passos))
    };
    let resolve = |p: &[Passo]| resolver(&p[0], obs, dados).is_some();
    primeira(&|p| ja_aberto(p, 0, obs))
        .or_else(|| primeira(&|p| p[0].acao.kind != ActionType::Launch && resolve(p)))
        .or_else(|| primeira(&resolve))
        .map(|(i, v)| (i, v.passos.as_slice()))
}

/// The run that just succeeded becomes variant 0; an older one doing the same steps is dropped.
pub fn juntar(anterior: Option<Roteiro>, objetivo: &str, passos: Vec<Passo>) -> Roteiro {
    let mesmo = |a: &[Passo], b: &[Passo]| a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| x.janela == y.janela && x.alvo == y.alvo && x.acao == y.acao);
    let mut variantes: Vec<Variante> = anterior.map(|r| r.variantes).unwrap_or_default();
    variantes.retain(|v| !mesmo(&v.passos, &passos));
    variantes.insert(0, Variante { passos });
    variantes.truncate(MAX_VARIANTES);
    Roteiro { versao: VERSAO, objetivo: objetivo.to_owned(), variantes }
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

    fn com_botoes(n: usize) -> Observation {
        let mut obs = tela("TMS");
        obs.elements = (0..n).map(|i| serde_json::from_value(json!({
            "id": format!("e{i}"), "name": "", "role": "button", "value": null, "enabled": true,
            "rect": null, "focused": false, "actions": ["invoke"],
        })).unwrap()).collect();
        obs
    }

    #[test]
    fn file_name_is_readable_and_the_same_on_any_machine() {
        let dados = json!({"usuario": "ana"}).as_object().unwrap().clone();
        let nome = arquivo(Path::new("r"), "Abrir o  Bloco de Notas e salvar o relatório!", &dados);
        let nome = nome.file_name().unwrap().to_str().unwrap();
        let inicio = "desktop-abrir-o-bloco-de-notas-e-salvar-o-relatorio-";
        assert!(nome.starts_with(inicio) && nome.ends_with(".json"), "{nome}");
        assert_eq!(nome.len(), inicio.len() + 8 + ".json".len());
        let outra = json!({"usuario": "bia"}).as_object().unwrap().clone();
        assert_eq!(arquivo(Path::new("r"), "abrir o bloco de notas e salvar o relatório!", &outra), Path::new("r").join(nome));
        assert!(arquivo(Path::new("r"), "!!", &dados).to_str().unwrap().starts_with("r/desktop-roteiro-"));
    }

    #[test]
    fn scripts_live_in_the_repository_of_the_session() {
        let raiz = std::env::temp_dir().join(format!("hcc-roteiro-pasta-{}", std::process::id()));
        let sub = raiz.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let cache = Path::new("/cache");
        assert_eq!(pasta(&sub, cache), cache);
        std::fs::create_dir(raiz.join(".git")).unwrap();
        assert_eq!(pasta(&sub, cache), raiz.join(".hangar/roteiros"));
        std::fs::remove_dir_all(raiz).unwrap();
    }

    #[test]
    fn twin_controls_must_be_as_many_as_when_recorded() {
        let clique = Action { kind: ActionType::Invoke, target: Some("e1".into()), ..Default::default() };
        let gravado = passo(&clique, &com_botoes(2), &Map::new());
        assert_eq!(resolver(&gravado, &com_botoes(2), &Map::new()).and_then(|a| a.target), Some("e1".into()));
        assert_eq!(resolver(&gravado, &com_botoes(3), &Map::new()), None);
    }

    #[test]
    fn script_from_another_format_version_is_refused_and_version_one_is_read_as_one_variant() {
        let pasta = std::env::temp_dir().join(format!("hcc-roteiro-versao-{}", std::process::id()));
        let arquivo = pasta.join("r.json");
        let clique = Action { kind: ActionType::Invoke, target: Some("e0".into()), ..Default::default() };
        let p = passo(&clique, &com_botoes(1), &Map::new());
        salvar(&arquivo, &juntar(None, "x", vec![p.clone()])).unwrap();
        assert_eq!(carregar(&arquivo).unwrap().map(|r| r.versao), Some(2));
        std::fs::write(&arquivo, json!({"versao": 1, "objetivo": "x", "passos": [p]}).to_string()).unwrap();
        let lido = carregar(&arquivo).unwrap().unwrap();
        assert_eq!((lido.versao, lido.variantes.len(), lido.variantes[0].passos.len()), (2, 1, 1));
        std::fs::write(&arquivo, r#"{"versao":3,"objetivo":"x","variantes":[]}"#).unwrap();
        assert!(carregar(&arquivo).unwrap_err().contains("versão 3"));
        std::fs::remove_dir_all(pasta).unwrap();
    }

    fn clique_em(janela: &str) -> Passo {
        let clique = Action { kind: ActionType::Invoke, target: Some("e0".into()), ..Default::default() };
        let mut obs = com_botoes(1);
        obs.windows[0].class_name = janela.into();
        passo(&clique, &obs, &Map::new())
    }

    #[test]
    fn replay_picks_the_variant_whose_first_step_fits_the_screen() {
        let roteiro = juntar(Some(juntar(None, "x", vec![clique_em("TMS")])), "x", vec![clique_em("Outra")]);
        assert_eq!(roteiro.variantes[0].passos[0].janela.classe, "Outra");
        let (i, passos) = escolher(&roteiro, &com_botoes(1), &Map::new()).unwrap();
        assert_eq!((i, passos[0].janela.classe.as_str()), (1, "TMS"));
        assert!(escolher(&roteiro, &tela("Nenhuma"), &Map::new()).is_none());
    }

    #[test]
    fn a_variant_that_fits_the_screen_beats_one_that_starts_by_launching_blindly() {
        let abrir = Action { kind: ActionType::Launch, application: Some("app".into()), ..Default::default() };
        let lancando = vec![passo(&abrir, &tela("Desktop"), &Map::new()), clique_em("Nova")];
        let roteiro = juntar(Some(juntar(None, "x", vec![clique_em("TMS")])), "x", lancando);
        assert_eq!(escolher(&roteiro, &com_botoes(1), &Map::new()).map(|(i, _)| i), Some(1));
        assert_eq!(escolher(&roteiro, &tela("Desktop"), &Map::new()).map(|(i, _)| i), Some(0));
    }

    #[test]
    fn merging_puts_the_latest_first_drops_its_twin_and_keeps_five() {
        let mut r = None;
        for j in ["A", "B", "C", "D", "E", "F"] { r = Some(juntar(r, "x", vec![clique_em(j)])); }
        r = Some(juntar(r, "x", vec![clique_em("D")]));
        let ordem: Vec<_> = r.unwrap().variantes.iter().map(|v| v.passos[0].janela.classe.clone()).collect();
        assert_eq!(ordem, ["D", "F", "E", "C", "B"]);
    }
}
