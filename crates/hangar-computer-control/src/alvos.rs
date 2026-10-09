//! Target resolution: `<name>-agent.json` files in the targets dir, plus the automatic `linux`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

const SUFIXO: &str = "-agent.json";

fn env_nao_vazio(nome: &str) -> Option<OsString> {
    std::env::var_os(nome).filter(|v| !v.is_empty())
}

pub fn alvos() -> Vec<(String, PathBuf)> {
    let pasta = env_nao_vazio("HCC_AGENTS_DIR").map(PathBuf::from).or_else(|| {
        let config = PathBuf::from(env_nao_vazio("HCC_AGENT_CONFIG")?);
        config.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf)
    });
    let mut encontrados: Vec<(String, PathBuf)> = pasta
        .and_then(|p| std::fs::read_dir(p).ok())
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let e = e.ok()?;
            let nome = e.file_name().into_string().ok()?;
            Some((nome.strip_suffix(SUFIXO)?.to_owned(), e.path()))
        })
        .collect();
    encontrados.sort_by(|a, b| a.1.cmp(&b.1));
    if !encontrados.iter().any(|(n, _)| n == "linux")
        && let Some(local) = linux_automatico()
    {
        encontrados.push(("linux".to_owned(), local));
    }
    encontrados
}

pub fn config_do(alvo: Option<&str>) -> Result<Option<PathBuf>, String> {
    let Some(alvo) = alvo.filter(|a| !a.is_empty()) else {
        return Ok(env_nao_vazio("HCC_AGENT_CONFIG").map(PathBuf::from).or_else(linux_automatico));
    };
    let disponiveis = alvos();
    match disponiveis.iter().find(|(n, _)| n == alvo) {
        Some((_, p)) => Ok(Some(p.clone())),
        None => {
            let nomes: Vec<&str> = disponiveis.iter().map(|(n, _)| n.as_str()).collect();
            let lista = if nomes.is_empty() { "nenhum".to_owned() } else { nomes.join(", ") };
            Err(format!("alvo desconhecido: {alvo}; disponíveis: {lista}"))
        }
    }
}

pub fn padrao() -> String {
    let nome = env_nao_vazio("HCC_AGENT_CONFIG")
        .and_then(|c| Some(Path::new(&c).file_name()?.to_string_lossy().into_owned()))
        .map(|n| n.strip_suffix(SUFIXO).map(str::to_owned).unwrap_or(n))
        .filter(|n| !n.is_empty());
    match nome {
        Some(n) => n,
        None if linux_automatico().is_some() => "linux".to_owned(),
        None => "nenhum".to_owned(),
    }
}

fn linux_automatico() -> Option<PathBuf> {
    linux_local(&std::env::current_exe().ok()?)
}

/// Under Hyprland the running binary is the `linux` target: nobody has to create a file.
pub fn linux_local(exe: &Path) -> Option<PathBuf> {
    if !cfg!(target_os = "linux") || env_nao_vazio("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return None;
    }
    // `.v2`: the Python MCP writes `linux-agent.json` with its own command; both coexist.
    let caminho = std::env::home_dir()?.join(".cache/hangar-computer-control/linux-agent.v2.json");
    let Some(exe) = exe.to_str() else {
        eprintln!("alvo linux indisponível: caminho do executável não é UTF-8: {}", exe.display());
        return None;
    };
    let conteudo = format!(
        "{{\"transport\":\"local\",\"request_timeout\":15,\"command\":[{},\"agent\"]}}",
        serde_json::Value::from(exe)
    );
    let gravar = || -> std::io::Result<()> {
        if std::fs::read_to_string(&caminho).is_ok_and(|atual| atual == conteudo) {
            return Ok(());
        }
        let pasta = caminho.parent().expect("caminho tem pasta");
        std::fs::create_dir_all(pasta)?;
        // Atomic swap: another session reading it never sees half a file.
        let temporario = pasta.join(format!(".linux-agent.v2.json.{}", std::process::id()));
        let r = std::fs::write(&temporario, &conteudo).and_then(|()| std::fs::rename(&temporario, &caminho));
        if r.is_err() {
            let _ = std::fs::remove_file(&temporario);
        }
        r
    };
    match gravar() {
        Ok(()) => Some(caminho),
        Err(e) => {
            // Runs at startup: failing here must not take the other targets down.
            eprintln!("alvo linux indisponível: não gravei {}: {e}", caminho.display());
            None
        }
    }
}
