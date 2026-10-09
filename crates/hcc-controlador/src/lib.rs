//! Controller: starts agents (local or over ssh) and serves their long-poll.

pub mod config;
pub mod servidor;
pub mod sessao;
pub mod local;
pub mod ssh;

pub use config::{AgentConfig, Transport};
pub use sessao::{AgentConnector, AgentSession};
