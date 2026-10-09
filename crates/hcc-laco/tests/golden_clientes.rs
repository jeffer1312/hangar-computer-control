//! Prompts and Jev/LLM request bodies against `golden/clientes.json`, captured from laco_uia.py.

use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use hcc_laco::jev::{JEV_URL, Jev};
use hcc_laco::llm::{LLM_URL, Llm};
use hcc_laco::prompts;
use hcc_laco::tipos::{Candidato, Escolha, Intencao};
use hcc_protocolo::Action;
use serde_json::{Map, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const T: Duration = Duration::from_secs(5);

fn golden() -> Value {
    serde_json::from_str(include_str!("golden/clientes.json")).unwrap()
}

/// Answers the scripted responses in order, one per request.
struct Roteiro(Mutex<Vec<Value>>);

impl Respond for Roteiro {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(self.0.lock().unwrap().remove(0))
    }
}

async fn servidor(respostas: &Value) -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(Roteiro(Mutex::new(respostas.as_array().unwrap().clone())))
        .mount(&s)
        .await;
    s
}

/// Every request matches the golden body and carries the key as a bearer header.
async fn confere_pedidos(s: &MockServer, pedidos: &Value) {
    let recebidos = s.received_requests().await.unwrap();
    let pedidos = pedidos.as_array().unwrap();
    assert_eq!(recebidos.len(), pedidos.len());
    for (r, g) in recebidos.iter().zip(pedidos) {
        assert_eq!(r.headers["authorization"], "Bearer KEY");
        assert_eq!(r.headers["content-type"], "application/json");
        let corpo: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(corpo, g["body"]);
    }
}

fn texto(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

fn candidato(c: &Value) -> Candidato {
    let i = &c["intencao"];
    Candidato {
        acao: serde_json::from_value(c["acao"].clone()).unwrap(),
        descricao: c["descricao"].as_str().unwrap().to_owned(),
        intencao: Intencao {
            tipo: texto(&i[0]).unwrap(),
            alvo: texto(&i[1]).unwrap(),
            valor: texto(&i[2]),
            teclas: i[3].as_array().unwrap().iter().map(|k| k.as_str().unwrap().to_owned()).collect(),
            aplicacao: texto(&i[4]),
        },
    }
}

fn escolha(s: &str) -> Escolha {
    match s {
        "DONE" => Escolha::Done,
        "WAIT" => Escolha::Wait,
        "BLOCKED" => Escolha::Blocked,
        n => Escolha::Indice(n.parse().unwrap()),
    }
}

fn perto(a: f64, b: &Value) {
    assert!((a - b.as_f64().unwrap()).abs() < 1e-9, "{a} != {b}");
}

#[test]
fn prompts_byte_equal() {
    let g = golden();
    let p = &g["prompts"];
    let desfechos: Map<String, Value> =
        prompts::DESFECHOS.iter().map(|(k, v)| ((*k).to_owned(), Value::from(*v))).collect();
    assert_eq!(Value::Object(desfechos), p["DESFECHOS"]);
    assert_eq!(prompts::DESFECHOS.map(|d| d.0), ["DONE", "WAIT", "BLOCKED"]);
    assert_eq!(prompts::REGRAS, p["REGRAS"]);
    assert_eq!(prompts::risco_pergunta(), p["RISCO_PERGUNTA"]);
    assert_eq!(prompts::AJUDA, p["AJUDA"]);
    assert_eq!(prompts::AJUDA_SCHEMA, p["AJUDA_SCHEMA"]);
    let torneio = g["decidir"].as_array().unwrap().iter().find(|c| c["nome"] == "trezentos_torneio").unwrap();
    let lote = &torneio["pedidos"][0]["body"]["questions"]["batch_0"]["instructions"];
    assert_eq!(prompts::REGRAS_LOTE, lote);
    assert_eq!(JEV_URL, g["jev_url"]);
    assert_eq!(LLM_URL, g["llm_url"]);
}

#[tokio::test]
async fn decidir_matches_python() {
    let g = golden();
    for caso in g["decidir"].as_array().unwrap() {
        let s = servidor(&caso["respostas"]).await;
        let cands: Vec<Candidato> = caso["candidatos"].as_array().unwrap().iter().map(candidato).collect();
        let d = Jev::new(s.uri(), "KEY".into()).decidir(&caso["estado"], &cands, T).await.unwrap();
        let r = &caso["resultado"];
        assert_eq!(d.escolha, escolha(r["choice"].as_str().unwrap()), "{}", caso["nome"]);
        perto(d.p, &r["probability"]);
        perto(d.risco, &r["risky"]);
        let top: Vec<Value> = d.top.iter().map(|(k, p, desc)| serde_json::json!([k, p, desc])).collect();
        assert_eq!(Value::from(top), r.get("top").cloned().unwrap_or(Value::Array(vec![])), "{}", caso["nome"]);
        confere_pedidos(&s, &caso["pedidos"]).await;
    }
}

#[tokio::test]
async fn arriscado_matches_python() {
    let g = golden();
    let caso = &g["arriscado"];
    let s = servidor(&Value::Array(vec![caso["resposta"].clone()])).await;
    let jev = Jev::new(s.uri(), "KEY".into());
    let p = jev.arriscado(&caso["estado"], caso["proposta"].as_str().unwrap(), T).await.unwrap();
    perto(p, &caso["resultado"]);
    confere_pedidos(&s, &caso["pedidos"]).await;
}

#[tokio::test]
async fn ajudar_matches_python() {
    let g = golden();
    for caso in g["ajudar"].as_array().unwrap() {
        let s = servidor(&Value::Array(vec![caso["resposta"].clone()])).await;
        let llm = Llm::new(s.uri(), texto(&caso["modelo"]).unwrap(), texto(&caso["esforco"]), Some("KEY".into()));
        let png = base64::engine::general_purpose::STANDARD.decode(caso["png"].as_str().unwrap()).unwrap();
        let ajuda = llm.ajudar(&caso["estado"], &png, T).await;
        let r = &caso["resultado"];
        assert_eq!(ajuda.interpretacao, r["interpretacao"], "{}", caso["nome"]);
        assert_eq!(ajuda.impedimento, r["impedimento"]);
        let acoes: Vec<Action> = serde_json::from_value(r["acoes"].clone()).unwrap();
        assert_eq!(ajuda.acoes, acoes);
        confere_pedidos(&s, &caso["pedidos"]).await;
    }
}
