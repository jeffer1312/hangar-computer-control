//! Jev client: one choice + risk request per decision, batches and tournament (laco_uia.py:267-315).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use serde_json::{Map, Value, json};

use crate::http::{self, prob};
use crate::llm::Imagem;
use crate::prompts::{DESFECHOS, PROPOSTA, REGRAS, REGRAS_LOTE, bloqueio_pergunta, risco_pergunta};
use crate::tipos::{Candidato, Decisao, Escolha, Intencao};

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
    /// A request with the screenshot failed once: later requests go without it.
    sem_imagem: AtomicBool,
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
fn sem_get(v: &Value) -> String {
    let tipo = match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    };
    format!("AttributeError: '{tipo}' object has no attribute 'get'")
}

/// `respostas.get("risky", {}).get("noul")` (laco_uia.py:270, :286).
fn risco(r: &Map<String, Value>) -> Result<f64, String> {
    match r.get("risky") {
        None => Ok(0.0),
        Some(Value::Object(m)) => Ok(prob(m.get("noul"))),
        Some(v) => Err(sem_get(v)),
    }
}

const FORA: &str = "IndexError: list index out of range";

type Top = Vec<(String, f64, String)>;
/// Center of a control in screenshot pixels, and Jev's confidence in it.
pub type Ponto = (f64, f64, f64);
/// Jev's pick among the proposals (index, probability) and, per proposal, where its labelled control is.
pub type Localizacao = ((Option<usize>, f64), Vec<Option<Ponto>>);

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

/// A `box` answer's center in screenshot pixels and its confidence; the edges come as fractions of the screenshot.
fn ponto(r: &Value, imagem: &Imagem) -> Option<Ponto> {
    let lado = |k: &str| r.get("box")?.get(k)?.as_f64().filter(|v| (0.0..=1.0).contains(v));
    let (esquerda, topo, direita, base) = (lado("left")?, lado("top")?, lado("right")?, lado("bottom")?);
    Some(((esquerda + direita) / 2.0 * imagem.largura as f64, (topo + base) / 2.0 * imagem.altura as f64, prob(r.get("confidence"))))
}

/// What happened to the screenshot passed with a request: `enviada`, `falhou` (with the error in `motivo`),
/// `desligada`, `sem_prazo`; empty without one.
#[derive(Debug)]
pub struct Envio {
    pub imagem: &'static str,
    pub motivo: String,
}

/// Python `round(p, 2)`: correctly rounded decimal, ties to even on the exact binary value.
fn arredonda(p: f64) -> f64 {
    format!("{p:.2}").parse().unwrap_or(p)
}

impl Jev {
    pub fn new(url: String, key: String, model: String) -> Jev {
        Jev { url, key, model, http: http::cliente(), sem_imagem: AtomicBool::new(false) }
    }

    /// The answers, and what happened to the screenshot (`enviada` when it went with them). A request with it that
    /// fails is asked again without it, and the screenshot stays off for the rest of the run.
    async fn perguntar(&self, state: &Value, questions: Value, imagem: Option<&Imagem>, timeout: Duration)
        -> Result<(Map<String, Value>, Envio), String> {
        let mut envio = Envio { imagem: "", motivo: String::new() };
        if let Some(imagem) = imagem {
            if self.sem_imagem.load(Ordering::Relaxed) {
                envio.imagem = "desligada";
            } else if timeout < PRAZO_IMAGEM {
                envio.imagem = "sem_prazo";
            } else {
                match self.pedir(state, &questions, Some(imagem), timeout.min(TETO_IMAGEM)).await {
                    Ok(r) => return Ok((r, Envio { imagem: "enviada", motivo: String::new() })),
                    Err(e) => {
                        self.sem_imagem.store(true, Ordering::Relaxed);
                        envio = Envio { imagem: "falhou", motivo: e };
                    }
                }
            }
        }
        // Without the screenshot, questions about it have nothing to look at. The retry gets the whole `timeout`, as
        // the request without it always had: a slow Jev that just timed out would fail again in what was left.
        let mut questions = questions;
        if let Value::Object(q) = &mut questions { q.remove(BLOQUEIO); }
        let r = self.pedir(state, &questions, None, timeout).await?;
        Ok((r, envio))
    }

    async fn pedir(&self, state: &Value, questions: &Value, imagem: Option<&Imagem>, timeout: Duration) -> Result<Map<String, Value>, String> {
        let mut corpo = json!({"model": self.model, "state": state, "questions": questions});
        let mut cabecalhos: &[(&str, &str)] = &[];
        if let Some(imagem) = imagem {
            let Value::Object(m) = &mut corpo["state"] else { return Err("TypeError: state sem objeto para a imagem".into()) };
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
        let mut r = http::post(&self.http, &self.url, &corpo, &self.key, cabecalhos, timeout).await.map_err(|f| f.to_string())?;
        match r.get_mut("answers").map(Value::take) {
            Some(Value::Object(a)) => Ok(a),
            Some(v) => Err(sem_get(&v)),
            None => Err("KeyError: 'answers'".into()),
        }
    }

    /// The risk, and what happened to the screenshot.
    pub async fn arriscado(&self, estado: &Value, proposta: &str, imagem: Option<&Imagem>, timeout: Duration) -> Result<(f64, Envio), String> {
        let mut state = estado.clone();
        if let Value::Object(m) = &mut state {
            m.insert("proposedAction".into(), json!(proposta));
        }
        let (r, envio) = self.perguntar(&state, json!({"risky": risco_pergunta()}), imagem, timeout).await?;
        Ok((risco(&r)?, envio))
    }

    /// One `choice` among `opcoes` plus NONE: the index picked (None for NONE) and its probability.
    pub async fn escolher(&self, estado: &Value, instrucoes: &str, opcoes: &[String], imagem: Option<&Imagem>, timeout: Duration) -> Result<(Option<usize>, f64), String> {
        let (r, _) = self.perguntar(estado, json!({"pick": pergunta_escolha(instrucoes, opcoes)}), imagem, timeout).await?;
        resposta_escolha(r.get("pick"), opcoes.len())
    }

    /// One request over the screenshot: which of `propostas` (description, label of the control a click aims at)
    /// is the next step, and where each labelled control is on the screenshot. Nothing without the screenshot
    /// (turned off, no time left: the request would be pointless); a failed request turns it off like any other.
    pub async fn localizar_e_escolher(&self, estado: &Value, imagem: &Imagem, propostas: &[(String, Option<String>)],
        timeout: Duration) -> (Envio, Option<Localizacao>) {
        let envio = |imagem, motivo| Envio { imagem, motivo };
        if self.sem_imagem.load(Ordering::Relaxed) { return (envio("desligada", String::new()), None); }
        if timeout < PRAZO_IMAGEM { return (envio("sem_prazo", String::new()), None); }
        let opcoes: Vec<String> = propostas.iter().map(|(d, _)| d.clone()).collect();
        let mut perguntas = Map::new();
        perguntas.insert("pick".into(), pergunta_escolha(PROPOSTA, &opcoes));
        for (i, (_, rotulo)) in propostas.iter().enumerate() {
            if let Some(r) = rotulo {
                perguntas.insert(format!("box_{i}"), json!({"type": "box", "instructions": format!("{r} on the screenshot `tela`")}));
            }
        }
        let lido = async {
            let r = self.pedir(estado, &Value::Object(perguntas), Some(imagem), timeout.min(TETO_IMAGEM)).await?;
            let pontos = (0..propostas.len()).map(|i| r.get(&format!("box_{i}")).and_then(|b| ponto(b, imagem))).collect();
            Ok::<_, String>((resposta_escolha(r.get("pick"), opcoes.len())?, pontos))
        }.await;
        match lido {
            Ok(l) => (envio("enviada", String::new()), Some(l)),
            Err(e) => {
                self.sem_imagem.store(true, Ordering::Relaxed);
                (envio("falhou", e), None)
            }
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
            perguntas.insert(BLOQUEIO.into(), bloqueio_pergunta());
            let (respostas, envio) = self.perguntar(estado, Value::Object(perguntas), imagem, timeout).await?;
            let risco = risco(&respostas)?;
            let com_imagem = envio.imagem == "enviada";
            let blocked = respostas.get(BLOQUEIO).and_then(|b| b.get("noul")).filter(|_| com_imagem).map(|v| prob(Some(v)));
            let decisao = |escolha, p, top| Decisao { escolha, p, risco, top, com_imagem, imagem: envio.imagem,
                motivo: envio.motivo.clone(), blocked };
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
