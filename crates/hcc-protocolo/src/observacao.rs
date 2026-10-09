//! Observation of the desktop, as the agents send it.

use serde::{Deserialize, Serialize};

/// `[left, top, right, bottom]`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rect(pub i32, pub i32, pub i32, pub i32);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Window {
    pub id: String,
    pub name: String,
    pub process_id: u32,
    pub class_name: String,
    pub rect: Rect,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ElementAction {
    Invoke,
    Select,
    Toggle,
    Scroll,
    Expand,
    Collapse,
    SetValue,
    Focus,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Element {
    pub id: String,
    pub name: String,
    pub role: String,
    pub value: Option<String>,
    pub enabled: bool,
    pub rect: Option<Rect>,
    pub focused: bool,
    pub actions: Vec<ElementAction>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub password: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Screen {
    pub width: u32,
    pub height: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Observation {
    pub observation_id: String,
    pub connected: bool,
    pub session_id: u32,
    pub foreground: String,
    pub windows: Vec<Window>,
    pub elements: Vec<Element>,
    pub truncated: bool,
    pub timestamp: f64,
    pub screen: Screen,
}
