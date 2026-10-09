//! Action the loop asks an agent to perform (port of `Acao`, laco_uia.py:50-77).

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum ActionType {
    #[default]
    Invoke,
    SetValue,
    Select,
    Toggle,
    Expand,
    Collapse,
    Focus,
    Scroll,
    Activate,
    Keys,
    Text,
    Launch,
    Mouse,
}

impl ActionType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invoke => "invoke",
            Self::SetValue => "set_value",
            Self::Select => "select",
            Self::Toggle => "toggle",
            Self::Expand => "expand",
            Self::Collapse => "collapse",
            Self::Focus => "focus",
            Self::Scroll => "scroll",
            Self::Activate => "activate",
            Self::Keys => "keys",
            Self::Text => "text",
            Self::Launch => "launch",
            Self::Mouse => "mouse",
        }
    }
}

impl fmt::Display for ActionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Amount {
    Small,
    Large,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Middle,
    Right,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MouseMode {
    Click,
    Double,
    Move,
    Drag,
    Scroll,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Action {
    #[serde(rename = "type")]
    pub kind: ActionType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<Direction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub button: Option<Button>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<MouseMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x2: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y2: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotulo: Option<String>,
}

impl Action {
    /// Field limits first, then the fields each type requires: the order pydantic reports them.
    pub fn validate(&self) -> Result<(), String> {
        if self.value.as_ref().is_some_and(|v| v.chars().count() > 20000) {
            return Err("value com mais de 20000 caracteres".into());
        }
        if self.keys.as_ref().is_some_and(|k| !(1..=8).contains(&k.len())) {
            return Err("keys deve ter de 1 a 8 teclas".into());
        }
        if self.delta.is_some_and(|d| !(-50..=50).contains(&d)) {
            return Err("delta fora de -50..50".into());
        }
        if self.rotulo.as_ref().is_some_and(|r| r.chars().count() > 200) {
            return Err("rotulo com mais de 200 caracteres".into());
        }
        let target = ("target", self.target.is_some());
        let exigidos: &[(&str, bool)] = match self.kind {
            ActionType::SetValue => &[target, ("value", self.value.is_some())],
            ActionType::Text => &[("value", self.value.is_some())],
            ActionType::Keys => &[("keys", self.keys.is_some())],
            ActionType::Launch => &[("application", self.application.is_some())],
            ActionType::Mouse => &[
                ("x", self.x.is_some()),
                ("y", self.y.is_some()),
                ("button", self.button.is_some()),
                ("mode", self.mode.is_some()),
            ],
            ActionType::Scroll => &[target, ("direction", self.direction.is_some()), ("amount", self.amount.is_some())],
            _ => &[target],
        };
        if exigidos.iter().any(|(_, presente)| !presente) {
            let nomes: Vec<&str> = exigidos.iter().map(|(nome, _)| *nome).collect();
            return Err(format!("{} exige {}", self.kind, tupla_python(&nomes)));
        }
        Ok(())
    }
}

/// `('a', 'b')`, and `('a',)` for one item, like Python's tuple repr.
fn tupla_python(itens: &[&str]) -> String {
    let partes: Vec<String> = itens.iter().map(|i| format!("'{i}'")).collect();
    if partes.len() == 1 {
        format!("({},)", partes[0])
    } else {
        format!("({})", partes.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acao(kind: ActionType) -> Action {
        Action { kind, target: Some("e1".into()), ..Default::default() }
    }

    #[test]
    fn limits_count_characters_not_bytes() {
        let ok = Action { value: Some("ç".repeat(20000)), ..acao(ActionType::SetValue) };
        assert_eq!(ok.validate(), Ok(()));
        let longo = Action { value: Some("a".repeat(20001)), ..acao(ActionType::SetValue) };
        assert_eq!(longo.validate().unwrap_err(), "value com mais de 20000 caracteres");
        let rotulo = Action { rotulo: Some("ã".repeat(201)), ..acao(ActionType::Invoke) };
        assert_eq!(rotulo.validate().unwrap_err(), "rotulo com mais de 200 caracteres");
    }

    #[test]
    fn keys_and_delta_bounds() {
        let nove = Action { keys: Some(vec!["a".into(); 9]), ..acao(ActionType::Keys) };
        assert_eq!(nove.validate().unwrap_err(), "keys deve ter de 1 a 8 teclas");
        let oito = Action { keys: Some(vec!["a".into(); 8]), ..acao(ActionType::Keys) };
        assert_eq!(oito.validate(), Ok(()));
        let delta = Action { delta: Some(-50), ..acao(ActionType::Invoke) };
        assert_eq!(delta.validate(), Ok(()));
        let delta = Action { delta: Some(-51), ..acao(ActionType::Invoke) };
        assert_eq!(delta.validate().unwrap_err(), "delta fora de -50..50");
    }

    #[test]
    fn scroll_requires_direction_and_amount() {
        assert_eq!(acao(ActionType::Scroll).validate().unwrap_err(), "scroll exige ('target', 'direction', 'amount')");
        assert_eq!(Action { kind: ActionType::Text, ..Default::default() }.validate().unwrap_err(), "text exige ('value',)");
    }

    #[test]
    fn action_type_display_is_wire_name() {
        assert_eq!(ActionType::SetValue.to_string(), "set_value");
        assert_eq!(serde_json::to_value(ActionType::SetValue).unwrap(), "set_value");
    }
}
