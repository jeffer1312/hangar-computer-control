//! Vision LLM fallback: interprets the screen and proposes actions; Jev still decides (laco_uia.py:318-338).

use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};

use crate::http::{self, Falha, py_dumps};
use crate::prompts::{AJUDA, AJUDA_SCHEMA};
use crate::tipos::Ajuda;

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

fn validar(a: &Ajuda) -> Result<(), String> {
    if a.interpretacao.chars().count() > 1500 {
        return Err("interpretacao com mais de 1500 caracteres".into());
    }
    if a.acoes.len() > 6 {
        return Err("acoes com mais de 6 itens".into());
    }
    a.acoes.iter().try_for_each(|x| x.validate())
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
        let mut corpo = json!({"model": self.model, "messages": [
            {"role": "system", "content": format!("{AJUDA}{AJUDA_SCHEMA}")},
            {"role": "user", "content": conteudo}]});
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
        let bruto = match bruto.strip_prefix("```") {
            Some(_) => {
                let (_, resto) = bruto.split_once('\n').ok_or_else(|| Falha::new("IndexError", "list index out of range"))?;
                resto.rsplit_once("```").map_or(resto, |(antes, _)| antes).trim()
            }
            None => bruto,
        };
        let ajuda: Ajuda = serde_json::from_str(bruto).map_err(|e| Falha::new("ValidationError", e.to_string()))?;
        validar(&ajuda).map_err(|e| Falha::new("ValidationError", e))?;
        Ok(ajuda)
    }
}
