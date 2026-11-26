//! Jev client: one choice + risk request per decision, batches and tournament (laco_uia.py:267-315).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use base64::Engine;
use serde_json::{Map, Value, json};

use crate::http::{self, Falha, prob};
use crate::llm::Imagem;
use crate::prompts::{DESFECHOS, PROPOSTA, REGRAS, REGRAS_LOTE, bloqueio_pergunta, risco_pergunta};
use crate::tipos::{Candidato, Decisao, Envio, Escolha, Intencao};

pub const JEV_URL: &str = "https://api.typesafe.ai/v1/systemone";
pub const JEV_MODEL: &str = "jev-latest";
/// TypeSafe refuses more than 255 options per question; 4 are left for DONE/WAIT/BLOCKED.
pub const LOTE: usize = 251;
/// A request with the screenshot waits at most this long, and only goes with `PRAZO_IMAGEM` left: a retry without
/// it must still fit.
const TETO_IMAGEM: Duration = Duration::from_secs(15);
const PRAZO_IMAGEM: Duration = Duration::from_secs(20);
const BLOQUEIO: &str = "blocked";

pub struct Jev {
    pub url: String,
    pub key: String,
    pub model: String,
    http: reqwest::Client,
    /// A request with the screenshot was refused, or failed in passing twice in a row: later requests go without it.
    sem_imagem: AtomicBool,
    /// Passing failures in a row of requests with the screenshot.
    passageiras: AtomicU32,
}

#[derive(PartialEq)]
enum Grupo<'a> {
    Intencao(&'a Intencao),
    Desfecho(&'a str),
}

fn indice(k: &str) -> Option<usize> {
    (!k.is_empty() && k.bytes().all(|b| b.is_ascii_digit())).then(|| k.parse().unwrap_or(usize::MAX))
}

fn desfecho(k: &str) -> Result<Escolha, String> {
    match k {
        "DONE" => Ok(Escolha::Done),
        "WAIT" => Ok(Escolha::Wait),
        "BLOCKED" => Ok(Escolha::Blocked),
        _ => Err(format!("ValueError: invalid literal for int() with base 10: {}", crate::descrever::py_repr(k))),
    }
}

/// Python `x.get(...)` on a non-dict raises AttributeError: a malformed answer fails closed, never "no risk".
fn sem_get(v: &Value) -> Falha {
    let tipo = match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    };
    Falha::new("AttributeError", format!("'{tipo}' object has no attribute 'get'"))
}

/// `respostas.get("risky", {}).get("noul")` (laco_uia.py:270, :286).
fn risco(r: &Map<String, Value>) -> Result<f64, String> {
    match r.get("risky") {
        None => Ok(0.0),
        Some(Value::Object(m)) => Ok(prob(m.get("noul"))),
        Some(v) => Err(sem_get(v).to_string()),
    }
}

const FORA: &str = "IndexError: list index out of range";

type Top = Vec<(String, f64, String)>;

fn pergunta_escolha(instrucoes: &str, opcoes: &[String]) -> Value {
    let mut criterios = Map::new();
    for (i, o) in opcoes.iter().take(LOTE).enumerate() { criterios.insert(i.to_string(), json!(o)); }
    criterios.insert("NONE".into(), json!("None of the options."));
    json!({"type": "choice", "instructions": instrucoes, "criteria": criterios})
}

fn resposta_escolha(r: Option<&Value>, n: usize) -> Result<(Option<usize>, f64), String> {
    let opcao = r.and_then(|r| r.get("choice")).and_then(Value::as_str);
    let p = prob(opcao.and_then(|o| r?.get("probabilities")?.get(o)));
    match opcao {
        Some("NONE") | None => Ok((None, p)),
        Some(o) => indice(o).filter(|&i| i < n.min(LOTE)).map(|i| (Some(i), p)).ok_or_else(|| FORA.into()),
    }
}

/// Python `round(p, 2)`: correctly rounded decimal, ties to even on the exact binary value.
fn arredonda(p: f64) -> f64 {
    format!("{p:.2}").parse().unwrap_or(p)
}

impl Jev {
    pub fn new(url: String, key: String, model: String) -> Jev {
        Jev { url, key, model, http: http::cliente(), sem_imagem: AtomicBool::new(false), passageiras: AtomicU32::new(0) }
    }

    /// The answers, and what happened to the screenshot (`enviada` when it went with them). `so_com_imagem` are
    /// questions about the screenshot, asked only with it. A request with it that fails is asked again without it.
    async fn perguntar(&self, state: &Value, questions: Map<String, Value>, so_com_imagem: &[(&str, Value)],
        imagem: Option<&Imagem>, timeout: Duration) -> Result<(Map<String, Value>, Envio), String> {
        let mut envio = Envio::default();
        if let Some(imagem) = imagem {
            let mut todas = questions.clone();
            todas.extend(so_com_imagem.iter().map(|(k, v)| ((*k).to_owned(), v.clone())));
            match self.com_imagem(state, todas, imagem, timeout).await {
                Ok(r) => return Ok((r, Envio { imagem: "enviada", motivo: String::new() })),
                Err(e) => envio = e,
            }
        }
        // The retry gets the whole `timeout`, as the request without it always had: a slow Jev that just timed out
        // would fail again in what was left.
        let r = self.pedir(state, &Value::Object(questions), None, timeout).await.map_err(|f| f.to_string())?;
        Ok((r, envio))
    }

    /// The request with the screenshot, when it may go: not once it is off for the run (one refusal or timeout, or
    /// two passing failures in a row), only with `PRAZO_IMAGEM` left, and capped at `TETO_IMAGEM`.
    async fn com_imagem(&self, state: &Value, questions: Map<String, Value>, imagem: &Imagem, timeout: Duration)
        -> Result<Map<String, Value>, Envio> {
        let envio = |imagem, motivo| Envio { imagem, motivo };
        if self.sem_imagem.load(Ordering::Relaxed) { return Err(envio("desligada", String::new())); }
        if timeout < PRAZO_IMAGEM { return Err(envio("sem_prazo", String::new())); }
        match self.pedir(state, &Value::Object(questions), Some(imagem), timeout.min(TETO_IMAGEM)).await {
            Ok(r) => { self.passageiras.store(0, Ordering::Relaxed); Ok(r) }
            Err(e) => {
                if !e.passageira() || self.passageiras.fetch_add(1, Ordering::Relaxed) + 1 >= 2 {
                    self.sem_imagem.store(true, Ordering::Relaxed);
                }
                Err(envio("falhou", e.to_string()))
            }
        }
    }

    async fn pedir(&self, state: &Value, questions: &Value, imagem: Option<&Imagem>, timeout: Duration) -> Result<Map<String, Value>, Falha> {
        let mut corpo = json!({"model": self.model, "state": state, "questions": questions});
        let mut cabecalhos: &[(&str, &str)] = &[];
        if let Some(imagem) = imagem {
            let Value::Object(m) = &mut corpo["state"] else { return Err(Falha::new("TypeError", "state sem objeto para a imagem")) };
            let b64 = base64::engine::general_purpose::STANDARD.encode(&imagem.png);
            // OpenRouter silently drops `assets`: there the image only counts as an `image_url` part in `state`.
            if self.url.contains("openrouter.ai") {
                m.insert("tela".into(), json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{b64}")}}));
            } else {
                let r = format!("img-{}", uuid::Uuid::new_v4());
                m.insert("tela".into(), json!(r));
                let mut assets = Map::new();
                assets.insert(r, json!({"type": "image", "data_base64": b64, "format": "png"}));
                corpo["assets"] = Value::Object(assets);
                cabecalhos = &[("TypeSafe-Danger-Zone-Images", "true")];
            }
        }
        let mut r = http::post(&self.http, &self.url, &corpo, &self.key, cabecalhos, timeout).await?;
        match r.get_mut("answers").map(Value::take) {
            Some(Value::Object(a)) => Ok(a),
            Some(v) => Err(sem_get(&v)),
            None => Err(Falha::new("KeyError", "'answers'")),
        }
    }

    /// The risk, and what happened to the screenshot.
    pub async fn arriscado(&self, estado: &Value, proposta: &str, imagem: Option<&Imagem>, timeout: Duration) -> Result<(f64, Envio), String> {
        let mut state = estado.clone();
        if let Value::Object(m) = &mut state {
            m.insert("proposedAction".into(), json!(proposta));
        }
        let questions = Map::from_iter([("risky".to_owned(), risco_pergunta())]);
        let (r, envio) = self.perguntar(&state, questions, &[], imagem, timeout).await?;
        Ok((risco(&r)?, envio))
    }

    /// One `choice` among `opcoes` plus NONE: the index picked (None for NONE) and its probability.
    pub async fn escolher(&self, estado: &Value, instrucoes: &str, opcoes: &[String], imagem: Option<&Imagem>, timeout: Duration) -> Result<(Option<usize>, f64), String> {
        let questions = Map::from_iter([("pick".to_owned(), pergunta_escolha(instrucoes, opcoes))]);
        let (r, _) = self.perguntar(estado, questions, &[], imagem, timeout).await?;
        resposta_escolha(r.get("pick"), opcoes.len())
    }

    /// Which of the helper's `propostas` is the next step, seeing the screenshot: the index (None for NONE) and its
    /// probability. Nothing without the screenshot (turned off, no time left: the pick would be pointless); a failed
    /// request turns it off like any other.
    pub async fn escolher_proposta(&self, estado: &Value, imagem: &Imagem, propostas: &[String], timeout: Duration)
        -> (Envio, Option<(Option<usize>, f64)>) {
        let perguntas = Map::from_iter([("pick".to_owned(), pergunta_escolha(PROPOSTA, propostas))]);
        let r = match self.com_imagem(estado, perguntas, imagem, timeout).await {
            Ok(r) => r,
            Err(e) => return (e, None),
        };
        match resposta_escolha(r.get("pick"), propostas.len()) {
            Ok(escolha) => (Envio { imagem: "enviada", motivo: String::new() }, Some(escolha)),
            Err(e) => (Envio { imagem: "falhou", motivo: e }, None),
        }
    }

    pub async fn decidir(&self, estado: &Value, cands: &[Candidato], imagem: Option<&Imagem>, timeout: Duration) -> Result<Decisao, String> {
        let mut restantes: Vec<usize> = (0..cands.len()).collect();
        loop {
            let mut lotes: Vec<&[usize]> = restantes.chunks(LOTE).collect();
            if lotes.is_empty() {
                lotes.push(&[]);
            }
            let mut perguntas = Map::new();
            for (n, lote) in lotes.iter().enumerate() {
                let mut criterios = Map::new();
                for &i in *lote {
                    criterios.insert(i.to_string(), json!(cands[i].descricao));
                }
                for (k, v) in DESFECHOS {
                    criterios.insert(k.into(), json!(v));
                }
                let (nome, regras) = if lotes.len() == 1 { ("action".to_owned(), REGRAS) } else { (format!("batch_{n}"), REGRAS_LOTE) };
                perguntas.insert(nome, json!({"type": "choice", "instructions": regras, "criteria": criterios}));
            }
            perguntas.insert("risky".into(), risco_pergunta());
            let (respostas, envio) = self.perguntar(estado, perguntas, &[(BLOQUEIO, bloqueio_pergunta())], imagem, timeout).await?;
            let risco = risco(&respostas)?;
            let com_imagem = envio.imagem == "enviada";
            let blocked = respostas.get(BLOQUEIO).and_then(|b| b.get("noul")).map(|v| prob(Some(v)));
            let decisao = |escolha, p, top| Decisao { escolha, p, risco, top, com_imagem, envio: envio.clone(), blocked };
            if lotes.len() == 1 {
                let (escolha, p, top) = um_lote(respostas.get("action"), cands)?;
                return Ok(decisao(escolha, p, top));
            }
            let mut vencedores = Vec::new();
            for n in 0..lotes.len() {
                let r = respostas.get(&format!("batch_{n}"));
                let opcao = r.and_then(|r| r.get("choice")).and_then(Value::as_str);
                let p = prob(opcao.and_then(|o| r?.get("probabilities")?.get(o)));
                match opcao {
                    Some(o @ ("DONE" | "WAIT")) => {
                        return Ok(decisao(desfecho(o)?, p, vec![]));
                    }
                    Some(o) => {
                        if let Some(i) = indice(o) {
                            vencedores.push((i, p));
                        }
                    }
                    None => {}
                }
            }
            if vencedores.iter().any(|&(i, _)| i >= cands.len()) {
                return Err(FORA.into());
            }
            match vencedores[..] {
                [] => return Ok(decisao(Escolha::Blocked, 1.0, vec![])),
                [(i, p)] => return Ok(decisao(Escolha::Indice(i), p, vec![])),
                _ => restantes = vencedores.iter().map(|&(i, _)| i).collect(),
            }
        }
    }
}

/// Same intent spread over several nodes (ComboBox 'Nome:', Edit 'Nome', an LLM `text` with the same value)
/// splits the vote: sum per intent, pick the best group, then its best node.
fn um_lote(resposta: Option<&Value>, cands: &[Candidato]) -> Result<(Escolha, f64, Top), String> {
    let probs: Vec<(&str, f64)> = resposta
        .and_then(|r| r.get("probabilities"))
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.as_str(), prob(Some(v)))).collect())
        .unwrap_or_default();
    let mut grupos: Vec<(Grupo, Vec<(f64, &str)>)> = Vec::new();
    for &(k, p) in &probs {
        let g = match indice(k) {
            Some(i) => Grupo::Intencao(&cands.get(i).ok_or(FORA)?.intencao),
            None => Grupo::Desfecho(k),
        };
        match grupos.iter_mut().find(|(x, _)| *x == g) {
            Some((_, v)) => v.push((p, k)),
            None => grupos.push((g, vec![(p, k)])),
        }
    }
    let soma = |v: &[(f64, &str)]| v.iter().map(|x| x.0).sum::<f64>();
    // Python max() keeps the FIRST maximal group.
    let mut melhor: Option<&Vec<(f64, &str)>> = None;
    for (_, v) in &grupos {
        if melhor.is_none_or(|m| soma(v) > soma(m)) {
            melhor = Some(v);
        }
    }
    let melhor = melhor.ok_or("ValueError: max() iterable argument is empty")?;
    let opcao = melhor
        .iter()
        .max_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(b.1)))
        .map(|x| x.1)
        .unwrap_or_default();
    let escolha = match indice(opcao) {
        Some(i) => Escolha::Indice(i),
        None => desfecho(opcao)?,
    };
    let mut top = probs.clone();
    top.sort_by(|a, b| b.1.total_cmp(&a.1));
    let top = top
        .into_iter()
        .take(5)
        .map(|(k, p)| {
            let desc = indice(k).map_or_else(|| k.to_owned(), |i| cands[i].descricao.clone());
            (k.to_owned(), arredonda(p), desc)
        })
        .collect();
    Ok((escolha, soma(melhor), top))
}
