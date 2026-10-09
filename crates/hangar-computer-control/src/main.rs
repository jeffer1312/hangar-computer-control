//! hangar-computer-control: MCP server by default, desktop agent with `agent --config <path>`.
// The Python exe was `--noconsole`; inherited stdio pipes still carry the MCP on a Windows host.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod agente;
mod alvos;
mod mcp;
mod motor;
#[cfg_attr(feature = "motor-fake", allow(dead_code))]
mod motor_real;
mod progresso;

use std::ffi::OsString;
use std::path::PathBuf;

const USO: &str = "uso: hangar-computer-control [agent] --config <arquivo>";

fn servir() -> i32 {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    #[cfg(feature = "motor-fake")]
    let motor = motor::FakeMotor;
    #[cfg(not(feature = "motor-fake"))]
    let motor = motor_real::MotorReal;
    let codigo = match rt.block_on(mcp::servir(motor)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    };
    // Exit with the runtime alive: dropping it waits on the blocking stdin read forever.
    std::process::exit(codigo);
}

/// `agent --config <path>`, or the bare `--config <path>` that existing Windows targets launch.
fn config_do_agente(args: &[OsString]) -> Option<PathBuf> {
    match args {
        [agent, flag, path] if agent == "agent" && flag == "--config" => Some(PathBuf::from(path)),
        [flag, path] if flag == "--config" => Some(PathBuf::from(path)),
        _ => None,
    }
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let codigo = if args.is_empty() {
        servir()
    } else if let Some(config) = config_do_agente(&args) {
        agente::rodar(&config)
    } else {
        eprintln!("{USO}");
        2
    };
    std::process::exit(codigo);
}

#[cfg(test)]
mod tests {
    use super::config_do_agente;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn agent_args() {
        assert_eq!(config_do_agente(&args(&["agent", "--config", "c.json"])), Some(PathBuf::from("c.json")));
        assert_eq!(config_do_agente(&args(&["--config", "c.json"])), Some(PathBuf::from("c.json")));
        for ruim in [&["agent"][..], &["agent", "--config"], &["--config"], &["x", "--config", "c"], &["agent", "c.json"],
            &["--config", "c", "extra"]] {
            assert_eq!(config_do_agente(&args(ruim)), None, "{ruim:?}");
        }
    }
}
