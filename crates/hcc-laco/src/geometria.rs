//! Coordinates and scaling (laco_uia.py:246-264).

use std::collections::HashSet;
use std::sync::LazyLock;

use hcc_protocolo::{Action, ActionType, Observation, Screen};
use regex::Regex;

static PAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\s*,\s*(\d+)").unwrap());

/// LLM coordinates come at screenshot scale (1280 px); a pair copied from the goal text is already screen scale.
pub fn para_tela(acao: &Action, screen: &Screen, goal: &str) -> Action {
    let mut a = acao.clone();
    if a.kind != ActionType::Mouse {
        return a;
    }
    let escala = (f64::from(screen.width) / 1280.0).max(1.0);
    let literais: HashSet<(&str, &str)> =
        PAR.captures_iter(goal).map(|c| (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str())).collect();
    let escalar = |x: &mut Option<i32>, y: &mut Option<i32>| {
        if let (Some(px), Some(py)) = (*x, *y)
            && !literais.contains(&(px.to_string().as_str(), py.to_string().as_str()))
        {
            *x = Some((f64::from(px) * escala).round_ties_even() as i32);
            *y = Some((f64::from(py) * escala).round_ties_even() as i32);
        }
    };
    escalar(&mut a.x, &mut a.y);
    escalar(&mut a.x2, &mut a.y2);
    a
}

/// A click outside the front window would land on the one behind it.
pub fn dentro(acao: &Action, obs: &Observation) -> bool {
    if acao.kind != ActionType::Mouse {
        return true;
    }
    let (Some(x), Some(y)) = (acao.x, acao.y) else { return false };
    obs.windows
        .iter()
        .find(|w| w.id == obs.foreground)
        .is_some_and(|w| w.rect.0 <= x && x < w.rect.2 && w.rect.1 <= y && y < w.rect.3)
}
