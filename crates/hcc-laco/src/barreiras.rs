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
    hist.iter().filter(|r| r.action != "WAIT")
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
        let ultima = hist.iter_mut().rev().find(|r| r.action != "WAIT")?;
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
