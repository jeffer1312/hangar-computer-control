//! Messages between controller and agent over the reverse long-poll.

use crate::acao::Action;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const PROTOCOL: u32 = 3;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "operation", rename_all = "snake_case")]
#[expect(clippy::large_enum_variant, reason = "one command per RPC; boxing `action` would change the interface every crate matches on")]
pub enum Command {
    Heartbeat,
    Stop,
    Observe {
        id: String,
        ttl_ms: u64,
        #[serde(default, skip_serializing_if = "Scope::is_default")]
        scope: Scope,
    },
    Act { id: String, ttl_ms: u64, action: Action, observation_id: String },
    Screenshot {
        id: String,
        ttl_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<String>,
    },
}

/// What an observation reads. The default is the whole foreground window.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    /// `Window.id` read even when it is not in front; it becomes `Observation.foreground`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// Region to drill into; `None` reads the whole window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<RootSpec>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skeleton: bool,
}

impl Scope {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// A region found by role and name; `index` counts equal matches in reading order.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RootSpec {
    pub role: String,
    pub name: String,
    pub index: u32,
}

/// Whether an action reached the app. Repeating is safe only after `NotDelivered`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    #[default]
    Unknown,
    NotDelivered,
    DeliveredUnverified,
    DeliveredVerified,
}

/// Body of every agent POST: hello, idle poll, or the answer to a command.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct AgentPost {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hello: bool,
    pub boot: String,
    pub session_id: u32,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Only on a failed `act`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActResult {
    pub ok: bool,
    #[serde(default)]
    pub delivery: Delivery,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ScreenshotResult {
    /// base64 STANDARD of the PNG.
    pub png: String,
}

/// The connection JSON file handed to the agent.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Connection {
    pub url: String,
    pub token: String,
}

// The token authenticates the agent; keep it out of logs.
impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection").field("url", &self.url).field("token", &"<oculto>").finish()
    }
}
