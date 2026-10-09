//! Jev client: one choice + risk request per decision, batches and tournament (laco_uia.py:267-315).

use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::http::{self, prob};
use crate::prompts::{DESFECHOS, REGRAS, REGRAS_LOTE, risco_pergunta};
use crate::tipos::{Candidato, Decisao, Escolha, Intencao};

pub const JEV_URL: &str = "https://api.typesafe.ai/v1/systemone";
pub const JEV_MODEL: &str = "jev-latest";
/// TypeSafe refuses more than 255 options per question; 4 are left for DONE/WAIT/BLOCKED.
pub const LOTE: usize = 251;

pub struct Jev {
    pub url: String,
    pub key: String,
    pub model: String,
    http: reqwest::Client,
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

/// Python `round(p, 2)`: correctly rounded decimal, ties to even on the exact binary value.
fn arredonda(p: f64) -> f64 {
    format!("{p:.2}").parse().unwrap_or(p)
}

impl Jev {
    pub fn new(url: String, key: String, model: String) -> Jev {
        Jev { url, key, model, http: http::cliente() }
    }

    async fn perguntar(&self, state: &Value, questions: Value, timeout: Duration) -> Result<Map<String, Value>, String> {
        let corpo = json!({"model": self.model, "state": state, "questions": questions});
        let mut r = http::post(&self.http, &self.url, &corpo, &self.key, timeout).await.map_err(|f| f.to_string())?;
        match r.get_mut("answers").map(Value::take) {
            Some(Value::Object(a)) => Ok(a),
            Some(v) => Err(sem_get(&v)),
            None => Err("KeyError: 'answers'".into()),
        }
    }

    pub async fn arriscado(&self, estado: &Value, proposta: &str, timeout: Duration) -> Result<f64, String> {
        let mut state = estado.clone();
        if let Value::Object(m) = &mut state {
            m.insert("proposedAction".into(), json!(proposta));
        }
        let r = self.perguntar(&state, json!({"risky": risco_pergunta()}), timeout).await?;
        risco(&r)
    }

    pub async fn decidir(&self, estado: &Value, cands: &[Candidato], timeout: Duration) -> Result<Decisao, String> {
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
            let respostas = self.perguntar(estado, Value::Object(perguntas), timeout).await?;
            let risco = risco(&respostas)?;
            if lotes.len() == 1 {
                return um_lote(respostas.get("action"), cands, risco);
            }
            let mut vencedores = Vec::new();
            for n in 0..lotes.len() {
                let r = respostas.get(&format!("batch_{n}"));
                let opcao = r.and_then(|r| r.get("choice")).and_then(Value::as_str);
                let p = prob(opcao.and_then(|o| r?.get("probabilities")?.get(o)));
                match opcao {
                    Some(o @ ("DONE" | "WAIT")) => {
                        return Ok(Decisao { escolha: desfecho(o)?, p, risco, top: vec![] });
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
                [] => return Ok(Decisao { escolha: Escolha::Blocked, p: 1.0, risco, top: vec![] }),
                [(i, p)] => return Ok(Decisao { escolha: Escolha::Indice(i), p, risco, top: vec![] }),
                _ => restantes = vencedores.iter().map(|&(i, _)| i).collect(),
            }
        }
    }
}

/// Same intent spread over several nodes (ComboBox 'Nome:', Edit 'Nome', an LLM `text` with the same value)
/// splits the vote: sum per intent, pick the best group, then its best node.
fn um_lote(resposta: Option<&Value>, cands: &[Candidato], risco: f64) -> Result<Decisao, String> {
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
    Ok(Decisao { escolha, p: soma(melhor), risco, top })
}
