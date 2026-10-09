//! Compact state sent to Jev, screen signature and the no-controls check (laco_uia.py:211-243).

use std::borrow::Cow;
use std::collections::HashSet;

use hcc_protocolo::{ElementAction, Observation, Rect};
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::candidatos::{chave_secreta, segredos};
use crate::descrever::py_repr_com;
use crate::tipos::Registro;

const NAO_JANELAS: [&str; 3] = ["Shell_TrayWnd", "Progman", "Worker Window"];

fn ate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Screen change = foreground id + multiset of (role, name, value[:200]), as a sorted list.
pub fn assinatura(obs: &Observation) -> (String, Vec<(String, String, String)>) {
    let mut itens: Vec<_> = obs
        .elements
        .iter()
        .map(|e| (e.role.clone(), e.name.clone(), ate(e.value.as_deref().unwrap_or("None"), 200)))
        .collect();
    itens.sort();
    (obs.foreground.clone(), itens)
}

pub fn estado_compacto(
    goal: &str,
    dados: &Map<String, Value>,
    obs: &Observation,
    recentes: &[Registro],
    hint: Option<&str>,
) -> Value {
    let janela = obs.windows.iter().find(|w| w.id == obs.foreground).map(|w| w.name.clone());
    let elementos: Vec<Value> = obs
        .elements
        .iter()
        .map(|e| {
            let mut m = Map::new();
            m.insert("id".into(), json!(e.id));
            m.insert("role".into(), json!(e.role));
            m.insert("name".into(), json!(e.name));
            if let Some(v) = e.value.as_deref().filter(|v| !v.is_empty()) {
                m.insert("value".into(), json!(v));
            }
            if e.focused {
                m.insert("focused".into(), json!(true));
            }
            if e.password {
                m.insert("password".into(), json!(true));
            }
            Value::Object(m)
        })
        .collect();
    let outras: Vec<&str> = obs
        .windows
        .iter()
        .filter(|w| w.id != obs.foreground && !NAO_JANELAS.contains(&w.class_name.as_str()))
        .map(|w| w.name.as_str())
        .collect();
    let recentes: Vec<Value> = recentes[recentes.len().saturating_sub(10)..]
        .iter()
        .map(|r| json!({"action": r.action, "result": r.result, "screenChanged": r.screen_changed}))
        .collect();
    let mut estado = json!({
        "goal": goal,
        "data": dados,
        "window": janela,
        "otherWindows": outras,
        "elements": elementos,
        "recentActions": recentes,
    });
    if let Some(h) = hint.filter(|h| !h.is_empty()) {
        estado["hint"] = json!(h);
    }
    let valores = segredos(dados);
    let padrao = padrao_segredos(&valores);
    if valores.iter().any(|s| !s.is_empty()) {
        mascarar(&mut estado, &padrao.expect("não foi possível preparar a proteção dos segredos"));
    }
    // After the pass, so a short secret never re-matches inside the placeholder.
    for (k, v) in estado["data"].as_object_mut().into_iter().flatten() {
        if chave_secreta(k) {
            *v = json!("<senha>");
        }
    }
    // Cut after masking: a secret straddling char 200 must not leave its prefix behind.
    for e in estado["elements"].as_array_mut().into_iter().flatten() {
        if let Some(Value::String(v)) = e.get_mut("value") {
            *v = ate(v, 200);
        }
    }
    estado
}

/// Raw and repr echoes in one pass, longest first, without re-matching `<senha>`.
pub(crate) fn padrao_segredos(valores: &HashSet<String>) -> Option<Regex> {
    let mut sec = Vec::new();
    for s in valores.iter().filter(|s| !s.is_empty()) {
        sec.push((s.len(), regex::escape(s)));
        for aspa in ['\'', '"'] {
            let tamanho = py_repr_com(s, aspa).len() - 2;
            let mut alternativa = String::new();
            for c in s.chars() {
                let literal = c.to_string();
                let repr = py_repr_com(&literal, aspa);
                let interior = &repr[1..repr.len() - 1];
                // Agents disagree on which Unicode characters are printable.
                if !c.is_control() && c != '\\' && c != aspa && interior != literal {
                    alternativa.push_str(&format!("(?:{}|{})", regex::escape(interior), regex::escape(&literal)));
                } else {
                    alternativa.push_str(&regex::escape(interior));
                }
            }
            sec.push((tamanho, alternativa));
        }
    }
    sec.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    sec.dedup();
    let alternativas: Vec<String> = sec.into_iter().map(|(_, s)| s).collect();
    (!alternativas.is_empty()).then(|| Regex::new(&alternativas.join("|"))).and_then(Result::ok)
}

/// Every string of the state, whatever field echoes it (titles, recent actions, data under a plain key).
fn mascarar(v: &mut Value, re: &Regex) {
    match v {
        Value::String(s) => {
            if let Cow::Owned(m) = re.replace_all(s, "<senha>") {
                *s = m;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| mascarar(x, re)),
        Value::Object(o) => o.values_mut().for_each(|x| mascarar(x, re)),
        _ => {}
    }
}

/// Front window with nothing actionable in the tree (drawn buttons); the taskbar does not count.
pub fn sem_controles(obs: &Observation) -> bool {
    let barra = obs.windows.iter().find(|w| w.class_name == "Shell_TrayWnd").map(|w| w.rect);
    let na_barra = |r: Option<Rect>| match (barra, r) {
        (Some(b), Some(r)) => b.0 <= r.0 && b.1 <= r.1 && r.2 <= b.2 && r.3 <= b.3,
        _ => false,
    };
    !obs.elements
        .iter()
        .any(|e| e.enabled && e.actions.iter().any(|a| *a != ElementAction::Focus) && !na_barra(e.rect))
}
