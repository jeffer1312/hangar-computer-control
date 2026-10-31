//! Tool progress file: the terminal-less Claude does not forward MCP progress, so Hangar reads
//! the steps from `~/.hangar/tool-progress/<toolUseId>.jsonl`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn id_valido(id: &str) -> bool {
    id.strip_prefix("toolu_").is_some_and(|resto| {
        (1..=80).contains(&resto.len())
            && resto.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

pub fn arquivo(tool_use_id: Option<&str>) -> Option<PathBuf> {
    let id = tool_use_id.filter(|id| id_valido(id))?;
    let pasta = std::env::home_dir()?.join(".hangar").join("tool-progress");
    std::fs::create_dir_all(&pasta).ok()?;
    Some(pasta.join(format!("{id}.jsonl")))
}

/// `imagem`: the action's marked print, for Hangar to show next to the step.
pub fn anotar(path: &Path, message: &str, imagem: Option<&Path>) {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let imagem = imagem.map(|i| format!(", \"imagem\": {}", serde_json::Value::from(i.display().to_string()))).unwrap_or_default();
    let linha = format!(
        "{{\"t\": {}, \"message\": {}{imagem}}}\n",
        serde_json::Value::from(t),
        serde_json::Value::from(message)
    );
    // One write per line keeps concurrent appends whole.
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(linha.as_bytes()));
}

#[cfg(test)]
mod tests {
    use super::id_valido;

    #[test]
    fn tool_use_id_shape() {
        assert!(id_valido("toolu_abc-_9"));
        assert!(id_valido(&format!("toolu_{}", "a".repeat(80))));
        for ruim in ["toolu_", "../x", "toolu_a/b", "toolu_a\n", "tooluabc", "toolu_á"] {
            assert!(!id_valido(ruim), "{ruim:?}");
        }
        assert!(!id_valido(&format!("toolu_{}", "a".repeat(81))));
    }
}
