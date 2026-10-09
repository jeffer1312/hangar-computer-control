//! hangar-computer-control: MCP server by default, desktop agent with `agent --config <path>`.

// Only the fake engine drives the MCP until Task 10 wires the real one.
#[cfg_attr(not(feature = "motor-fake"), allow(dead_code))]
mod mcp;
#[cfg_attr(not(feature = "motor-fake"), allow(dead_code))]
mod alvos;
#[cfg_attr(not(feature = "motor-fake"), allow(dead_code))]
mod progresso;
#[cfg_attr(not(feature = "motor-fake"), allow(dead_code))]
mod motor;
mod motor_real;
mod agente;

fn main() {
    #[cfg(feature = "motor-fake")]
    if std::env::args_os().len() == 1 {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };
        let codigo = match rt.block_on(mcp::servir(motor::FakeMotor)) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{e}");
                1
            }
        };
        // Exit with the runtime alive: dropping it waits on the blocking stdin read forever.
        std::process::exit(codigo);
    }
    eprintln!("motor real ainda não ligado");
    std::process::exit(1);
}
