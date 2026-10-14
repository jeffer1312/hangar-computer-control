//! Vision LLM fallback: interprets the screen and proposes actions; Jev still decides (laco_uia.py:318-338).

use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};

use crate::http::{self, Falha, py_dumps};
use crate::prompts::{AJUDA, AJUDA_SCHEMA};
use crate::tipos::Ajuda;
use hcc_protocolo::Action;

pub const LLM_URL: &str = "http://127.0.0.1:8317/v1/chat/completions";
pub const LLM_MODELO: &str = "gpt-5.6-luna";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

pub struct Llm {
    pub url: String,
    pub model: String,
    pub effort: Option<String>,
    pub key: Option<String>,
    http: reqwest::Client,
}

fn be(b: &[u8]) -> u64 {
    b.iter().fold(0, |n, &x| (n << 8) | u64::from(x))
}

impl Llm {
    pub fn new(url: String, model: String, effort: Option<String>, key: Option<String>) -> Llm {
        Llm { url, model, effort, key, http: http::cliente() }
    }

    /// Unset URL/model fall back to the defaults; empty effort or key count as absent.
    pub fn from_env() -> Llm {
        let var = |k| std::env::var(k).ok();
        let cheio = |k| var(k).filter(|v: &String| !v.is_empty());
        Llm::new(
            var("LLM_PROXY_URL").unwrap_or_else(|| LLM_URL.into()),
            var("LLM_MODEL").unwrap_or_else(|| LLM_MODELO.into()),
            cheio("LLM_EFFORT"),
            cheio("LLM_PROXY_KEY"),
        )
    }

    /// Never fails: any error becomes the hint `ajuda visual indisponível: <Tipo>: <msg[:300]>`.
    pub async fn ajudar(&self, estado: &Value, png: &[u8], timeout: Duration) -> Ajuda {
        self.pedir(estado, png, timeout).await.unwrap_or_else(|f| Ajuda {
            interpretacao: format!("ajuda visual indisponível: {}: {}", f.tipo, f.msg.chars().take(300).collect::<String>()),
            acoes: vec![],
            impedimento: String::new(),
        })
    }

    pub(crate) async fn pedir(&self, estado: &Value, png: &[u8], timeout: Duration) -> Result<Ajuda, Falha> {
        let chave = self.key.as_deref().filter(|k| !k.is_empty()).ok_or_else(|| {
            Falha::new("RuntimeError", "falta LLM_PROXY_KEY no ambiente para o fallback de interpretação")
        })?;
        let mut estado = estado.clone();
        if png.starts_with(PNG)
            && let Value::Object(m) = &mut estado
        {
            let parte = |a: usize, b: usize| be(png.get(a..b.min(png.len())).unwrap_or_default());
            m.insert("imageSize".into(), json!({"width": parte(16, 20), "height": parte(20, 24)}));
        }
        let mut conteudo = vec![json!({"type": "text", "text": py_dumps(&estado)})];
        if !png.is_empty() {
            let url = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png));
            conteudo.push(json!({"type": "image_url", "image_url": {"url": url}}));
        }
        let mut mensagens = vec![
            json!({"role": "system", "content": format!("{AJUDA}{AJUDA_SCHEMA}")}),
            json!({"role": "user", "content": conteudo})];
        // A reply without the object is asked once more, quoting it back.
        let mut recusadas = Vec::new();
        for tentativa in 0..2 {
            let mut corpo = json!({"model": self.model, "messages": mensagens});
            if let Some(e) = self.effort.as_deref().filter(|e| !e.is_empty()) {
                corpo["reasoning_effort"] = json!(e);
            }
            let r = http::post(&self.http, &self.url, &corpo, chave, timeout).await?;
            let campo = |v: &Value, k: &str| v.get(k).cloned().ok_or_else(|| Falha::new("KeyError", format!("'{k}'")));
            let primeira = campo(&r, "choices")?
                .get(0)
                .cloned()
                .ok_or_else(|| Falha::new("IndexError", "list index out of range"))?;
            let conteudo = campo(&campo(&primeira, "message")?, "content")?;
            let bruto = conteudo
                .as_str()
                .ok_or_else(|| Falha::new("AttributeError", "'NoneType' object has no attribute 'strip'"))?
                .trim();
            if let Some(ajuda) = ler_ajuda(bruto) { return Ok(ajuda); }
            recusadas.push(bruto.chars().take(200).collect::<String>());
            if tentativa == 1 { break; }
            mensagens.push(json!({"role": "assistant", "content": bruto}));
            mensagens.push(json!({"role": "user", "content": "Reply again with only the JSON object of the schema, nothing else."}));
        }
        Err(Falha::new("ValidationError", format!("resposta sem o objeto JSON pedido: {}", recusadas.join(" | "))))
    }
}

const CAMPOS: &[&str] = &["type", "target", "value", "keys", "application", "args", "direction", "amount", "x", "y",
    "button", "mode", "x2", "y2", "delta", "rotulo"];

/// The first `{` of the reply that holds `interpretacao`, wherever the model put it (after a sentence, in a ```
/// block). Unknown fields are ignored, a mouse action missing button/mode gets left/click (scroll with a delta),
/// and the list stops at the first action that still does not validate (what follows depended on it), with a note
/// in `interpretacao`; at most 6 are kept.
fn ler_ajuda(bruto: &str) -> Option<Ajuda> {
    let objeto = bruto.match_indices('{').find_map(|(i, _)| {
        serde_json::Deserializer::from_str(&bruto[i..]).into_iter::<Value>().next()?.ok()
            .filter(|v| v.get("interpretacao").is_some_and(Value::is_string))
    })?;
    let texto = |k: &str| objeto.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let lidas: Vec<Result<Action, String>> = objeto.get("acoes").and_then(Value::as_array).into_iter().flatten().map(|a| {
        let mut a: serde_json::Map<String, Value> = a.as_object().ok_or("ação não é um objeto")?.iter()
            .filter(|(k, v)| CAMPOS.contains(&k.as_str()) && !v.is_null()).map(|(k, v)| (k.clone(), v.clone())).collect();
        if a.get("type").and_then(Value::as_str) == Some("mouse") {
            a.entry("button").or_insert_with(|| json!("left"));
            let modo = if a.contains_key("delta") { "scroll" } else { "click" };
            a.entry("mode").or_insert_with(|| json!(modo));
        }
        let acao: Action = serde_json::from_value(Value::Object(a)).map_err(|e| e.to_string())?;
        acao.validate().map(|()| acao)
    }).collect();
    let validas = lidas.iter().take_while(|a| a.is_ok()).count();
    let mut interpretacao: String = texto("interpretacao").chars().take(1500).collect();
    if let Some(Err(motivo)) = lidas.get(validas) {
        interpretacao.push_str(&format!(" [{} ação(ões) da ajuda descartada(s): {motivo}]", lidas.len() - validas));
    }
    let acoes = lidas.into_iter().take(validas.min(6)).flatten().collect();
    Some(Ajuda { interpretacao, acoes, impedimento: texto("impedimento") })
}
