//! Every pure loop function against `golden/puro.json`, captured from laco_uia.py.

use std::collections::HashSet;

use hcc_laco::barreiras::fecha_janela;
use hcc_laco::candidatos::{candidatos, segredos};
use hcc_laco::descrever::{descrever, intencao};
use hcc_laco::estado::{assinatura, estado_compacto, sem_controles};
use hcc_laco::geometria::{dentro, para_tela};
use hcc_laco::tipos::{Intencao, Registro};
use hcc_protocolo::{Action, Observation, Rect};
use serde_json::{Map, Value};

fn golden() -> Value {
    serde_json::from_str(include_str!("golden/puro.json")).unwrap()
}

fn acao(v: &Value) -> Action {
    serde_json::from_value(v.clone()).unwrap()
}

fn intencao_golden(v: &Value) -> Intencao {
    let t = |i: usize| v[i].as_str().map(str::to_owned);
    Intencao {
        tipo: t(0).unwrap(),
        alvo: t(1).unwrap(),
        valor: t(2),
        teclas: v[3].as_array().unwrap().iter().map(|k| k.as_str().unwrap().to_owned()).collect(),
        aplicacao: t(4),
    }
}

fn registro(v: &Value) -> Registro {
    Registro {
        action: v["action"].as_str().unwrap().to_owned(),
        result: v["result"].as_str().unwrap().to_owned(),
        screen_changed: v["screenChanged"].as_bool(),
        target_rect: v.get("target_rect").map(|r| serde_json::from_value::<Rect>(r.clone()).unwrap()),
        clique: v.get("clique").map(|c| serde_json::from_value(c.clone()).unwrap()),
        executada: v["action"] != "WAIT",
    }
}

struct Contexto {
    dados: Map<String, Value>,
    segredos: HashSet<String>,
    recentes: Vec<Registro>,
}

fn contexto(g: &Value) -> Contexto {
    let dados = g["dados"].as_object().unwrap().clone();
    let segredos = segredos(&dados);
    Contexto { dados, segredos, recentes: g["recentes"].as_array().unwrap().iter().map(registro).collect() }
}

fn fixtures(g: &Value) -> impl Iterator<Item = (&str, Observation, &Value)> {
    g["fixtures"].as_array().unwrap().iter().map(|f| {
        (f["nome"].as_str().unwrap(), serde_json::from_value(f["obs"].clone()).unwrap(), f)
    })
}

#[test]
fn segredos_are_the_secret_values_as_python_str() {
    let g = golden();
    let esperado: HashSet<String> =
        g["segredos"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_owned()).collect();
    assert_eq!(contexto(&g).segredos, esperado);
}

#[test]
fn candidates_descriptions_intents_and_close_flags() {
    let g = golden();
    let c = contexto(&g);
    for (nome, obs, f) in fixtures(&g) {
        let got = candidatos(&obs, &c.dados, &c.segredos);
        let want = f["candidatos"].as_array().unwrap();
        assert_eq!(got.len(), want.len(), "{nome}: candidate count");
        for (i, (cand, w)) in got.iter().zip(want).enumerate() {
            assert_eq!(cand.acao, acao(&w["acao"]), "{nome}[{i}] acao");
            assert_eq!(cand.descricao, w["descricao"], "{nome}[{i}] descricao");
            assert_eq!(descrever(&cand.acao, &obs, &c.segredos, false), w["sem_valor"], "{nome}[{i}] sem_valor");
            assert_eq!(cand.intencao, intencao_golden(&f["intencoes"][i]), "{nome}[{i}] intencao");
            assert_eq!(intencao(&cand.acao, &obs), cand.intencao, "{nome}[{i}] intencao()");
            assert_eq!(fecha_janela(&cand.acao, &obs), f["fecha_janela"][i], "{nome}[{i}] fecha_janela");
        }
    }
}

#[test]
fn extra_actions_descriptions() {
    let g = golden();
    let c = contexto(&g);
    for (nome, obs, f) in fixtures(&g) {
        for (i, e) in f["extras"].as_array().unwrap().iter().enumerate() {
            let a = acao(&e["acao"]);
            assert_eq!(descrever(&a, &obs, &c.segredos, true), e["descricao"], "{nome} extra {i}");
            assert_eq!(descrever(&a, &obs, &c.segredos, false), e["sem_valor"], "{nome} extra {i} sem_valor");
            assert_eq!(intencao(&a, &obs), intencao_golden(&e["intencao"]), "{nome} extra {i} intencao");
            assert_eq!(fecha_janela(&a, &obs), e["fecha_janela"], "{nome} extra {i} fecha_janela");
        }
    }
}

#[test]
fn signature_is_foreground_and_sorted_multiset() {
    let g = golden();
    for (nome, obs, f) in fixtures(&g) {
        let (fg, itens) = assinatura(&obs);
        assert_eq!(fg, f["assinatura"][0], "{nome}");
        let want: Vec<(String, String, String)> = serde_json::from_value(f["assinatura"][1].clone()).unwrap();
        assert_eq!(itens, want, "{nome}");
    }
}

#[test]
fn compact_state_serializes_byte_identical() {
    let g = golden();
    let c = contexto(&g);
    for (nome, obs, f) in fixtures(&g) {
        let dica = f["dica"].as_str().unwrap();
        let got = estado_compacto("preencher o cadastro", &c.dados, &obs, &c.recentes, Some(dica).filter(|d| !d.is_empty()));
        assert_eq!(
            serde_json::to_string(&got).unwrap(),
            serde_json::to_string(&f["estado_compacto"]).unwrap(),
            "{nome}"
        );
    }
}

#[test]
fn no_controls_scaling_and_inside() {
    let g = golden();
    for (nome, obs, f) in fixtures(&g) {
        assert_eq!(sem_controles(&obs), f["sem_controles"], "{nome} sem_controles");
        for caso in f["para_tela"].as_array().unwrap() {
            let goal = caso["goal"].as_str().unwrap();
            for (antes, depois) in caso["antes"].as_array().unwrap().iter().zip(caso["depois"].as_array().unwrap()) {
                assert_eq!(para_tela(&acao(antes), &obs.screen, goal), acao(depois), "{nome} para_tela {antes} / {goal}");
            }
        }
        for caso in f["dentro"].as_array().unwrap() {
            assert_eq!(dentro(&acao(&caso["acao"]), &obs), caso["dentro"], "{nome} dentro {}", caso["acao"]);
        }
    }
}
