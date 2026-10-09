use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transport {
    #[default]
    Local,
    Ssh,
}

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub transport: Transport,
    pub command: Option<Vec<String>>,
    pub host: Option<String>,
    pub agent_path: Option<PathBuf>,
    pub shared_executable: Option<String>,
    pub proxy_command: Option<String>,
    pub request_timeout: Duration,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            transport: Transport::Local,
            command: None,
            host: None,
            agent_path: None,
            shared_executable: None,
            proxy_command: None,
            request_timeout: Duration::from_secs(15),
        }
    }
}

#[derive(Deserialize)]
struct FileConfig {
    command: Option<Vec<String>>,
    host: Option<String>,
    agent_path: Option<PathBuf>,
    shared_executable: Option<String>,
    proxy_command: Option<String>,
    #[serde(default = "default_timeout")]
    request_timeout: f64,
}

fn default_timeout() -> f64 { 15.0 }

impl AgentConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let prefix = |error: String| format!("{}: {error}", path.display());
        let bytes = std::fs::read(path).map_err(|error| prefix(error.to_string()))?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|error| prefix(error.to_string()))?;
        let transport = match value.get("transport") {
            None => Transport::Local,
            Some(serde_json::Value::String(name)) if name == "local" => Transport::Local,
            Some(serde_json::Value::String(name)) if name == "ssh" => Transport::Ssh,
            Some(_) => return Err("transport deve ser local ou ssh".into()),
        };
        let raw: FileConfig = serde_json::from_value(value)
            .map_err(|error| prefix(error.to_string()))?;
        let request_timeout = Duration::try_from_secs_f64(raw.request_timeout)
            .map_err(|error| prefix(error.to_string()))?;
        Ok(Self {
            transport,
            command: raw.command,
            host: raw.host,
            agent_path: raw.agent_path,
            shared_executable: raw.shared_executable,
            proxy_command: raw.proxy_command,
            request_timeout,
        })
    }
}
