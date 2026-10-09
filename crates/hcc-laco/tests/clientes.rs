//! Jev/LLM client behavior that the goldens do not pin: failures, redaction, secrets.

use std::time::Duration;

use hcc_laco::candidatos::{candidatos, segredos};
use hcc_laco::estado::estado_compacto;
use hcc_laco::jev::{JEV_MODEL, Jev};
use hcc_laco::llm::Llm;
use hcc_protocolo::Observation;
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const T: Duration = Duration::from_secs(5);
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x02\0\0\0\x02";

async fn responde(r: ResponseTemplate) -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("POST")).respond_with(r).mount(&s).await;
    s
}

fn llm(s: &MockServer, key: Option<&str>) -> Llm {
    Llm::new(s.uri(), "m".into(), None, key.map(str::to_owned))
}

fn conteudo(texto: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"choices": [{"message": {"content": texto}}]}))
}

#[tokio::test]
async fn llm_failure_degrades_to_hint() {
    let s = responde(ResponseTemplate::new(500).set_body_string("falhou")).await;
    let a = llm(&s, Some("KEY")).ajudar(&json!({}), PNG, T).await;
    assert_eq!(a.interpretacao, format!("ajuda visual indisponível: RuntimeError: HTTP 500 em {}: falhou", s.uri()));
    assert!(a.acoes.is_empty() && a.impedimento.is_empty());
}

#[tokio::test]
async fn llm_timeout_degrades_to_hint() {
    let s = responde(conteudo("{}").set_delay(Duration::from_secs(3))).await;
    let a = llm(&s, Some("KEY")).ajudar(&json!({}), PNG, Duration::from_millis(200)).await;
    assert_eq!(a.interpretacao, "ajuda visual indisponível: TimeoutError: timed out");
}

#[tokio::test]
async fn llm_invalid_answer_degrades_to_hint() {
    let sete = json!({"interpretacao": "x", "acoes": vec![json!({"type": "keys", "keys": ["Tab"]}); 7]});
    let s = responde(conteudo(&sete.to_string())).await;
    let a = llm(&s, Some("KEY")).ajudar(&json!({}), PNG, T).await;
    assert!(a.interpretacao.starts_with("ajuda visual indisponível: ValidationError: "), "{}", a.interpretacao);
    let s = responde(conteudo(r#"{"interpretacao": "x", "acoes": [{"type": "invoke"}]}"#)).await;
    let a = llm(&s, Some("KEY")).ajudar(&json!({}), PNG, T).await;
    assert!(a.interpretacao.starts_with("ajuda visual indisponível: ValidationError: "), "{}", a.interpretacao);
}

#[tokio::test]
async fn llm_without_key_is_unavailable() {
    let s = responde(conteudo("{}")).await;
    for key in [None, Some("")] {
        let a = llm(&s, key).ajudar(&json!({}), PNG, T).await;
        assert_eq!(
            a.interpretacao,
            "ajuda visual indisponível: RuntimeError: falta LLM_PROXY_KEY no ambiente para o fallback de interpretação"
        );
    }
    assert!(s.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn http_error_text_redacts_key() {
    let corpo = format!("token segredo-123 inválido {}", "x".repeat(3000));
    let s = responde(ResponseTemplate::new(403).set_body_string(corpo)).await;
    let e = Jev::new(s.uri(), "segredo-123".into(), JEV_MODEL.into()).arriscado(&json!({}), "p", T).await.unwrap_err();
    let esperado = format!("token [redigido] inválido {}", "x".repeat(2000 - "token segredo-123 inválido ".len()));
    assert_eq!(e, format!("RuntimeError: HTTP 403 em {}: {esperado}", s.uri()));
    let a = llm(&s, Some("segredo-123")).ajudar(&json!({}), PNG, T).await;
    assert!(!a.interpretacao.contains("segredo-123") && a.interpretacao.contains("[redigido]"));

    // A key straddling byte 2000 is kept whole and redacted: no prefix of it survives.
    let s = responde(ResponseTemplate::new(500).set_body_string(format!("{}segredo-123", "x".repeat(1995)))).await;
    let e = Jev::new(s.uri(), "segredo-123".into(), JEV_MODEL.into()).arriscado(&json!({}), "p", T).await.unwrap_err();
    assert_eq!(e, format!("RuntimeError: HTTP 500 em {}: {}[redigido]", s.uri(), "x".repeat(1995)));
}

#[tokio::test]
async fn http_error_key_twice_never_leaks() {
    let k = "K".repeat(50);
    let s = responde(ResponseTemplate::new(500).set_body_string(format!("{k}{}{k}", "x".repeat(1980)))).await;
    let e = Jev::new(s.uri(), k.clone(), JEV_MODEL.into()).arriscado(&json!({}), "p", T).await.unwrap_err();
    assert!(!e.contains(&"K".repeat(10)), "vazou: ...{}", &e[e.len() - 40..]);
}

#[tokio::test]
async fn malformed_risk_answer_fails_closed() {
    for corpo in [json!({"answers": null}), json!({"answers": {"risky": null}}), json!({"answers": {"risky": 0.9}})] {
        let s = responde(ResponseTemplate::new(200).set_body_json(corpo.clone())).await;
        let r = Jev::new(s.uri(), "KEY".into(), JEV_MODEL.into()).arriscado(&json!({}), "p", T).await;
        assert!(r.as_ref().is_err_and(|e| e.starts_with("AttributeError: ")), "{corpo} -> {r:?}");
    }
    let s = responde(ResponseTemplate::new(200).set_body_json(json!({"answers": {
        "action": {"probabilities": {"DONE": 0.9}}, "risky": 0.9}}))).await;
    let r = Jev::new(s.uri(), "KEY".into(), JEV_MODEL.into()).decidir(&json!({}), &[], T).await;
    assert_eq!(r.unwrap_err(), "AttributeError: 'float' object has no attribute 'get'");
}

#[tokio::test]
async fn redirect_is_an_http_error_not_followed() {
    let s = responde(ResponseTemplate::new(307).insert_header("location", "http://127.0.0.1:9/")).await;
    let e = Jev::new(s.uri(), "KEY".into(), JEV_MODEL.into()).arriscado(&json!({}), "p", T).await.unwrap_err();
    assert_eq!(e, format!("RuntimeError: HTTP 307 em {}: ", s.uri()));
}

#[tokio::test]
async fn llm_answer_shape_errors_name_the_python_exception() {
    for (resposta, esperado) in [
        (json!({}), "KeyError: 'choices'"),
        (json!({"choices": []}), "IndexError: list index out of range"),
        (json!({"choices": [{}]}), "KeyError: 'message'"),
        (json!({"choices": [{"message": {"content": null}}]}), "AttributeError: 'NoneType' object has no attribute 'strip'"),
        (json!({"choices": [{"message": {"content": "```sem quebra"}}]}), "IndexError: list index out of range"),
    ] {
        let s = responde(ResponseTemplate::new(200).set_body_json(resposta)).await;
        let a = llm(&s, Some("KEY")).ajudar(&json!({}), PNG, T).await;
        assert_eq!(a.interpretacao, format!("ajuda visual indisponível: {esperado}"));
    }
}

#[tokio::test]
async fn empty_probabilities_is_an_error_not_a_panic() {
    let s = responde(ResponseTemplate::new(200).set_body_json(json!({"answers": {"action": {"probabilities": {}}}}))).await;
    let e = Jev::new(s.uri(), "KEY".into(), JEV_MODEL.into()).decidir(&json!({}), &[], T).await.unwrap_err();
    assert_eq!(e, "ValueError: max() iterable argument is empty");
}

#[tokio::test]
async fn unknown_choice_is_an_error_not_a_panic() {
    let s = responde(ResponseTemplate::new(200).set_body_json(json!({"answers": {"action": {"probabilities": {"7": 0.9}}}}))).await;
    let e = Jev::new(s.uri(), "KEY".into(), JEV_MODEL.into()).decidir(&json!({}), &[], T).await.unwrap_err();
    assert_eq!(e, "IndexError: list index out of range");
    let s = responde(ResponseTemplate::new(200).set_body_json(json!({"answers": {}}))).await;
    let p = Jev::new(s.uri(), "KEY".into(), JEV_MODEL.into()).arriscado(&json!({}), "p", T).await.unwrap();
    assert_eq!(p, 0.0);
    let s = responde(ResponseTemplate::new(200).set_body_json(json!({"sem": 1}))).await;
    let e = Jev::new(s.uri(), "KEY".into(), JEV_MODEL.into()).arriscado(&json!({}), "p", T).await.unwrap_err();
    assert_eq!(e, "KeyError: 'answers'");
}

#[tokio::test]
async fn secret_never_reaches_jev_or_llm() {
    let obs: Observation = serde_json::from_value(json!({
        "observation_id": "o1", "connected": true, "session_id": 1, "foreground": "w1",
        "windows": [{"id": "w1", "name": "Login s3gr3d0", "process_id": 1, "class_name": "TMainForm", "rect": [0, 0, 800, 600]}],
        "elements": [
            {"id": "e0", "name": "Senha", "role": "Edit", "value": "s3gr3d0", "enabled": true, "rect": [0, 0, 9, 9],
             "focused": true, "actions": ["set_value"], "password": true},
            {"id": "e1", "name": "Eco", "role": "Text", "value": "digitado: s3gr3d0", "enabled": true, "rect": null,
             "focused": false, "actions": ["set_value"]}],
        "truncated": false, "timestamp": 1.0, "screen": {"width": 1280, "height": 800}}))
    .unwrap();
    let dados = json!({"usuario": "ana", "senha": "s3gr3d0"}).as_object().unwrap().clone();
    let segredos = segredos(&dados);
    let estado = estado_compacto("entrar com a senha", &dados, &obs, &[], Some("dica"));
    let cands = candidatos(&obs, &dados, &segredos);
    assert!(cands.iter().any(|c| c.acao.value.as_deref() == Some("s3gr3d0")));

    let s = responde(ResponseTemplate::new(200).set_body_json(json!({
        "answers": {"action": {"probabilities": {"0": 0.9}}, "risky": {"noul": 0.1}},
        "choices": [{"message": {"content": "{\"interpretacao\": \"ok\"}"}}]}))).await;
    let jev = Jev::new(s.uri(), "KEY".into(), JEV_MODEL.into());
    jev.decidir(&estado, &cands, T).await.unwrap();
    jev.arriscado(&estado, &cands[0].descricao, T).await.unwrap();
    llm(&s, Some("KEY")).ajudar(&estado, PNG, T).await;
    let pedidos = s.received_requests().await.unwrap();
    assert_eq!(pedidos.len(), 3);
    for p in pedidos {
        assert!(!String::from_utf8_lossy(&p.body).contains("s3gr3d0"));
    }
}

#[tokio::test]
async fn fenced_json_answer_accepted() {
    let s = responde(conteudo("  ```json\n{\"interpretacao\": \"tela\", \"acoes\": [{\"type\": \"keys\", \"keys\": [\"Enter\"]}]}\n```  ")).await;
    let a = llm(&s, Some("KEY")).ajudar(&json!({}), &[], T).await;
    assert_eq!(a.interpretacao, "tela");
    assert_eq!(a.acoes.len(), 1);
    let corpo: Value = serde_json::from_slice(&s.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(corpo["messages"][1]["content"], json!([{"type": "text", "text": "{}"}]));
}

#[tokio::test]
async fn model_from_config_goes_in_body() {
    let s = responde(ResponseTemplate::new(200).set_body_json(json!({"answers": {"risky": {"noul": 0.1}}}))).await;
    Jev::new(s.uri(), "KEY".into(), "~typesafe/jev-latest".into()).arriscado(&json!({}), "p", T).await.unwrap();
    let pedidos = s.received_requests().await.unwrap();
    let corpo: Value = serde_json::from_slice(&pedidos[0].body).unwrap();
    assert_eq!(corpo["model"], "~typesafe/jev-latest");
}
