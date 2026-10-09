//! Action descriptions and intents (laco_uia.py:175-208).

use std::collections::HashSet;
use std::fmt::Write;

use hcc_protocolo::{Action, ActionType, Direction, Element, MouseMode, Observation, Window};

use crate::barreiras::fecha_janela;
use crate::tipos::Intencao;

pub enum Alvo<'a> {
    Janela(&'a Window),
    Elemento(&'a Element),
}

impl Alvo<'_> {
    fn nome(&self) -> &str {
        match self {
            Alvo::Janela(w) => &w.name,
            Alvo::Elemento(e) => &e.name,
        }
    }
}

/// Python built `{id: x for x in [*windows, *elements]}`: elements win, the last duplicate wins.
pub fn alvo<'a>(acao: &Action, obs: &'a Observation) -> Option<Alvo<'a>> {
    let id = acao.target.as_deref()?;
    obs.elements
        .iter()
        .rev()
        .find(|e| e.id == id)
        .map(Alvo::Elemento)
        .or_else(|| obs.windows.iter().rev().find(|w| w.id == id).map(Alvo::Janela))
}

pub fn descrever(acao: &Action, obs: &Observation, segredos: &HashSet<String>, com_valor: bool) -> String {
    let alvo = alvo(acao, obs);
    let mut nome = match &alvo {
        Some(Alvo::Janela(w)) => format!(" window '{}'", w.name),
        Some(Alvo::Elemento(e)) => format!(" {} '{}'", e.role, e.name),
        None => match acao.rotulo.as_deref() {
            Some(r) if !r.is_empty() => format!(" '{r}'"),
            _ => String::new(),
        },
    };
    if alvo.is_some() && fecha_janela(acao, obs) {
        nome += " (title bar button: closes the whole application window)";
    }
    let secreto = matches!(alvo, Some(Alvo::Elemento(e)) if e.password)
        || acao.value.as_ref().is_some_and(|v| segredos.contains(v));
    let valor = if com_valor && !secreto {
        acao.value.as_deref().map_or_else(|| "None".to_owned(), py_repr)
    } else {
        "***".to_owned()
    };
    let opt = |n: Option<i32>| n.map_or_else(|| "None".to_owned(), |n| n.to_string());
    let extra = match acao.kind {
        ActionType::SetValue => format!(" = {valor}"),
        ActionType::Text => format!(" {valor} no foco atual"),
        ActionType::Keys => format!(" {}", acao.keys.as_deref().unwrap_or_default().join("+")),
        ActionType::Launch => {
            let mut partes = vec![acao.application.clone().unwrap_or_default()];
            partes.extend(acao.args.iter().flatten().cloned());
            format!(" {}", partes.join(" "))
        }
        ActionType::Scroll => format!(" {}", acao.direction.map_or("None", direcao)),
        ActionType::Mouse => format!(" {} ({},{})", acao.mode.map_or("None", modo), opt(acao.x), opt(acao.y)),
        _ => String::new(),
    };
    format!("{}{nome}{extra}", acao.kind)
}

pub fn direcao(d: Direction) -> &'static str {
    match d {
        Direction::Up => "up",
        Direction::Down => "down",
        Direction::Left => "left",
        Direction::Right => "right",
    }
}

pub fn modo(m: MouseMode) -> &'static str {
    match m {
        MouseMode::Click => "click",
        MouseMode::Double => "double",
        MouseMode::Move => "move",
        MouseMode::Drag => "drag",
        MouseMode::Scroll => "scroll",
    }
}

pub fn intencao(acao: &Action, obs: &Observation) -> Intencao {
    let nome = alvo(acao, obs).map(|a| a.nome().replace('&', "")).unwrap_or_default();
    let tipo = match acao.kind {
        ActionType::SetValue | ActionType::Text => "text".to_owned(),
        k => k.to_string(),
    };
    Intencao {
        tipo,
        alvo: casefold(nome.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)).trim_end_matches(':')),
        valor: acao.value.clone(),
        teclas: acao.keys.clone().unwrap_or_default(),
        aplicacao: acao.application.clone(),
    }
}

/// Python `str.casefold()`.
// ponytail: lowercase plus the folds Portuguese/Greek UI text hits; full CaseFolding.txt if ligatures show up.
pub fn casefold(s: &str) -> String {
    s.to_lowercase().replace('ß', "ss").replace('ς', "σ").replace('µ', "μ").replace('ſ', "s")
}

/// Python `repr(str)`.
pub fn py_repr(s: &str) -> String {
    let aspa = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(aspa);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == aspa => {
                out.push('\\');
                out.push(c);
            }
            c if !imprimivel(c) => {
                let n = c as u32;
                let _ = match n {
                    0..0x100 => write!(out, "\\x{n:02x}"),
                    0x100..0x10000 => write!(out, "\\u{n:04x}"),
                    _ => write!(out, "\\U{n:08x}"),
                };
            }
            c => out.push(c),
        }
    }
    out.push(aspa);
    out
}

/// Python `str.isprintable()` per character: not Cc, Cf, Co, Zl, Zp, nor Zs other than space.
// ponytail: unassigned (Cn) code points count as printable here; a Unicode table crate fixes that if it matters.
fn imprimivel(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    let n = c as u32;
    let formato = matches!(n,
        0xAD | 0x600..=0x605 | 0x61C | 0x6DD | 0x70F | 0x890..=0x891 | 0x8E2 | 0x180E | 0x200B..=0x200F
        | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD
        | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F);
    let privado = matches!(n, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD);
    !(c.is_control() || c.is_whitespace() || formato || privado)
}
