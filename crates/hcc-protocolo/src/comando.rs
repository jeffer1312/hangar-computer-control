//! Messages between controller and agent over the reverse long-poll.

use crate::acao::Action;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const PROTOCOL: u32 = 2;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "operation", rename_all = "snake_case")]
#[expect(clippy::large_enum_variant, reason = "one command per RPC; boxing `action` would change the interface every crate matches on")]
pub enum Command {
    Heartbeat,
    Stop,
    Observe { id: String, ttl_ms: u64 },
    Act { id: String, ttl_ms: u64, action: Action, observation_id: String },
    Screenshot { id: String, ttl_ms: u64 },
}

/// Body of every agent POST: hello, idle poll, or the answer to a command.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
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
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActResult {
    pub ok: bool,
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
