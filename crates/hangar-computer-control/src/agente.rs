//! `agent --config <path>`: serves this machine's desktop to the controller that started it.

use std::path::Path;

use hcc_protocolo::Desktop;

pub fn rodar(config: &Path) -> i32 {
    #[cfg(feature = "fake-desktop")]
    if std::env::var_os("HCC_FAKE_DESKTOP").is_some() {
        return servir(hcc_agente::fake::FakeDesktop::from_env(), config);
    }
    real(config)
}

#[cfg(target_os = "linux")]
fn real(config: &Path) -> i32 {
    servir(hcc_agente_linux::LinuxDesktop::new(), config)
}

#[cfg(windows)]
fn real(config: &Path) -> i32 {
    servir(hcc_agente_windows::WindowsDesktop::new(), config)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn real(config: &Path) -> i32 {
    falhar(config, "sistema sem agente de desktop")
}

fn servir<D: Desktop>(desktop: Result<D, String>, config: &Path) -> i32 {
    match desktop {
        Ok(d) => hcc_agente::run(d, config),
        Err(e) => falhar(config, &e),
    }
}

/// Same exit as `hcc_agente::run`'s errors: the credential leaves the disk, the reason goes to
/// stderr and to `<config>.error.log`, where the controller reads it.
fn falhar(config: &Path, msg: &str) -> i32 {
    eprintln!("{msg}");
    if let Err(e) = std::fs::remove_file(config) {
        eprintln!("{}: {e}", config.display());
    }
    let log = config.with_extension("error.log");
    if let Err(e) = std::fs::write(&log, msg) {
        eprintln!("{}: {e}", log.display());
    }
    1
}
