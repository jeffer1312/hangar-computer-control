//! Candidate actions from an observation (laco_uia.py:148-172).

use std::collections::HashSet;
use std::sync::LazyLock;

use hcc_protocolo::{Action, ActionType, Amount, Button, Direction, Element, ElementAction, MouseMode, Observation, Rect, dividir_comando};
use regex::Regex;
use serde_json::{Map, Value};

use crate::descrever::{Alvo, alvo, descrever, intencao};
use crate::tipos::Candidato;

static CHAVE_SECRETA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)senha|password|passwd|pin|token").unwrap());
static FECHAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(fech|encerr|close|quit|exit)\w*").unwrap());
static ABRIR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(abr[ia]|inici|execut|lan[cç]|open|start|launch|run)\w*").unwrap());

/// Roles whose real click is offered when UIA has no pattern for them (laco_uia.py:159).
const CLICAVEIS: [&str; 7] = ["ListItem", "TreeItem", "Hyperlink", "Button", "MenuItem", "TabItem", "Image"];

pub fn chave_secreta(chave: &str) -> bool {
    CHAVE_SECRETA.is_match(chave)
}

/// Values of secret keys, as Python `str()` prints them (laco_uia.py:400-401).
pub fn segredos(dados: &Map<String, Value>) -> HashSet<String> {
    dados.iter().filter(|(k, _)| chave_secreta(k)).map(|(_, v)| py_str(v)).collect()
}

/// Python `str()` of a JSON value.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if !n.is_i64() && !n.is_u64() => py_float(f),
            _ => n.to_string(),
        },
        // ponytail: lists/dicts in `dados` are never typed; Python would print its own repr here.
        Value::Array(_) | Value::Object(_) => v.to_string(),
    }
}

/// Python `repr(float)`: shortest round-trip digits, exponent form outside 1e-4..1e16.
fn py_float(f: f64) -> String {
    let e = format!("{f:e}");
    let (mantissa, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    if (-4..16).contains(&exp) {
        let s = format!("{f}");
        if s.contains('.') { s } else { s + ".0" }
    } else {
        format!("{mantissa}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

fn tipo(a: ElementAction) -> ActionType {
    match a {
        ElementAction::Invoke => ActionType::Invoke,
        ElementAction::Select => ActionType::Select,
        ElementAction::Toggle => ActionType::Toggle,
        ElementAction::Scroll => ActionType::Scroll,
        ElementAction::Expand => ActionType::Expand,
        ElementAction::Collapse => ActionType::Collapse,
        ElementAction::SetValue => ActionType::SetValue,
        ElementAction::Focus => ActionType::Focus,
    }
}

/// Python `(a + b) // 2`, without i32 overflow on a hostile rect.
pub fn centro(r: Rect) -> (i32, i32) {
    let meio = |a: i32, b: i32| ((i64::from(a) + i64::from(b)).div_euclid(2)) as i32;
    (meio(r.0, r.2), meio(r.1, r.3))
}

pub fn clique(target: Option<String>, x: i32, y: i32) -> Action {
    Action {
        kind: ActionType::Mouse,
        target,
        x: Some(x),
        y: Some(y),
        button: Some(Button::Left),
        mode: Some(MouseMode::Click),
        ..Default::default()
    }
}

pub fn candidatos(obs: &Observation, dados: &Map<String, Value>, segredos: &HashSet<String>) -> Vec<Candidato> {
    let mut acoes: Vec<Action> = obs
        .windows
        .iter()
        .filter(|w| w.id != obs.foreground)
        .map(|w| Action { kind: ActionType::Activate, target: Some(w.id.clone()), ..Default::default() })
        .collect();
    acoes.extend(obs.apps.iter().filter_map(|app| {
        let (name, command) = app.split_once(" => ")?;
        if name.is_empty() { return None; }
        let mut tokens = dividir_comando(command)?.into_iter();
        let executable = tokens.next().filter(|t| !t.is_empty())?;
        let args: Vec<String> = tokens.collect();
        Some(Action { kind: ActionType::Launch, application: Some(executable), args: (!args.is_empty()).then_some(args), rotulo: Some(name.into()), ..Default::default() })
    }));
    let valores: Vec<String> =
        dados.values().filter(|v| matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_))).map(py_str).collect();
    for e in obs.elements.iter().filter(|e| e.enabled) {
        let sem_padrao = e.actions.iter().all(|a| *a == ElementAction::Focus);
        // Menu VCL ignores UIA Invoke/Expand, and a link inside a list item has no pattern: real click.
        if let Some(r) = e.rect.filter(|_| (e.role == "MenuItem" || sem_padrao) && CLICAVEIS.contains(&e.role.as_str())) {
            let (x, y) = centro(r);
            acoes.push(clique(Some(e.id.clone()), x, y));
            continue;
        }
        for &a in &e.actions {
            let base = Action { kind: tipo(a), target: Some(e.id.clone()), ..Default::default() };
            match a {
                ElementAction::SetValue => {
                    acoes.extend(valores.iter().map(|v| Action { value: Some(v.clone()), ..base.clone() }))
                }
                ElementAction::Scroll => acoes.extend([Direction::Up, Direction::Down].map(|d| Action {
                    direction: Some(d),
                    amount: Some(Amount::Small),
                    ..base.clone()
                })),
                // Focusing is never the intended step; invoke/set_value already focus.
                ElementAction::Focus => {}
                _ => acoes.push(base),
            }
        }
    }
    acoes.extend(
        ["Enter", "Escape", "Tab"].map(|k| Action { kind: ActionType::Keys, keys: Some(vec![k.into()]), ..Default::default() }),
    );
    acoes
        .into_iter()
        .map(|acao| Candidato {
            descricao: descrever(&acao, obs, segredos, true),
            intencao: intencao(&acao, obs),
            acao,
        })
        .collect()
}

/// `termo` appears in `texto` as whole words, ignoring case.
fn nomeado(texto: &str, termo: &str) -> bool {
    let termo = termo.trim().to_lowercase();
    !termo.is_empty() && texto.match_indices(&termo).any(|(i, _)| {
        let fora = |c: Option<char>| !c.is_some_and(char::is_alphanumeric);
        fora(texto[..i].chars().next_back()) && fora(texto[i + termo.len()..].chars().next())
    })
}

/// A goal that names installed apps may only launch those: the full installed list lets Jev open another browser.
pub fn apps_do_objetivo(cands: Vec<Candidato>, goal: &str) -> Vec<Candidato> {
    // A goal that only closes has nothing to launch: reopening the app made Jev declare DONE with it open again.
    if FECHAR.is_match(goal) && !ABRIR.is_match(goal) {
        return cands.into_iter().filter(|c| c.acao.kind != ActionType::Launch).collect();
    }
    let goal = goal.to_lowercase();
    let citado = |a: &Action| {
        let executavel = a.application.as_deref().unwrap_or_default().rsplit(['/', '\\']).next().unwrap_or_default();
        let executavel = executavel.strip_suffix(".exe").or_else(|| executavel.strip_suffix(".EXE")).unwrap_or(executavel);
        nomeado(&goal, a.rotulo.as_deref().unwrap_or_default()) || nomeado(&goal, executavel)
    };
    if !cands.iter().any(|c| c.acao.kind == ActionType::Launch && citado(&c.acao)) {
        return cands;
    }
    cands.into_iter().filter(|c| c.acao.kind != ActionType::Launch || citado(&c.acao)).collect()
}

fn editavel(e: &Element) -> bool {
    e.enabled && (e.role == "Edit" || e.actions.contains(&ElementAction::SetValue))
}

/// Untargeted text goes where the focus is, often a tab or a banner: aim it at the editable field instead.
pub fn texto_no_editavel(acao: Action, obs: &Observation) -> Vec<Action> {
    if acao.kind != ActionType::Text || acao.target.is_some() || obs.elements.iter().any(|e| e.focused && editavel(e)) {
        return vec![acao];
    }
    let alvos: Vec<Action> =
        obs.elements.iter().filter(|e| editavel(e)).map(|e| Action { target: Some(e.id.clone()), ..acao.clone() }).collect();
    if alvos.is_empty() { vec![acao] } else { alvos }
}

/// `descrever`, except that text aimed at a field names the field instead of "no foco atual".
pub fn descrever_acao(acao: &Action, obs: &Observation, segredos: &HashSet<String>, com_valor: bool) -> String {
    let Some(Alvo::Elemento(e)) = alvo(acao, obs).filter(|_| acao.kind == ActionType::Text) else {
        return descrever(acao, obs, segredos, com_valor);
    };
    let texto = descrever(&Action { target: None, rotulo: None, ..acao.clone() }, obs, segredos, com_valor && !e.password);
    format!("{} em {} '{}'", texto.strip_suffix(" no foco atual").unwrap_or(&texto), e.role, e.name)
}
