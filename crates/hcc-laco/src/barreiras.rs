//! Loop guards: no effect, repeats, warnings, closing windows (laco_uia.py:191-201, :409-446, :522-525).

use std::collections::HashSet;
use std::sync::LazyLock;

use hcc_protocolo::{Action, ActionType, Observation};
use regex::Regex;

use crate::candidatos::{centro, clique};
use crate::descrever::{casefold, descrever, intencao};
use crate::tipos::{Candidato, Registro};

static PEDE_FECHAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)\b(fech|encerr|sair|close|quit|exit)\w*\b.*\b(janela|aplicativo|aplicação|programa|sistema|window|application|app)\b",
    )
    .unwrap()
});

const OFERTA: &str = "; a real mouse click on the same control is now offered";

static LITERAIS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?:^|[^\p{L}\p{N}])'([^']*)'|"([^"]*)"|“([^”]*)”"#).unwrap());
static COMBOS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(ctrl|control|alt|shift|super|win)(\+\w+)+").unwrap());

fn normalizar_combo(teclas: &str) -> String {
    let mut teclas: Vec<String> = teclas.split('+').map(|t| match t.to_ascii_uppercase().as_str() {
        "CONTROL" => "CTRL".into(), "SUPER" | "META" => "WIN".into(), _ => t.to_ascii_uppercase(),
    }).collect();
    teclas.sort_by_key(|t| match t.as_str() { "CTRL" => 0, "ALT" => 1, "SHIFT" => 2, "WIN" => 3, _ => 4 });
    teclas.join("+")
}

fn pressionado(teclas: &str, recentes: &[Registro]) -> bool {
    recentes.iter().filter(|r| r.executada && (r.result == "ok" || r.result.strip_prefix("ok") == Some(OFERTA)))
        .filter_map(|r| r.action.strip_prefix("keys ").and_then(|s| s.rsplit(' ').next()))
        .any(|r| normalizar_combo(r) == teclas)
}

/// A secret typed before the goal's keys (Ctrl+F) lands in the wrong field, in plain sight: hold it.
pub fn segurar_segredos(cands: Vec<Candidato>, objetivo: &str, recentes: &[Registro], segredos: &HashSet<String>) -> Vec<Candidato> {
    let ocultos: Vec<std::ops::Range<usize>> = crate::estado::padrao_segredos(segredos)
        .map(|r| r.find_iter(objetivo).map(|m| m.range()).collect()).unwrap_or_default();
    let falta = COMBOS.find_iter(objetivo)
        .filter(|c| !ocultos.iter().any(|o| o.start < c.end() && c.start() < o.end))
        .any(|c| !pressionado(&normalizar_combo(c.as_str()), recentes));
    if !falta { return cands; }
    cands.into_iter().filter(|c| {
        !matches!(c.acao.kind, ActionType::SetValue | ActionType::Text)
            || !c.acao.value.as_deref().is_some_and(|v| segredos.iter().any(|s| !s.is_empty() && v.contains(s.as_str())))
    }).collect()
}

pub fn pendente(objetivo: &str, obs: &Observation, recentes: &[Registro], segredos: &HashSet<String>) -> Option<String> {
    pendencia(objetivo, obs, recentes, segredos).map(|(_, texto)| texto)
}

/// How many explicit items `pendente` checks: quoted literals (only on a non-empty tree) plus named key combos.
pub fn itens_pendentes(objetivo: &str, obs: &Observation) -> usize {
    let literais = if obs.elements.is_empty() { 0 } else {
        LITERAIS.captures_iter(objetivo).filter_map(|c| c.iter().skip(1).flatten().next())
            .filter(|l| l.as_str().chars().count() >= 2).count()
    };
    literais + COMBOS.find_iter(objetivo).count()
}

pub(crate) fn pendencia(objetivo: &str, obs: &Observation, recentes: &[Registro], segredos: &HashSet<String>) -> Option<(usize, String)> {
    let publico = |texto: &str| {
        if segredos.iter().any(|s| s.contains(texto)) { return "<senha>".into(); }
        crate::estado::padrao_segredos(segredos).map_or_else(
            || if segredos.iter().any(|s| !s.is_empty()) { "<senha>".into() } else { texto.to_owned() },
            |r| r.replace_all(texto, "<senha>").into_owned(),
        )
    };
    let ocultos: Vec<std::ops::Range<usize>> = crate::estado::padrao_segredos(segredos)
        .map(|r| r.find_iter(objetivo).map(|m| m.range()).collect()).unwrap_or_default();
    let toca = |r: std::ops::Range<usize>| ocultos.iter().any(|o| o.start < r.end && r.start < o.end);
    if !obs.elements.is_empty() {
        for (indice, literal) in LITERAIS.captures_iter(objetivo).filter_map(|c| c.iter().skip(1).flatten().next()).enumerate() {
            let texto = literal.as_str();
            if texto.chars().count() < 2 { continue; }
            if !obs.elements.iter().any(|e| e.name.contains(texto) || e.value.as_deref().is_some_and(|v| v.contains(texto))) {
                let nome = if toca(literal.range()) { "<senha>".into() } else { publico(texto) };
                return Some((indice, format!("ainda falta: o texto '{nome}' não aparece na tela")));
            }
        }
    }
    let inicio_combos = LITERAIS.find_iter(objetivo).count();
    for (indice, combo) in COMBOS.find_iter(objetivo).enumerate() {
        let teclas = normalizar_combo(combo.as_str());
        if !pressionado(&teclas, recentes) {
            let oculto = publico(combo.as_str());
            let nome = if oculto == combo.as_str() && !toca(combo.range()) { teclas } else { "<senha>".into() };
            return Some((inicio_combos + indice, format!("ainda falta: {nome} não foi pressionado")));
        }
    }
    None
}

/// Alt+F4, or the Close button of the title bar: closes the whole window, not a tab.
pub fn fecha_janela(acao: &Action, obs: &Observation) -> bool {
    if acao.kind == ActionType::Keys {
        let teclas: HashSet<String> = acao.keys.iter().flatten().map(|k| casefold(k)).collect();
        return teclas.contains("alt") && teclas.contains("f4");
    }
    let Some(alvo) = acao.target.as_deref().and_then(|id| obs.elements.iter().rev().find(|e| e.id == id)) else {
        return false;
    };
    let Some(r) = alvo.rect.filter(|_| alvo.role == "Button" && matches!(casefold(&alvo.name).as_str(), "close" | "fechar"))
    else {
        return false;
    };
    obs.elements.iter().any(|t| {
        t.role == "TitleBar"
            && t.rect.is_some_and(|t| {
                let folga = |a: i32, b: i32| i64::from(a) <= i64::from(b) + 1;
                folga(t.0, r.0) && folga(t.1, r.1) && folga(r.2, t.2) && folga(r.3, t.3)
            })
    })
}

pub fn objetivo_pede_fechar(goal: &str) -> bool {
    PEDE_FECHAR.is_match(goal)
}

/// Text of the app's message box: Text/Edit longer than 15 chars, only on small trees.
pub fn aviso(obs: &Observation) -> Option<String> {
    if obs.elements.len() > 25 {
        return None;
    }
    let textos: Vec<&str> = obs
        .elements
        .iter()
        .filter(|e| e.role == "Text" || e.role == "Edit")
        .map(|e| e.value.as_deref().filter(|v| !v.is_empty()).unwrap_or(&e.name))
        .filter(|t| t.chars().count() > 15)
        .collect();
    Some(textos.join(" ")).filter(|t| !t.is_empty())
}

fn executadas(hist: &[Registro]) -> impl Iterator<Item = &Registro> {
    hist.iter().filter(|r| r.action != "WAIT" && !r.action.starts_with("DONE recusado: "))
}

#[derive(Debug, Default)]
pub struct Barreiras {
    avisos: Vec<String>,
    anterior: String,
}

impl Barreiras {
    /// The same warning coming back after another attempt is the app saying no. The same one in
    /// consecutive cycles is a chain of boxes (OK opens the next), not a refusal.
    pub fn aviso_repetido(&mut self, obs: &Observation) -> Option<String> {
        let textos = aviso(obs).unwrap_or_default();
        if !textos.is_empty() && textos != self.anterior {
            self.avisos.push(textos.clone());
            if self.avisos.iter().filter(|a| **a == textos).count() >= 2 {
                let corte: String = textos.chars().take(400).collect();
                return Some(format!("o aplicativo respondeu duas vezes com o mesmo aviso: {corte}"));
            }
        }
        self.anterior = textos;
        None
    }

    pub fn tres_sem_efeito(hist: &[Registro]) -> Option<String> {
        let feitas: Vec<&Registro> = executadas(hist).collect();
        let ultimas = &feitas[feitas.len().saturating_sub(3)..];
        (feitas.len() >= 3 && ultimas.iter().all(|r| r.screen_changed == Some(false))).then(|| {
            let nomes: Vec<&str> = ultimas.iter().map(|r| r.action.as_str()).collect();
            format!("três ações seguidas sem efeito na tela: {}", nomes.join("; "))
        })
    }

    /// Drops what ran without effect in the last 3, what ran twice, and a third click on the same spot.
    pub fn barrar(cands: Vec<Candidato>, hist: &[Registro]) -> Vec<Candidato> {
        let feitas: Vec<&Registro> = executadas(hist).collect();
        let sem_efeito: HashSet<&str> = feitas[feitas.len().saturating_sub(3)..]
            .iter()
            .filter(|r| r.screen_changed == Some(false))
            .map(|r| r.action.as_str())
            .collect();
        let vezes = |a: &str| feitas.iter().filter(|r| r.action == a).count();
        cands
            .into_iter()
            .filter(|c| {
                if sem_efeito.contains(c.descricao.as_str()) || vezes(&c.descricao) >= 2 {
                    return false;
                }
                // The LLM's click drifts a few pixels per cycle: count the region, not the point.
                let a = &c.acao;
                let (Some(modo), Some(x), Some(y)) = (a.mode.filter(|_| a.kind == ActionType::Mouse), a.x, a.y) else {
                    return true;
                };
                feitas
                    .iter()
                    .filter_map(|r| r.clique)
                    .filter(|&(m, cx, cy)| m == modo && cx.abs_diff(x) <= 15 && cy.abs_diff(y) <= 15)
                    .count()
                    < 2
            })
            .collect()
    }

    /// Accessibility action "ok" but the screen did not change (VCL menus ignore Invoke): offer the real click.
    pub fn reclique(hist: &mut [Registro], obs: &Observation) -> Option<Candidato> {
        let ultima = hist.iter_mut().rev().find(|r| r.action != "WAIT" && !r.action.starts_with("DONE recusado: "))?;
        let r = ultima.target_rect.filter(|_| ultima.screen_changed == Some(false))?;
        if !ultima.result.contains("real mouse click") {
            ultima.result.push_str(OFERTA);
        }
        let (x, y) = centro(r);
        let acao = clique(None, x, y);
        Some(Candidato {
            descricao: descrever(&acao, obs, &HashSet::new(), true),
            intencao: intencao(&acao, obs),
            acao,
        })
    }
}
