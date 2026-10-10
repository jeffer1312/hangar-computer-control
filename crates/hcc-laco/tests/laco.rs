mod fakes;

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fakes::{Answer, FakeConnector, Script, help, seconds, tree};
use hcc_laco::candidatos::{candidatos, segredos};
use hcc_laco::descrever::descrever;
use hcc_laco::executar::{Opcoes, executar};
use hcc_laco::geometria::{dentro, para_tela};
use hcc_laco::jev::{JEV_MODEL, Jev, LOTE};
use hcc_laco::llm::Llm;
use hcc_laco::resultado::Resultado;
use hcc_laco::tipos::{Escolha, Progresso};
use hcc_protocolo::{Action, Observation, Rect, SessionError};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup(answers: Vec<Answer>) -> (Opcoes, MockServer) {
    let server = MockServer::start().await;
    let script = Script::new(answers);
    Mock::given(method("POST")).respond_with(script).mount(&server).await;
    let config = tempfile::tempdir().unwrap().keep().join("vm-a-agent.json");
    let op = Opcoes {
        texto: "preencher Nome com Ana".into(), max_passos: 3,
        dados: json!({"nome":"Ana"}).as_object().unwrap().clone(), limite: seconds(240), config: Some(config),
        jev: Some(Jev::new(format!("{}/jev", server.uri()), "fake-key".into(), JEV_MODEL.into())),
        llm: Llm::new(format!("{}/llm", server.uri()), "fake-model".into(), None, Some("fake-key".into())),
    };
    (op, server)
}

async fn run(op: Opcoes, con: &FakeConnector) -> Resultado {
    executar(op, con, CancellationToken::new(), &|_| {}).await
}

async fn bodies(server: &MockServer) -> Vec<Value> {
    server.received_requests().await.unwrap().iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
}

fn action(v: Value) -> Action { serde_json::from_value(v).unwrap() }
fn done() -> Answer { Answer::Choice("DONE", 0.95, 0.05) }
fn select(s: &'static str) -> Answer { Answer::Select(s, 0.9, 0.05) }
fn empty(id: &str) -> Observation {
    let mut obs = tree(id, "");
    obs.elements.clear();
    obs
}

#[tokio::test]
async fn tree_action_then_done() {
    let con = FakeConnector::new(vec![tree("o1", ""), tree("o2", "Ana")]);
    let (op, server) = setup(vec![select("set_value"), done()]).await;
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(r.motivo, "objetivo confirmado (95%) na janela 'Editor'");
    {
        let s = con.state.lock().unwrap();
        assert_eq!(s.acts.len(), 1);
        assert_eq!(s.acts[0].0, action(json!({"type":"set_value","target":"e0","value":"Ana"})));
        assert_eq!(s.acts[0].1, "o1");
        assert_eq!(s.closes, 1);
    }
    let requests = bodies(&server).await;
    assert_eq!(requests[1]["state"]["recentActions"][0]["screenChanged"], true);
    assert_eq!(requests[1]["state"]["elements"][0]["value"], "Ana");
    assert!(r.tempos.iter().any(|t| t.starts_with("acao:set_value=")));
    assert!(r.tempos.last().unwrap().starts_with("total="));
}

#[tokio::test]
async fn visual_hint_action_requires_selection() {
    let con = FakeConnector::new(vec![tree("o1", ""), tree("o2", "Ana")]);
    let (op, server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05),
        help(json!([{"type":"text","target":"e0","value":"Ana"}])), select("text 'Ana'"), done()]).await;
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(con.state.lock().unwrap().captures, 1);
    assert_eq!(con.state.lock().unwrap().acts[0].0.kind.to_string(), "text");
    assert_eq!(bodies(&server).await[2]["state"]["hint"], "tela vista");
}

#[tokio::test]
async fn drawn_dialog_click_checks_risk_only() {
    let mut before = empty("o1");
    before.screen.width = 1920;
    before.windows[0].rect = Rect(831, 465, 1088, 567);
    let con = FakeConnector::new(vec![before, tree("o2", "Ana")]);
    let (op, server) = setup(vec![help(json!([{"type":"mouse","x":696,"y":366,"button":"left",
        "mode":"click","rotulo":"botão Não"}])), Answer::Risk(0.05), done()]).await;
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(con.state.lock().unwrap().acts[0].0.x, Some(1044));
    assert!(r.passos[0].contains("'botão Não'"));
    let requests = bodies(&server).await;
    assert!(requests[0].get("messages").is_some());
    assert_eq!(requests[1]["questions"].as_object().unwrap().len(), 1);
    assert_eq!(requests[1]["state"]["proposedAction"], "mouse 'botão Não' click (1044,549)");
}

#[tokio::test]
async fn direct_path_types_text_then_presses_enter() {
    let con = FakeConnector::new(vec![titled("o1", "A"), titled("o2", "B"), titled("o3", "C"), tree("o4", "fim")]);
    let (op, server) = setup(vec![help(json!([
        {"type":"keys","keys":["CTRL","L"]}, {"type":"text","value":"https://x.com"}, {"type":"keys","keys":["Enter"]},
    ])), Answer::Risk(0.05), Answer::Risk(0.05), Answer::Risk(0.05), done()]).await;
    let r = run(op, &con).await;
    let acts: Vec<_> = con.state.lock().unwrap().acts.iter().map(|(a, id, _)| (a.clone(), id.clone())).collect();
    assert_eq!(acts, vec![
        (action(json!({"type":"keys","keys":["CTRL","L"]})), "o1".into()),
        (action(json!({"type":"text","value":"https://x.com"})), "o2".into()),
        (action(json!({"type":"keys","keys":["Enter"]})), "o3".into()),
    ], "{}", r.resumo());
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(con.state.lock().unwrap().captures, 1, "uma ajuda só: as três ações saem no mesmo ciclo");
    let requests = bodies(&server).await;
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[1]["state"]["proposedAction"], "keys CTRL+L");
    assert_eq!(requests[2]["state"]["proposedAction"], "text 'https://x.com' no foco atual");
    assert_eq!(requests[3]["state"]["proposedAction"], "keys Enter");
    for i in [1, 2, 3] { assert_eq!(requests[i]["questions"].as_object().unwrap().len(), 1); }
}

#[tokio::test]
async fn first_act_never_untargeted_text() {
    let mut before = empty("o1");
    before.windows[0].name = "Messenger".into();
    let mut editor = before.windows[0].clone();
    editor.id = "w2".into();
    editor.name = "Editor".into();
    before.windows.push(editor);
    let con = FakeConnector::new(vec![before, tree("o2", "Ana")]);
    let (op, server) = setup(vec![Answer::Vision(json!({
        "interpretacao":"o editor de texto ainda precisa ser aberto",
        "acoes":[{"type":"text","value":"ação, coração e pé"}],
    })), Answer::Choice("0", 0.9, 0.05), done()]).await;
    let r = run(op, &con).await;
    let acts: Vec<_> = con.state.lock().unwrap().acts.iter().map(|(a, _, _)| a.clone()).collect();
    assert_eq!(acts, vec![action(json!({"type":"activate","target":"w2"}))], "{}", r.resumo());
    assert!(r.ok, "{}", r.resumo());
    let requests = bodies(&server).await;
    let criteria = requests[1]["questions"]["action"]["criteria"].as_object().unwrap();
    assert!(!criteria.values().any(|v| v.as_str().is_some_and(|v| v.contains("text 'ação"))));
}

#[tokio::test]
async fn direct_sequence_ctrl_l_text_enter_still_runs() {
    let con = FakeConnector::new(vec![titled("o1", "a"), titled("o2", "b"), titled("o3", "c"), tree("o4", "fim")]);
    let (op, _server) = setup(vec![help(json!([
        {"type":"keys","keys":["CTRL","L"]}, {"type":"text","value":"ação"}, {"type":"keys","keys":["Enter"]},
    ])), Answer::Risk(0.05), Answer::Risk(0.05), Answer::Risk(0.05), done()]).await;
    let r = run(op, &con).await;
    let acts: Vec<_> = con.state.lock().unwrap().acts.iter().map(|(a, _, _)| a.clone()).collect();
    assert_eq!(acts, vec![action(json!({"type":"keys","keys":["CTRL","L"]})),
        action(json!({"type":"text","value":"ação"})), action(json!({"type":"keys","keys":["Enter"]}))], "{}", r.resumo());
    assert!(r.ok, "{}", r.resumo());
    assert!(jev_log(&r).iter().filter(|l| l.get("direta").is_some()).all(|l| l["ciclo"] == 0));
}

#[tokio::test]
async fn done_after_help_strong_reading_confirmed() {
    for (p, choice, p2) in [(0.99, "WAIT", 0.5), (0.95, "DONE", 0.64), (1.0, "DONE", 0.8)] {
        let con = FakeConnector::new(vec![empty("o1")]);
        let (op, server) = setup(vec![help(json!([])), Answer::Choice("DONE", p, 0.05),
            Answer::Choice(choice, p2, 0.05)]).await;
        let r = run(op, &con).await;
        assert!(r.ok, "{}", r.resumo());
        assert_eq!(r.motivo, format!("objetivo confirmado pela leitura da tela ({:.0}%) na janela 'Editor'", p * 100.0));
        assert!(bodies(&server).await[2]["state"].get("hint").is_none());
        assert!(con.state.lock().unwrap().acts.is_empty());
    }
}

#[tokio::test]
async fn done_after_help_weak_reading_refused() {
    for p in [0.9, 0.94] {
        let con = FakeConnector::new(vec![empty("o1")]);
        let (op, _server) = setup(vec![help(json!([])), Answer::Choice("DONE", p, 0.05),
            Answer::Choice("WAIT", 0.5, 0.05)]).await;
        let r = run(op, &con).await;
        assert!(!r.ok);
        assert_eq!(r.motivo, "não confirmado: só a leitura do print indica que terminou. tela vista");
    }
}

#[tokio::test]
async fn done_after_help_blocked_recheck_refused() {
    for choice in ["BLOCKED", "0"] {
        let con = FakeConnector::new(vec![empty("o1")]);
        let (op, _server) = setup(vec![help(json!([])), Answer::Choice("DONE", 0.99, 0.05),
            Answer::Choice(choice, 0.9, 0.05)]).await;
        let r = run(op, &con).await;
        assert!(!r.ok);
        assert_eq!(r.motivo, "não confirmado: só a leitura do print indica que terminou. tela vista");
        assert!(con.state.lock().unwrap().acts.is_empty());
    }
}

#[tokio::test]
async fn done_after_failed_visual_help_is_not_confirmed() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (mut op, server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05),
        Answer::Choice("DONE", 0.99, 0.05), Answer::Choice("WAIT", 0.5, 0.05)]).await;
    Mock::given(path("/bad")).respond_with(ResponseTemplate::new(500).set_body_string("erro visual"))
        .with_priority(1).mount(&server).await;
    op.llm = Llm::new(format!("{}/bad", server.uri()), "fake-model".into(), None, Some("fake-key".into()));
    let r = run(op, &con).await;
    assert!(!r.ok, "{}", r.resumo());
    assert!(r.motivo.starts_with("não confirmado: só a leitura do print indica que terminou. ajuda visual indisponível: RuntimeError: HTTP 500"), "{}", r.resumo());
    assert!(con.state.lock().unwrap().acts.is_empty());
}

#[tokio::test]
async fn first_act_untargeted_text_in_focused_edit_is_not_offered() {
    let mut before = tree("o1", "");
    before.windows[0].name = "Messenger".into();
    let mut editor = before.windows[0].clone();
    editor.id = "w2".into();
    editor.name = "Editor".into();
    before.windows.push(editor);
    let con = FakeConnector::new(vec![before, tree("o2", "Ana")]);
    let (op, server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05),
        help(json!([{"type":"text","value":"ação"}])), select("activate window 'Editor'"), done()]).await;
    let r = run(op, &con).await;
    let acts: Vec<_> = con.state.lock().unwrap().acts.iter().map(|(a, _, _)| a.clone()).collect();
    assert_eq!(acts, vec![action(json!({"type":"activate","target":"w2"}))], "{}", r.resumo());
    let requests = bodies(&server).await;
    assert!(!requests[2]["questions"]["action"]["criteria"].as_object().unwrap().values()
        .any(|v| v.as_str().is_some_and(|v| v.contains("text 'ação"))));
    assert!(r.ok, "{}", r.resumo());
}

#[tokio::test]
async fn done_after_help_strong_reading_still_checks_pending_items() {
    let con = FakeConnector::new(vec![tree("o1", "outro")]);
    let (mut op, _server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05), help(json!([])),
        Answer::Choice("DONE", 0.99, 0.05), Answer::Choice("WAIT", 0.5, 0.05),
        Answer::Choice("DONE", 0.99, 0.05), Answer::Choice("WAIT", 0.5, 0.05)]).await;
    op.texto = "escrever 'Ana'".into();
    let r = run(op, &con).await;
    assert!(!r.ok);
    assert_eq!(r.motivo, "não confirmado: ainda falta: o texto 'Ana' não aparece na tela");
    assert!(con.state.lock().unwrap().acts.is_empty());
}

fn jev_log(r: &Resultado) -> Vec<Value> {
    std::fs::read_to_string(r.registro.as_ref().unwrap().join("jev.jsonl")).unwrap()
        .lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

#[tokio::test]
async fn direct_sequence_runs_keys_text_keys_in_one_cycle() {
    let con = FakeConnector::new(vec![titled("o1", "a"), titled("o2", "b"), titled("o3", "c"), tree("o4", "fim")]);
    let (op, server) = setup(vec![help(json!([
        {"type":"keys","keys":["CTRL","L"]}, {"type":"text","value":"x.com"}, {"type":"keys","keys":["Enter"]},
    ])), Answer::Risk(0.05), Answer::Risk(0.05), Answer::Risk(0.05), done()]).await;
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    let acts: Vec<_> = con.state.lock().unwrap().acts.iter().map(|(a, id, _)| (a.clone(), id.clone())).collect();
    assert_eq!(acts, vec![
        (action(json!({"type":"keys","keys":["CTRL","L"]})), "o1".into()),
        (action(json!({"type":"text","value":"x.com"})), "o2".into()),
        (action(json!({"type":"keys","keys":["Enter"]})), "o3".into()),
    ]);
    assert_eq!(con.state.lock().unwrap().captures, 1);
    let diretas: Vec<_> = jev_log(&r).into_iter().filter(|l| l.get("direta").is_some()).collect();
    assert_eq!(diretas.iter().map(|l| l["direta"].as_str().unwrap()).collect::<Vec<_>>(),
        vec!["keys CTRL+L", "text 'x.com' no foco atual", "keys Enter"]);
    assert!(diretas.iter().all(|l| l["ciclo"] == 0), "{diretas:?}");
    assert_eq!(r.passos.len(), 3);
    let requests = bodies(&server).await;
    let recentes = requests[4]["state"]["recentActions"].as_array().unwrap();
    assert_eq!(recentes.len(), 3);
    assert!(recentes.iter().all(|a| a["screenChanged"] == true), "{recentes:?}");
}

#[tokio::test]
async fn direct_sequence_stops_at_mouse() {
    let con = FakeConnector::new(vec![titled("o1", "a"), tree("o2", "fim")]);
    let (op, server) = setup(vec![help(json!([
        {"type":"keys","keys":["CTRL","L"]}, {"type":"mouse","x":10,"y":10,"button":"left","mode":"click"},
    ])), Answer::Risk(0.05), done()]).await;
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    let acts: Vec<_> = con.state.lock().unwrap().acts.iter().map(|(a, _, _)| a.clone()).collect();
    assert_eq!(acts, vec![action(json!({"type":"keys","keys":["CTRL","L"]}))]);
    assert_eq!(bodies(&server).await.len(), 3);
}

#[tokio::test]
async fn direct_sequence_stops_at_first_failure() {
    let con = FakeConnector::new(vec![titled("o1", "a")]);
    con.state.lock().unwrap().act_errors.push_back(SessionError::Agent("tecla recusada".into()));
    let (op, server) = setup(vec![help(json!([
        {"type":"keys","keys":["CTRL","L"]}, {"type":"text","value":"x.com"},
    ])), Answer::Risk(0.05),
        Answer::Vision(json!({"interpretacao":"x", "acoes":[], "impedimento":"parou"}))]).await;
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "parou");
    assert_eq!(con.state.lock().unwrap().acts.len(), 1);
    assert_eq!(bodies(&server).await.len(), 3, "a segunda ação não passa nem pela checagem de risco");
}

#[tokio::test]
async fn direct_sequence_respects_twice_guard() {
    let con = FakeConnector::new(vec![titled("o1", "a"), titled("o2", "b"), titled("o3", "c"), tree("o4", "fim")]);
    let (op, _server) = setup(vec![help(json!([
        {"type":"keys","keys":["Down"]}, {"type":"keys","keys":["Down"]}, {"type":"keys","keys":["Down"]},
    ])), Answer::Risk(0.05), Answer::Risk(0.05), Answer::Risk(0.05), done()]).await;
    let _r = run(op, &con).await;
    assert_eq!(con.state.lock().unwrap().acts.len(), 2, "barrar deixa no máximo 2 ações iguais");
}

#[tokio::test]
async fn direct_sequence_stops_after_no_effect() {
    let con = FakeConnector::new(vec![titled("o1", "a"), titled("o2", "a"), titled("o3", "a"), tree("o4", "fim")]);
    let (op, _server) = setup(vec![help(json!([
        {"type":"keys","keys":["Enter"]}, {"type":"keys","keys":["Enter"]},
    ])), Answer::Risk(0.05), Answer::Risk(0.05), done()]).await;
    let _r = run(op, &con).await;
    assert_eq!(con.state.lock().unwrap().acts.len(), 1, "o primeiro Enter não mudou a tela; o segundo é barrado");
}

#[tokio::test]
async fn direct_sequence_stops_when_foreground_changes() {
    let mut dialogo = tree("o2", "fim");
    dialogo.foreground = "w2".into();
    let con = FakeConnector::new(vec![titled("o1", "a"), dialogo]);
    let (op, _server) = setup(vec![help(json!([
        {"type":"keys","keys":["CTRL","L"]}, {"type":"text","value":"x.com"}, {"type":"keys","keys":["Enter"]},
    ])), Answer::Risk(0.05), done()]).await;
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    let acts: Vec<_> = con.state.lock().unwrap().acts.iter().map(|(a, _, _)| a.clone()).collect();
    assert_eq!(acts, vec![action(json!({"type":"keys","keys":["CTRL","L"]}))]);
}

#[tokio::test]
async fn direct_sequence_masks_secret_in_each_line() {
    let secret = "sëgredo-fila-123";
    let con = FakeConnector::new(vec![titled("o1", "a"), titled("o2", "b"), tree("o3", "fim")]);
    let (mut op, server) = setup(vec![help(json!([
        {"type":"keys","keys":["CTRL","L"]}, {"type":"text","value":secret},
    ])), Answer::Risk(0.05), Answer::Risk(0.05), done()]).await;
    op.dados = json!({"senha":secret}).as_object().unwrap().clone();
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(con.state.lock().unwrap().acts.len(), 2);
    let diretas: Vec<_> = jev_log(&r).into_iter().filter_map(|l| l.get("direta").cloned()).collect();
    assert_eq!(diretas, vec![json!("keys CTRL+L"), json!("text *** no foco atual")]);
    for file in ["jev.jsonl", "observacao.json"] {
        assert!(!std::fs::read_to_string(r.registro.as_ref().unwrap().join(file)).unwrap().contains(secret));
    }
    for body in bodies(&server).await { assert!(!body.to_string().contains(secret), "{body}"); }
    assert!(!r.resumo().contains(secret));
}

#[tokio::test]
async fn done_after_help_confirmed_by_goal_evidence() {
    let con = FakeConnector::new(vec![tree("o1", "Ana")]);
    let (mut op, _server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05), help(json!([])),
        Answer::Choice("DONE", 0.9, 0.05), Answer::Choice("DONE", 0.6, 0.05)]).await;
    op.texto = "escrever 'Ana' no Nome".into();
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(r.motivo, "objetivo confirmado (90%) na janela 'Editor'");
}

#[tokio::test]
async fn done_after_help_goal_evidence_ignores_recheck_choice() {
    for choice in ["BLOCKED", "0"] {
        let con = FakeConnector::new(vec![tree("o1", "Ana")]);
        let (mut op, _server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05), help(json!([])),
            Answer::Choice("DONE", 0.9, 0.05), Answer::Choice(choice, 0.6, 0.05)]).await;
        op.texto = "escrever 'Ana' no Nome".into();
        let r = run(op, &con).await;
        assert!(r.ok, "{choice}: {}", r.resumo());
        assert_eq!(r.motivo, "objetivo confirmado (90%) na janela 'Editor'");
    }
}

#[tokio::test]
async fn done_after_help_refused_without_goal_items() {
    let con = FakeConnector::new(vec![tree("o1", "Ana")]);
    let (op, _server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05), help(json!([])),
        Answer::Choice("DONE", 0.9, 0.05), Answer::Choice("DONE", 0.6, 0.05)]).await;
    let r = run(op, &con).await;
    assert!(!r.ok, "{}", r.resumo());
    assert_eq!(r.motivo, "não confirmado: só a leitura do print indica que terminou. tela vista");
}

#[tokio::test]
async fn direct_text_secret_is_masked() {
    let secret = "sëgredo-direto-123";
    let con = FakeConnector::new(vec![titled("o1", "A"), titled("o2", "B"), tree("o3", "fim")]);
    let (mut op, server) = setup(vec![help(json!([{"type":"keys","keys":["CTRL","L"]}, {"type":"text","value":secret}])),
        Answer::Risk(0.05), Answer::Risk(0.05), done()]).await;
    op.dados = json!({"senha":secret}).as_object().unwrap().clone();
    let progress = Mutex::new(vec![]);
    let r = executar(op, &con, CancellationToken::new(), &|p| progress.lock().unwrap().push(p)).await;
    let acts: Vec<_> = con.state.lock().unwrap().acts.iter().map(|(a, _, _)| a.clone()).collect();
    assert_eq!(acts, vec![action(json!({"type":"keys","keys":["CTRL","L"]})), action(json!({"type":"text","value":secret}))], "{}", r.resumo());
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(r.passos, vec!["1. keys CTRL+L [100%]", "2. text *** no foco atual [100%]"]);
    assert!(!r.resumo().contains(secret));
    assert!(!format!("{:?}", progress.lock().unwrap()).contains(secret));
    let requests = bodies(&server).await;
    assert_eq!(requests[2]["state"]["proposedAction"], "text *** no foco atual");
    for body in requests { assert!(!body.to_string().contains(secret), "{body}"); }
    let dir = r.registro.as_ref().unwrap();
    let log = std::fs::read_to_string(dir.join("jev.jsonl")).unwrap();
    assert!(!log.contains(secret));
    let text: Value = log.lines().map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|l| l["direta"] == "text *** no foco atual").unwrap();
    assert_eq!(text["direta"], "text *** no foco atual");
    assert!(!std::fs::read_to_string(dir.join("observacao.json")).unwrap().contains(secret));
}

#[test]
fn literal_coordinates_preserve_position() {
    let mut obs = empty("o1");
    obs.screen.width = 1920;
    obs.windows[0].rect = Rect(831,465,1088,567);
    let a = action(json!({"type":"mouse","x":1042,"y":547,"button":"left","mode":"click"}));
    let literal = para_tela(&a, &obs.screen, "clicar em Não em (1042, 547)");
    let scaled = para_tela(&a, &obs.screen, "clicar em Não");
    assert_eq!((literal.x, literal.y), (Some(1042), Some(547)));
    assert_eq!((scaled.x, scaled.y), (Some(1563), Some(820)));
    assert!(dentro(&literal, &obs));
    assert!(!dentro(&scaled, &obs));
}

#[tokio::test]
async fn risky_visual_click_is_blocked() {
    let con = FakeConnector::new(vec![empty("o1")]);
    let (op, _server) = setup(vec![help(json!([{"type":"mouse","x":10,"y":10,"button":"left",
        "mode":"click","rotulo":"botão Sim"}])), Answer::Risk(0.9)]).await;
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "ação arriscada não executada: mouse 'botão Sim' click (10,10) (risco 90%). Autorize no objetivo.");
    assert!(con.state.lock().unwrap().acts.is_empty());
}

#[tokio::test]
async fn window_close_requires_matching_goal() {
    let mut obs = tree("o1", "");
    obs.elements[0].role = "TitleBar".into();
    obs.elements[0].rect = Some(Rect(16,0,1920,23));
    obs.elements[1].name = "Close".into();
    obs.elements[1].rect = Some(Rect(1872,0,1920,22));
    for (goal, expected) in [("fechar a aba Relatórios pelo X", 0), ("fechar o aplicativo", 1)] {
        let con = FakeConnector::new(vec![obs.clone(), tree("o2", "x")]);
        let (mut op, _server) = setup(vec![select("invoke Button 'Close'"), done()]).await;
        op.texto = goal.into();
        let r = run(op, &con).await;
        assert_eq!(con.state.lock().unwrap().acts.len(), expected, "{}", r.resumo());
    }
}

#[tokio::test]
async fn done_after_visual_help_is_unconfirmed() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (op, _server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05), help(json!([])), Answer::Choice("DONE", 0.94, 0.05),
        Answer::Choice("WAIT", 0.9, 0.05)]).await;
    let r = run(op, &con).await;
    assert!(!r.ok);
    assert_eq!(r.motivo, "não confirmado: só a leitura do print indica que terminou. tela vista");
}

#[tokio::test]
async fn done_refused_until_typed_text_appears() {
    let con = FakeConnector::new(vec![tree("o1", "outro texto"), tree("o2", "ação, coração e pé")]);
    let (mut op, server) = setup(vec![done(), select("set_value"), done()]).await;
    op.texto = "escrever 'ação, coração e pé'".into();
    op.dados = json!({"texto":"ação, coração e pé"}).as_object().unwrap().clone();
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(r.passos.len(), 1, "DONE não pode confirmar antes de escrever");
    let requests = bodies(&server).await;
    assert_eq!(requests[1]["state"]["hint"], "ainda falta: o texto 'ação, coração e pé' não aparece na tela");
    assert_eq!(requests[1]["state"]["recentActions"][0]["action"], "DONE recusado: ainda falta: o texto 'ação, coração e pé' não aparece na tela");
    assert_eq!(requests[2]["state"]["elements"][0]["value"], "ação, coração e pé");
}

#[tokio::test]
async fn done_refused_twice_stops_not_confirmed() {
    let con = FakeConnector::new(vec![tree("o1", "outro texto")]);
    let (mut op, server) = setup(vec![done(), done()]).await;
    op.texto = "escrever 'ação, coração e pé'".into();
    let r = run(op, &con).await;
    assert!(!r.ok, "{}", r.resumo());
    assert_eq!(r.motivo, "não confirmado: ainda falta: o texto 'ação, coração e pé' não aparece na tela");
    assert_eq!(bodies(&server).await.len(), 2);
    assert!(r.passos.is_empty());
}

#[tokio::test]
async fn done_refused_until_named_combo_pressed() {
    let con = FakeConnector::new(vec![tree("o1", ""), tree("o2", "busca")]);
    let (mut op, server) = setup(vec![done(), Answer::Choice("BLOCKED", 0.9, 0.05),
        help(json!([{"type":"keys","keys":["CTRL","F"]}])), select("keys CTRL+F"), done()]).await;
    op.texto = "pressionar Ctrl+F".into();
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(r.passos, vec!["1. keys CTRL+F [90%]"]);
    let requests = bodies(&server).await;
    assert_eq!(requests[1]["state"]["hint"], "ainda falta: CTRL+F não foi pressionado");
    assert_eq!(requests[3]["state"]["hint"], "ainda falta: CTRL+F não foi pressionado\ntela vista");
}

#[tokio::test]
async fn done_refused_secret_never_reaches_model_or_log() {
    let secret = "sëgredo-pendente-123";
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (mut op, server) = setup(vec![done(), done()]).await;
    op.texto = format!("digitar '{secret}'");
    op.dados = json!({"senha":secret}).as_object().unwrap().clone();
    let r = run(op, &con).await;
    assert!(!r.ok, "{}", r.resumo());
    assert_eq!(r.motivo, "não confirmado: ainda falta: o texto '<senha>' não aparece na tela");
    for body in bodies(&server).await { assert!(!body.to_string().contains(secret), "{body}"); }
    for file in ["jev.jsonl", "observacao.json"] {
        let contents = std::fs::read_to_string(r.registro.as_ref().unwrap().join(file)).unwrap();
        assert!(!contents.contains(secret), "{contents}");
    }
}

#[tokio::test]
async fn done_refused_distinct_secret_requirements_are_not_repeats() {
    let con = FakeConnector::new(vec![tree("o1", ""), tree("o2", "alpha"), tree("o3", "alpha beta")]);
    let (mut op, server) = setup(vec![done(), Answer::Choice("BLOCKED", 0.9, 0.05),
        help(json!([{"type":"text","target":"e0","value":"alpha","rotulo":"primeiro"}])), select("text *** em Edit 'Nome'"),
        done(), Answer::Choice("BLOCKED", 0.9, 0.05),
        help(json!([{"type":"text","value":"beta","rotulo":"segundo"}])), select("text 'segundo'"), done()]).await;
    op.texto = "digitar 'alpha' e 'beta'".into();
    op.dados = json!({"senha_a":"alpha","senha_b":"beta"}).as_object().unwrap().clone();
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(r.passos.len(), 2);
    for body in bodies(&server).await {
        assert!(!body.to_string().contains("alpha"));
        assert!(!body.to_string().contains("beta"));
    }
}

#[tokio::test]
async fn done_refused_does_not_reset_wait_limit() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let wait = || Answer::Choice("WAIT", 0.9, 0.05);
    let (mut op, server) = setup(vec![wait(), wait(), wait(), done(), wait(), help(json!([])),
        Answer::Choice("BLOCKED", 0.9, 0.05)]).await;
    op.texto = "digitar 'Bia'".into();
    op.max_passos = 5;
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "sem ação segura: Jev escolheu BLOCKED com 90%. ainda falta: o texto 'Bia' não aparece na tela\ntela vista");
    assert!(bodies(&server).await[5].get("messages").is_some());
}

fn titled(id: &str, title: &str) -> Observation {
    let mut obs = empty(id);
    obs.windows[0].name = title.into();
    obs
}

#[tokio::test]
async fn title_change_counts_as_screen_change() {
    let con = FakeConnector::new(vec![titled("o1", "A"), titled("o2", "B"), titled("o3", "C"), titled("o4", "D")]);
    let (op, server) = setup(vec![
        help(json!([{"type":"keys","keys":["F6"]}])), Answer::Risk(0.05),
        help(json!([{"type":"text","value":"https://example.com"}])), Answer::Risk(0.05),
        help(json!([{"type":"keys","keys":["Enter"]}])), Answer::Risk(0.05),
        help(json!([])), Answer::Choice("BLOCKED", 0.9, 0.05),
    ]).await;
    let r = run(op, &con).await;
    assert_eq!(con.state.lock().unwrap().acts.len(), 3, "{}", r.resumo());
    assert_eq!(r.motivo, "sem ação segura: Jev escolheu BLOCKED com 90%. tela vista");
    let requests = bodies(&server).await;
    let recentes = requests.last().unwrap()["state"]["recentActions"].as_array().unwrap().clone();
    assert_eq!(recentes.len(), 3);
    assert!(recentes.iter().all(|a| a["screenChanged"] == true), "{recentes:?}");
}

#[tokio::test]
async fn done_after_help_confirmed_without_hint() {
    let con = FakeConnector::new(vec![empty("o1")]);
    let (op, server) = setup(vec![help(json!([])), Answer::Choice("DONE", 0.9, 0.05),
        Answer::Choice("DONE", 0.9, 0.05)]).await;
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(r.motivo, "objetivo confirmado (90%) na janela 'Editor'");
    let requests = bodies(&server).await;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1]["state"]["hint"], "tela vista");
    assert!(requests[2]["state"].get("hint").is_none(), "{}", requests[2]);
    let log = std::fs::read_to_string(r.registro.as_ref().unwrap().join("jev.jsonl")).unwrap();
    let ultima: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    assert_eq!((ultima["sem_dica"].clone(), ultima["choice"].clone()), (json!(true), json!("DONE")));
}

#[tokio::test]
async fn done_after_help_refused_when_jev_without_hint_disagrees() {
    let con = FakeConnector::new(vec![empty("o1")]);
    let (op, server) = setup(vec![help(json!([])), Answer::Choice("DONE", 0.9, 0.05),
        Answer::Choice("WAIT", 0.9, 0.05)]).await;
    let r = run(op, &con).await;
    assert!(!r.ok);
    assert_eq!(r.motivo, "não confirmado: só a leitura do print indica que terminou. tela vista");
    assert!(bodies(&server).await[2]["state"].get("hint").is_none());
    assert!(con.state.lock().unwrap().acts.is_empty());
}

#[test]
fn menu_click_and_secret_redaction() {
    let mut obs = tree("o1", "");
    obs.elements[0].name = "Menu".into();
    obs.elements[0].role = "MenuItem".into();
    obs.elements[0].rect = Some(Rect(180,23,240,42));
    obs.elements[0].actions = vec![hcc_protocolo::ElementAction::Expand, hcc_protocolo::ElementAction::Invoke];
    obs.elements[1].role = "Edit".into();
    obs.elements[1].password = true;
    obs.elements[1].actions = vec![hcc_protocolo::ElementAction::SetValue];
    let data = json!({"senha":"segredo"}).as_object().unwrap().clone();
    let secrets = segredos(&data);
    let c = candidatos(&obs, &data, &secrets);
    assert!(c.iter().any(|c| c.acao == action(json!({"type":"mouse","target":"e0","x":210,"y":32,"button":"left","mode":"click"}))));
    assert!(!c.iter().any(|c| matches!(c.acao.kind, hcc_protocolo::ActionType::Invoke | hcc_protocolo::ActionType::Expand)));
    assert!(!c.iter().find(|c| c.acao.kind == hcc_protocolo::ActionType::SetValue).unwrap().descricao.contains("segredo"));
    assert!(!descrever(&action(json!({"type":"text","value":"segredo"})), &obs, &secrets, true).contains("segredo"));
}

#[tokio::test]
async fn nearby_clicks_share_repeat_limit() {
    let con = FakeConnector::new(vec![empty("o1")]);
    let click = |x| help(json!([{"type":"mouse","x":x,"y":80,"button":"left","mode":"click","rotulo":"link Mais"}]));
    let (mut op, _server) = setup(vec![click(500), Answer::Risk(0.05), click(502), Answer::Risk(0.05),
        click(504), Answer::Choice("BLOCKED", 0.9, 0.05)]).await;
    op.max_passos = 5;
    let r = run(op, &con).await;
    assert!(!r.ok);
    assert_eq!(con.state.lock().unwrap().acts.len(), 2);
}

#[tokio::test]
async fn repeated_warning_can_be_dismissed() {
    let mut one = tree("o1", "1");
    one.elements[1].name = "OK".into();
    let mut warning = one.elements[0].clone();
    warning.id = "warning".into();
    warning.role = "Text".into();
    warning.name = "Unexpected Memory Leak detected".into();
    warning.value = None;
    warning.actions.clear();
    one.elements.push(warning);
    let mut two = one.clone();
    two.observation_id = "o2".into();
    two.elements[0].value = Some("2".into());
    let con = FakeConnector::new(vec![one, two, tree("o3", "Ana")]);
    let (op, _server) = setup(vec![select("invoke Button 'OK'"), select("invoke Button 'OK'"), done()]).await;
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(con.state.lock().unwrap().acts.len(), 2);
}

#[tokio::test]
async fn low_confidence_after_help_stops() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (op, _server) = setup(vec![Answer::Choice("0", 0.2, 0.05), help(json!([])), Answer::Choice("0", 0.2, 0.05)]).await;
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "sem ação segura: Jev escolheu 0 com 20%. tela vista");
    assert!(con.state.lock().unwrap().acts.is_empty());
}

#[tokio::test]
async fn risky_tree_action_is_blocked() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (op, _server) = setup(vec![Answer::Choice("0", 0.9, 0.8)]).await;
    let r = run(op, &con).await;
    assert!(r.motivo.contains("risco 80%"));
    assert!(con.state.lock().unwrap().acts.is_empty());
}

#[tokio::test]
async fn batch_selection_returns_numeric_winner() {
    let mut obs = tree("o1", "");
    let mut button = obs.elements[1].clone();
    obs.elements = (0..300).map(|i| {
        button.id = format!("e{i}"); button.name = format!("b{i}"); button.clone()
    }).collect();
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"answers": {
        "batch_0":{"choice":"BLOCKED","probabilities":{"BLOCKED":0.9}},
        "batch_1":{"choice":"260","probabilities":{"260":0.8}}, "risky":{"noul":0},
    }}))).mount(&server).await;
    let c = candidatos(&obs, &Map::new(), &HashSet::new());
    let d = Jev::new(server.uri(), "fake-key".into(), JEV_MODEL.into()).decidir(&json!({}), &c, seconds(10)).await.unwrap();
    assert_eq!((d.escolha, d.p, d.risco), (Escolha::Indice(260), 0.8, 0.0));
    assert_eq!(bodies(&server).await[0]["questions"]["batch_0"]["criteria"].as_object().unwrap().len(), LOTE + 3);
}

#[tokio::test]
async fn reasoning_effort_is_optional() {
    for effort in [Some("high".into()), None] {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(Script::new(vec![help(json!([]))])).mount(&server).await;
        let llm = Llm::new(server.uri(), "fake-model".into(), effort.clone(), Some("fake-key".into()));
        let a = llm.ajudar(&json!({}), &[], seconds(10)).await;
        assert_eq!(a.interpretacao, "tela vista");
        assert_eq!(bodies(&server).await[0].get("reasoning_effort").and_then(Value::as_str), effort.as_deref());
    }
}

#[tokio::test]
async fn three_without_effect_stops() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (op, _server) = setup(vec![select("set_value"), select("invoke"), select("keys Enter")]).await;
    let r = run(op, &con).await;
    assert!(r.motivo.starts_with("três ações seguidas sem efeito na tela:"), "{}", r.resumo());
    assert_eq!(con.state.lock().unwrap().acts.len(), 3);
}

#[tokio::test]
async fn twice_executed_is_banned() {
    let con = FakeConnector::new(vec![tree("o1", "1"), tree("o2", "2"), tree("o3", "3")]);
    let (op, server) = setup(vec![select("invoke"), select("invoke"), done()]).await;
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    let requests = bodies(&server).await;
    assert!(!requests[2]["questions"]["action"]["criteria"].as_object().unwrap().values()
        .any(|v| v.as_str().is_some_and(|v| v.contains("invoke Button 'Salvar'"))));
    assert_eq!(con.state.lock().unwrap().acts.len(), 2);
}

#[tokio::test]
async fn reclique_offered_after_no_effect() {
    let mut obs = tree("o1", "");
    obs.elements[1].rect = Some(Rect(180,23,240,42));
    let con = FakeConnector::new(vec![obs]);
    let (op, server) = setup(vec![select("invoke"), select("mouse click (210,32)"), done()]).await;
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(con.state.lock().unwrap().acts[1].0, action(json!({"type":"mouse","x":210,"y":32,"button":"left","mode":"click"})));
    assert!(bodies(&server).await[1]["state"]["recentActions"][0]["result"].as_str().unwrap().contains("a real mouse click"));
}

#[tokio::test]
async fn wait_three_times_calls_llm() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (mut op, _server) = setup(vec![Answer::Choice("WAIT", 0.9, 0.05); 4].into_iter()
        .chain([help(json!([])), Answer::Choice("BLOCKED", 0.9, 0.05)]).collect()).await;
    op.max_passos = 5;
    let r = run(op, &con).await;
    assert!(r.motivo.contains("sem ação segura"));
    assert_eq!(con.state.lock().unwrap().captures, 1);
    assert!(con.state.lock().unwrap().acts.is_empty());
}

#[tokio::test]
async fn truncated_tree_calls_llm_even_when_confident() {
    let mut obs = tree("o1", ""); obs.truncated = true;
    let con = FakeConnector::new(vec![obs, tree("o2", "Ana")]);
    let (op, _server) = setup(vec![select("set_value"), help(json!([])), select("set_value"), done()]).await;
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(con.state.lock().unwrap().captures, 1);
    assert_eq!(con.state.lock().unwrap().acts.len(), 1);
}

#[tokio::test]
async fn settle_waits_45s_after_launch() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (op, _server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05),
        help(json!([{"type":"launch","application":"editor"}])), select("launch editor"), done()]).await;
    let paused = Arc::new(Mutex::new(false));
    let clock = paused.clone();
    let progress = move |p| {
        let mut paused = clock.lock().unwrap();
        if matches!(p, Progresso::Acao(_)) {
            tokio::time::pause(); *paused = true;
        } else if *paused {
            tokio::time::resume(); *paused = false;
        }
    };
    let r = executar(op, &con, CancellationToken::new(), &progress).await;
    assert!(r.ok, "{}", r.resumo());
    let s = con.state.lock().unwrap();
    let elapsed = s.observed.last().unwrap().duration_since(s.acts[0].2);
    assert!(elapsed >= seconds(45) && elapsed < seconds(47), "{elapsed:?}");
    assert!(s.observed.len() >= 45);
}

#[tokio::test]
async fn agent_timeout_reconnects_and_continues() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    con.state.lock().unwrap().act_errors.push_back(SessionError::Timeout("agente não respondeu".into()));
    let (op, server) = setup(vec![select("set_value"), done()]).await;
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(con.state.lock().unwrap().connects, 2);
    assert_eq!(con.state.lock().unwrap().closes, 2);
    assert_eq!(bodies(&server).await[1]["state"]["recentActions"][0]["result"], "NOT executed: agente não respondeu");
    assert_eq!(bodies(&server).await[1]["state"]["recentActions"][0]["screenChanged"], false);
    assert!(r.tempos.iter().any(|t| t.starts_with("reconexao=")));
}

#[tokio::test]
async fn cancel_mid_run_sends_no_new_action() {
    let con = FakeConnector::new(vec![tree("o1", ""), tree("o2", "Ana")]);
    let cancel = CancellationToken::new();
    con.state.lock().unwrap().cancel_after_act = Some(cancel.clone());
    let (op, server) = setup(vec![select("set_value"), done()]).await;
    let r = executar(op, &con, cancel, &|_| {}).await;
    assert_eq!(r.motivo, "RuntimeError: objetivo cancelado; nenhuma nova ação será enviada");
    assert_eq!(con.state.lock().unwrap().acts.len(), 1);
    assert_eq!(con.state.lock().unwrap().closes, 1);
    assert_eq!(bodies(&server).await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn budget_exhausted_stops_with_limit_message() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    con.state.lock().unwrap().observe_pending = true;
    let (mut op, _server) = setup(vec![]).await;
    op.limite = seconds(2);
    let start = Instant::now();
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "TimeoutError: limite de duração atingido; objetivo não confirmado");
    assert!(start.elapsed() <= seconds(2));
    assert_eq!(con.state.lock().unwrap().closes, 1);
    assert!(con.state.lock().unwrap().acts.is_empty());
}

#[tokio::test]
async fn lock_busy_message() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (op, _server) = setup(vec![]).await;
    let user = ["LOGNAME", "USER", "LNAME", "USERNAME"].iter().find_map(|k| std::env::var(k).ok().filter(|s| !s.is_empty())).unwrap();
    let key = format!("{user}:{}", op.config.as_ref().unwrap().display());
    let digest: String = Sha256::digest(key.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    let path = std::env::temp_dir().join(format!("hangar-computer-control-{}.lock", &digest[..12]));
    let mut abrir = OpenOptions::new();
    abrir.create(true).read(true).append(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        abrir.share_mode(0);
    }
    let lock = abrir.open(path).unwrap();
    lock.try_lock().unwrap();
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "outro objetivo ainda está controlando o desktop");
    assert_eq!(con.state.lock().unwrap().connects, 0);
    assert!(r.registro.is_none());
    assert!(r.resumo().starts_with("parou: outro objetivo ainda está controlando o desktop\n"));
}

#[tokio::test]
async fn whole_run_never_leaks_secret() {
    let secret = "sëgredo-123";
    let mut before = tree("o1", "");
    before.elements[0].name = format!("Nome {secret}");
    let mut after = before.clone();
    after.observation_id = "o2".into();
    after.elements[0].value = Some(format!("echo: {secret}"));
    let con = FakeConnector::new(vec![before, after]);
    let (mut op, server) = setup(vec![select("set_value"), Answer::Choice("BLOCKED", 0.9, 0.05),
        help(json!([])), done()]).await;
    op.dados = json!({"password":secret}).as_object().unwrap().clone();
    let progress = Mutex::new(vec![]);
    let r = executar(op, &con, CancellationToken::new(), &|p| progress.lock().unwrap().push(p)).await;
    assert_eq!(con.state.lock().unwrap().acts[0].0.value.as_deref(), Some(secret));
    assert!(!r.resumo().contains(secret));
    assert!(!format!("{:?}", progress.lock().unwrap()).contains(secret));
    for body in bodies(&server).await {
        assert!(!body.to_string().contains(secret), "{body}");
    }
    let dir = r.registro.as_ref().unwrap();
    for file in ["observacao.json", "jev.jsonl"] {
        assert!(!std::fs::read_to_string(dir.join(file)).unwrap().contains(secret), "{file}");
    }
    let observation: Value = serde_json::from_slice(&std::fs::read(dir.join("observacao.json")).unwrap()).unwrap();
    assert_eq!(observation["elements"][0]["value"], "echo: <senha>");
    assert_eq!(r.captura, Some(dir.join("tela.png")));
}

#[test]
fn escaped_agent_echoes_mask_only_the_full_secret() {
    for (senha, eco) in [
        ("ab\u{a0}\\c9", "campo ficou com 'xab\u{a0}\\\\c9'"),
        ("ab\u{200b}\u{e000}\u{f0000}\\c9", "campo ficou com 'xab\\u200b\\ue000\u{f0000}\\\\c9'"),
    ] {
        let normal = "ab\u{a0}\\c8";
        let dados = json!({"senha": senha, "nome": normal}).as_object().unwrap().clone();
        let estado = hcc_laco::estado::estado_compacto("preencher", &dados, &tree("o1", normal), &[], Some(eco));
        assert_eq!(estado["hint"], "campo ficou com 'x<senha>'");
        assert_eq!(estado["data"]["senha"], "<senha>");
        assert_eq!(estado["data"]["nome"], normal);
        assert_eq!(estado["elements"][0]["value"], normal);
    }
}

#[tokio::test]
async fn echoed_readback_with_backslash_is_masked() {
    fn sem_segredo(valor: &Value, formas: &[&str]) {
        match valor {
            Value::String(s) => {
                for forma in formas { assert!(!s.contains(forma), "segredo {forma:?} exposto em {s:?}"); }
                if let Ok(interno) = serde_json::from_str::<Value>(s) { sem_segredo(&interno, formas); }
            }
            Value::Array(a) => a.iter().for_each(|v| sem_segredo(v, formas)),
            Value::Object(m) => {
                for (k, v) in m { sem_segredo(&json!(k), formas); sem_segredo(v, formas); }
            }
            _ => {}
        }
    }
    for (senha, simples, duplas, lido) in [
        ("ab\\c9", "ab\\\\c9", "ab\\\\c9", "'xab\\\\c9'"),
        ("ab'c9", "ab\\'c9", "ab'c9", "\"xab'c9\""),
        ("ab\"c9", "ab\"c9", "ab\\\"c9", "'xab\"c9'"),
        ("ab'\"c9", "ab\\'\"c9", "ab'\\\"c9", "'xab\\'\"c9'"),
        ("ab\r\n\t\0c9", "ab\\r\\n\\t\\x00c9", "ab\\r\\n\\t\\x00c9", "'xab\\r\\n\\t\\x00c9'"),
        ("ab\u{200b}\u{e000}\u{f0000}c9", "ab\\u200b\\ue000\\U000f0000c9", "ab\\u200b\\ue000\\U000f0000c9", "'xab\\u200b\\ue000\\U000f0000c9'"),
        ("ab\u{a0}\\c9", "ab\\xa0\\\\c9", "ab\\xa0\\\\c9", "'xab\u{a0}\\\\c9'"),
        ("ab\u{200b}\u{e000}\u{f0000}\\c9", "ab\\u200b\\ue000\\U000f0000\\\\c9", "ab\\u200b\\ue000\\U000f0000\\\\c9", "'xab\\u200b\\ue000\u{f0000}\\\\c9'"),
    ] {
        let mut obs = tree("o1", &format!("raw: {senha}; single: {simples}; double: {duplas}"));
        obs.elements[0].password = true;
        obs.elements[0].name = format!("Senha {simples} | {duplas}");
        obs.windows[0].name = format!("Editor {simples} | {duplas}");
        let erro = format!("valor digitado não confirmado; campo ficou com {lido}");
        let con = FakeConnector::new(vec![obs]);
        con.state.lock().unwrap().act_errors.push_back(SessionError::Agent(erro));
        let (mut op, server) = setup(vec![select("set_value"), Answer::Choice("BLOCKED", 0.9, 0.05),
            Answer::Vision(json!({"interpretacao":format!("campo ficou com {lido}"), "acoes":[]})), done(),
            Answer::Choice("WAIT", 0.9, 0.05)]).await;
        op.dados = json!({"senha":senha}).as_object().unwrap().clone();
        let progress = Mutex::new(vec![]);
        let r = executar(op, &con, CancellationToken::new(), &|p| progress.lock().unwrap().push(p)).await;
        let requests = bodies(&server).await;
        assert_eq!(requests.len(), 5, "{}", r.resumo());
        assert!(requests[4]["state"].get("hint").is_none());
        assert_eq!(requests[1]["state"]["recentActions"][0]["result"],
            format!("NOT executed: valor digitado não confirmado; campo ficou com {}x<senha>{}",
                &lido[..1], &lido[lido.len() - 1..]));
        let llm_estado: Value = serde_json::from_str(requests[2]["messages"][1]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(llm_estado["recentActions"], requests[1]["state"]["recentActions"]);
        assert_eq!(requests[3]["state"]["hint"], format!("campo ficou com {}x<senha>{}", &lido[..1], &lido[lido.len() - 1..]));
        let formas = [senha, simples, duplas];
        for body in requests { sem_segredo(&body, &formas); }
        let dir = r.registro.as_ref().unwrap();
        for file in ["jev.jsonl", "observacao.json"] {
            for linha in std::fs::read_to_string(dir.join(file)).unwrap().lines() {
                sem_segredo(&serde_json::from_str::<Value>(linha).unwrap(), &formas);
            }
        }
        sem_segredo(&json!(r.passos), &formas);
        sem_segredo(&json!(r.resumo()), &formas);
        sem_segredo(&json!(format!("{:?}", progress.lock().unwrap())), &formas);
        assert_eq!(r.passos.len(), 1);
        assert!(r.resumo().contains("<senha>"));
    }
}

#[tokio::test]
async fn broken_config_is_parou_value_error() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    con.state.lock().unwrap().connect_error = Some(SessionError::Config("vm-a-agent.json: JSON inválido".into()));
    let (op, _server) = setup(vec![]).await;
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "ValueError: vm-a-agent.json: JSON inválido");
    assert!(r.registro.is_some());
    assert!(r.tempos.iter().any(|t| t.starts_with("conexao=")));
    assert!(r.resumo().contains("registro:"));
}

#[tokio::test]
async fn validation_precedes_connection_and_keeps_summary() {
    for (text, count) in [("   ", 12), ("preencher", 0), ("preencher", 51)] {
        let con = FakeConnector::new(vec![tree("o1", "")]);
        let (mut op, _server) = setup(vec![]).await;
        op.texto = text.into(); op.max_passos = count;
        let r = run(op, &con).await;
        assert_eq!(r.motivo, "ValueError: objetivo vazio ou max_passos fora de 1..50");
        assert_eq!(con.state.lock().unwrap().connects, 0);
        assert!(r.registro.is_none());
        assert_eq!(r.tempos.len(), 1);
        assert!(r.resumo().ends_with("sem captura"));
    }
}

#[tokio::test]
async fn missing_credentials_stop_before_creating_registro() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (mut op, _server) = setup(vec![]).await;
    op.jev = None;
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "ValueError: faltam TYPESAFE_API_KEY ou HCC_AGENT_CONFIG");
    assert!(r.registro.is_none());
    assert_eq!(con.state.lock().unwrap().connects, 0);
}

#[tokio::test]
async fn close_failure_revokes_success() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    con.state.lock().unwrap().close_error = Some("falha de limpeza".into());
    let (op, _server) = setup(vec![done()]).await;
    let r = run(op, &con).await;
    assert!(!r.ok);
    assert_eq!(r.motivo, "objetivo confirmado (95%) na janela 'Editor'; falha ao fechar sessão: falha de limpeza");
}

#[tokio::test]
async fn observe_retries_only_transient_runtime_errors() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    con.state.lock().unwrap().observe_errors = [SessionError::Agent("janela ativa mudou".into()),
        SessionError::Agent("COMError: falha temporária".into())].into();
    let (op, _server) = setup(vec![done()]).await;
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(con.state.lock().unwrap().observed.len(), 3);
    let con = FakeConnector::new(vec![tree("o1", "")]);
    con.state.lock().unwrap().observe_errors.push_back(SessionError::Agent("erro não transitório".into()));
    let (op, _server) = setup(vec![]).await;
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "RuntimeError: erro não transitório");
    assert_eq!(con.state.lock().unwrap().observed.len(), 1);
}

#[tokio::test]
async fn invalid_observation_stops_without_deciding() {
    let mut obs = tree("", ""); obs.connected = false;
    let con = FakeConnector::new(vec![obs]);
    let (op, server) = setup(vec![]).await;
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "RuntimeError: desktop desconectado ou observação sem referência");
    assert!(bodies(&server).await.is_empty());
}

#[tokio::test]
async fn outside_visual_click_never_reaches_risk_or_act() {
    let mut obs = empty("o1"); obs.windows[0].rect = Rect(200,200,300,300);
    let con = FakeConnector::new(vec![obs]);
    let (op, _server) = setup(vec![help(json!([{"type":"mouse","x":10,"y":10,"button":"left","mode":"click"}])),
        Answer::Choice("BLOCKED", 0.9, 0.05)]).await;
    let r = run(op, &con).await;
    assert!(r.motivo.contains("sem ação segura"));
    assert!(con.state.lock().unwrap().acts.is_empty());
}

#[tokio::test]
async fn budget_caps_hanging_jev_request() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let server = MockServer::start().await;
    Mock::given(path("/jev")).respond_with(ResponseTemplate::new(200).set_delay(seconds(5)).set_body_json(json!({}))).mount(&server).await;
    let (mut op, _server) = setup(vec![]).await;
    op.jev = Some(Jev::new(format!("{}/jev", server.uri()), "fake-key".into(), JEV_MODEL.into()));
    op.limite = Duration::from_millis(50);
    let start = Instant::now();
    let r = run(op, &con).await;
    assert!(r.motivo.starts_with("TimeoutError:"), "{}", r.resumo());
    assert!(start.elapsed() < seconds(1));
    assert!(con.state.lock().unwrap().acts.is_empty());
    assert_eq!(con.state.lock().unwrap().closes, 1);
}

#[tokio::test]
async fn cycle_limit_never_sends_extra_action() {
    let con = FakeConnector::new(vec![tree("o1", ""), tree("o2", "Ana")]);
    let (mut op, _server) = setup(vec![select("set_value"), select("invoke")]).await;
    op.max_passos = 1;
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "limite de 1 ciclos; objetivo não confirmado");
    assert_eq!(con.state.lock().unwrap().acts.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_cleanup_keeps_lock_without_delaying_result() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (release, wait) = std::sync::mpsc::channel();
    let (entered, entered_rx) = std::sync::mpsc::channel();
    let (returned, returned_rx) = std::sync::mpsc::channel();
    let slow = Arc::new(fakes::SlowDrop { entered,
        release: Mutex::new(wait), finished: tokio::sync::Notify::new() });
    {
        let mut s = con.state.lock().unwrap();
        s.close_pending = true;
        s.slow_drop = Some(slow.clone());
    }
    let (mut op, _server) = setup(vec![done()]).await;
    let user = ["LOGNAME", "USER", "LNAME", "USERNAME"].iter().find_map(|k| std::env::var(k).ok().filter(|s| !s.is_empty())).unwrap();
    let key = format!("{user}:{}", op.config.as_ref().unwrap().display());
    let digest: String = Sha256::digest(key.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    let lock_path = std::env::temp_dir().join(format!("hangar-computer-control-{}.lock", &digest[..12]));
    op.limite = Duration::from_millis(200);
    let observer = std::thread::spawn(move || {
        let entered = entered_rx.recv_timeout(seconds(3)).is_ok();
        let returned = returned_rx.recv_timeout(Duration::from_millis(100)).is_ok();
        let busy = if returned {
            let file = OpenOptions::new().read(true).append(true).open(lock_path).unwrap();
            matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
        } else { false };
        release.send(()).unwrap();
        (entered, returned, busy)
    });
    let child = con.clone();
    let task = tokio::spawn(async move {
        let r = run(op, &child).await;
        let delivered = returned.send(()).is_ok();
        (r, delivered)
    });
    let (r, delivered) = task.await.unwrap();
    let (entered, returned, busy) = observer.join().unwrap();
    tokio::time::timeout(seconds(3), slow.finished.notified()).await.unwrap();
    assert!(entered);
    assert!(returned && delivered, "Drop lento atrasou a devolução do resultado");
    assert!(busy, "lock liberado antes da limpeza terminar");
    assert!(r.ok, "{}", r.resumo());
    assert!(!r.motivo.contains("falha ao fechar sessão"));
}

#[tokio::test]
async fn secret_llm_error_is_masked_before_truncating() {
    let secret = "XYZ".repeat(35);
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (mut op, server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05), done(), Answer::Choice("WAIT", 0.9, 0.05)]).await;
    let url = format!("{}/bad", server.uri());
    let prefix = format!("HTTP 500 em {url}: ");
    let detail = format!("{}{secret}", "a".repeat(280 - prefix.chars().count()));
    Mock::given(path("/bad")).respond_with(ResponseTemplate::new(500).set_body_string(detail)).with_priority(1).mount(&server).await;
    op.dados = json!({"password":secret}).as_object().unwrap().clone();
    op.llm = Llm::new(url, "fake-model".into(), None, Some("fake-key".into()));
    let r = run(op, &con).await;
    assert!(!r.motivo.contains("XYZ"), "{}", r.resumo());
    assert!(r.motivo.contains("ajuda visual indisponível: RuntimeError: HTTP 500"));
    assert!(r.motivo.contains("<senha>"));
    for body in bodies(&server).await { assert!(!body.to_string().contains("XYZ")); }
}

#[tokio::test]
async fn numeric_secret_echo_is_masked_in_models() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (mut op, server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05), help(json!([])), done()]).await;
    op.dados = json!({"pin":123456,"confirmacao":123456}).as_object().unwrap().clone();
    let r = run(op, &con).await;
    assert!(!r.ok);
    for body in bodies(&server).await {
        assert!(!body.to_string().contains("123456"), "PIN ecoado chegou ao modelo: {body}");
    }
}

#[tokio::test]
async fn escaped_secret_in_proposal_never_reaches_descriptions() {
    let con = FakeConnector::new(vec![tree("o1", ""), tree("o2", "fim")]);
    let (mut op, server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05),
        help(json!([{"type":"text","target":"e0","value":"prefixo ab\\cd"}])), select("text 'prefixo"), done()]).await;
    op.dados = json!({"password":"ab\\cd"}).as_object().unwrap().clone();
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(r.passos[0], "1. text 'prefixo <senha>' em Edit 'Nome' [90%]");
    assert_eq!(con.state.lock().unwrap().acts[0].0.value.as_deref(), Some("prefixo ab\\cd"));
    assert!(!bodies(&server).await[2].to_string().contains("ab\\"));
    assert!(!std::fs::read_to_string(r.registro.unwrap().join("jev.jsonl")).unwrap().contains("ab\\"));
}

#[tokio::test]
async fn secret_warning_masks_before_truncating() {
    let secret = "XYZ".repeat(35);
    let mut one = tree("o1", "1");
    let mut warning = one.elements[0].clone();
    warning.id = "warning".into(); warning.role = "Text".into();
    warning.value = None; warning.actions.clear();
    warning.name = format!("{}{secret}", "a".repeat(350));
    one.elements.push(warning);
    let mut two = one.clone(); two.observation_id = "o2".into();
    two.elements.last_mut().unwrap().name = "outro aviso diferente e comprido".into();
    let mut three = one.clone(); three.observation_id = "o3".into();
    let con = FakeConnector::new(vec![one, two, three]);
    let (mut op, _server) = setup(vec![select("keys Enter"), select("keys Tab")]).await;
    op.dados = json!({"password":secret}).as_object().unwrap().clone();
    let r = run(op, &con).await;
    assert!(!r.motivo.contains("XYZ"), "{}", r.resumo());
    assert_eq!(r.motivo, format!("o aplicativo respondeu duas vezes com o mesmo aviso: {}<senha>", "a".repeat(350)));
}

#[tokio::test]
async fn invalid_goal_precedes_oversized_secret_mask() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (mut op, _server) = setup(vec![]).await;
    op.texto.clear();
    op.dados = json!({"password":"x".repeat(1_000_000)}).as_object().unwrap().clone();
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "ValueError: objetivo vazio ou max_passos fora de 1..50");
    assert_eq!(con.state.lock().unwrap().connects, 0);
}

#[cfg(unix)]
#[tokio::test]
async fn lock_file_io_obeys_budget() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (mut op, _server) = setup(vec![]).await;
    op.limite = Duration::from_millis(50);
    let user = ["LOGNAME", "USER", "LNAME", "USERNAME"].iter().find_map(|k| std::env::var(k).ok().filter(|s| !s.is_empty())).unwrap();
    let key = format!("{user}:{}", op.config.as_ref().unwrap().display());
    let digest: String = Sha256::digest(key.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    let path = std::env::temp_dir().join(format!("hangar-computer-control-{}.lock", &digest[..12]));
    assert!(tokio::process::Command::new("mkfifo").arg(&path).status().await.unwrap().success());
    let result = tokio::time::timeout(Duration::from_millis(300), run(op, &con)).await;
    let reader = tokio::fs::OpenOptions::new().read(true).open(&path).await.unwrap();
    drop(reader);
    std::fs::remove_file(path).unwrap();
    let r = result.expect("I/O do lock ultrapassou o orçamento total");
    assert_eq!(r.motivo, "TimeoutError: limite de duração atingido; objetivo não confirmado");
    assert_eq!(con.state.lock().unwrap().connects, 0);
}

#[tokio::test]
async fn oversized_secret_mask_fails_closed() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (mut op, server) = setup(vec![done()]).await;
    op.dados = json!({"password":"x".repeat(1_000_000)}).as_object().unwrap().clone();
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "ValueError: não foi possível preparar a proteção dos segredos");
    assert!(bodies(&server).await.is_empty());
    assert_eq!(con.state.lock().unwrap().connects, 0);
}

#[test]
fn summary_keeps_python_line_order() {
    let r = Resultado { ok: true, motivo: "pronto".into(), passos: vec!["1. ação [90%]".into()],
        tempos: vec!["conexao=0.01s".into(), "total=1.00s".into()],
        captura: Some(PathBuf::from("/registro/tela.png")), registro: Some(PathBuf::from("/registro")) };
    assert_eq!(r.resumo(), "concluído: pronto\n1. ação [90%]\ntempos: conexao=0.01s; total=1.00s\ncaptura: /registro/tela.png\nregistro: /registro");
}

#[tokio::test]
async fn llm_uncaught_error_stops_like_python() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (mut op, server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05), done()]).await;
    let url = format!("{}/semchoices", server.uri());
    Mock::given(path("/semchoices")).respond_with(ResponseTemplate::new(200).set_body_string("{}")).with_priority(1).mount(&server).await;
    op.llm = Llm::new(url, "fake-model".into(), None, Some("fake-key".into()));
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "KeyError: 'choices'", "{}", r.resumo());
}

#[tokio::test]
async fn keys_proposal_never_leaks_secret() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    let (mut op, server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05),
        help(json!([{"type":"keys","keys":["hunter2"]}])), Answer::Choice("BLOCKED", 0.9, 0.05)]).await;
    op.dados = json!({"password":"hunter2"}).as_object().unwrap().clone();
    let r = run(op, &con).await;
    for body in bodies(&server).await {
        if body.get("messages").is_none() { assert!(!body.to_string().contains("hunter2"), "{body}"); }
    }
    assert!(!r.resumo().contains("hunter2"), "{}", r.resumo());
}

#[tokio::test]
async fn budget_exhausted_with_slow_close_keeps_python_message() {
    let con = FakeConnector::new(vec![tree("o1", "")]);
    {
        let mut s = con.state.lock().unwrap();
        s.observe_pending = true;
        s.close_pending = true;
    }
    let (mut op, _server) = setup(vec![]).await;
    op.limite = seconds(1);
    let r = run(op, &con).await;
    assert_eq!(r.motivo, "TimeoutError: limite de duração atingido; objetivo não confirmado");
}

fn tab_and_body(id: &str, text: &str) -> Observation {
    serde_json::from_value(json!({
        "observation_id": id, "connected": true, "session_id": 1, "foreground": "w1",
        "windows": [{"id": "w1", "name": "Editor", "process_id": 10, "class_name": "gedit", "rect": [0,0,800,600]}],
        "elements": [
            {"id":"t1", "name":"Sem título", "role":"TabItem", "value":null, "enabled":true,
             "rect":null, "focused":true, "actions":["select"]},
            {"id":"e1", "name":"Corpo", "role":"Edit", "value":text, "enabled":true,
             "rect":null, "focused":false, "actions":["focus"]},
        ],
        "truncated":false, "timestamp":1.0, "screen":{"width":800,"height":600},
    })).unwrap()
}

#[tokio::test]
async fn vision_text_goes_to_the_edit_not_the_focus() {
    let con = FakeConnector::new(vec![tab_and_body("o1", ""), tab_and_body("o2", "ação")]);
    let (mut op, server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05), help(json!([{"type":"text","value":"ação"}])),
        select("text 'ação' em Edit 'Corpo'"), done()]).await;
    op.texto = "escrever ação no editor".into();
    op.dados = Map::new();
    let r = run(op, &con).await;
    let acts: Vec<_> = con.state.lock().unwrap().acts.iter().map(|(a, _, _)| a.clone()).collect();
    assert_eq!(acts, vec![action(json!({"type":"text","target":"e1","value":"ação"}))], "{}", r.resumo());
    assert!(r.ok, "{}", r.resumo());
    assert_eq!(r.passos, vec!["1. text 'ação' em Edit 'Corpo' [90%]"]);
    let criteria = bodies(&server).await[2]["questions"]["action"]["criteria"].to_string();
    assert!(!criteria.contains("no foco atual"), "{criteria}");
}

#[tokio::test]
async fn secret_waits_for_named_combo() {
    let secret = "sëgredo-combo-123";
    let con = FakeConnector::new(vec![tree("o1", ""), tree("o2", "busca"), tree("o3", secret)]);
    let (mut op, server) = setup(vec![Answer::Choice("BLOCKED", 0.9, 0.05),
        help(json!([{"type":"set_value","target":"e0","value":secret}, {"type":"keys","keys":["CTRL","F"]}])),
        select("keys CTRL+F"), select("set_value Edit 'Nome' = ***"), done()]).await;
    op.texto = "pressionar Ctrl+F e digitar a senha".into();
    op.dados = json!({"senha":secret}).as_object().unwrap().clone();
    let r = run(op, &con).await;
    let acts: Vec<_> = con.state.lock().unwrap().acts.iter().map(|(a, _, _)| a.clone()).collect();
    assert_eq!(acts, vec![
        action(json!({"type":"keys","keys":["CTRL","F"]})),
        action(json!({"type":"set_value","target":"e0","value":secret})),
    ], "{}", r.resumo());
    assert!(r.ok, "{}", r.resumo());
    let requests = bodies(&server).await;
    for i in [0, 2] {
        let criteria = requests[i]["questions"]["action"]["criteria"].to_string();
        assert!(!criteria.contains("= ***"), "request {i}: {criteria}");
    }
    assert!(requests[3]["questions"]["action"]["criteria"].to_string().contains("set_value Edit 'Nome' = ***"));
    for body in requests { assert!(!body.to_string().contains(secret), "{body}"); }
}

#[tokio::test]
async fn drawn_screen_text_with_two_fields_is_jev_choice() {
    let mut o1 = tab_and_body("o1", "");
    o1.elements[0].actions = vec![hcc_protocolo::ElementAction::Focus];
    let mut segundo = o1.elements[1].clone();
    (segundo.id, segundo.name) = ("e2".into(), "Rodapé".into());
    o1.elements.push(segundo);
    let con = FakeConnector::new(vec![o1, tree("o2", "x")]);
    let (mut op, server) = setup(vec![help(json!([{"type":"text","value":"x"}])), select("text 'x' em Edit 'Rodapé'"), done()]).await;
    op.texto = "escrever x".into();
    op.dados = Map::new();
    let r = run(op, &con).await;
    let acts: Vec<_> = con.state.lock().unwrap().acts.iter().map(|(a, _, _)| a.clone()).collect();
    assert_eq!(acts, vec![action(json!({"type":"text","target":"e2","value":"x"}))], "{}", r.resumo());
    assert!(r.ok, "{}", r.resumo());
    assert!(bodies(&server).await[1]["questions"].get("action").is_some(), "no direct guess between fields");
}

#[tokio::test]
async fn app_closed_by_the_loop_is_not_offered_to_launch_again() {
    let mut aberto = tree("o1", "");
    aberto.apps = vec!["Calculator => gnome-calculator".into(), "Firefox => firefox".into()];
    aberto.windows[0].class_name = "org.gnome.Calculator".into();
    let mut fechado = aberto.clone();
    fechado.observation_id = "o2".into();
    fechado.windows[0].id = "w2".into();
    fechado.windows[0].process_id = 20;
    fechado.foreground = "w2".into();
    let con = FakeConnector::new(vec![aberto.clone(), fechado]);
    let (mut op, server) = setup(vec![select("Salvar"), done()]).await;
    op.texto = "fechar a Calculadora".into();
    op.dados = Map::new();
    let r = run(op, &con).await;
    assert!(r.ok, "{}", r.resumo());
    let decisoes: Vec<Value> = bodies(&server).await.into_iter().filter(|b| b["questions"].get("action").is_some()).collect();
    let lanca = |b: &Value, app: &str| b["questions"]["action"]["criteria"].as_object().unwrap().values()
        .any(|v| v.as_str().unwrap().starts_with(&format!("launch '{app}'")));
    let tem_launch = |b: &Value| lanca(b, "Calculator");
    assert!(tem_launch(&decisoes[0]), "before closing, launching is still an option");
    assert!(!tem_launch(&decisoes[1]), "the app the loop just closed must not be reopened");
    assert!(lanca(&decisoes[1], "Firefox"), "other apps can still be launched");

    // A dialog closing leaves its app's main window: launching stays available.
    let mut com_dialogo = aberto.clone();
    com_dialogo.windows.push(hcc_protocolo::Window { id: "w9".into(), name: "Salvar como".into(), process_id: 10,
        class_name: "dialog".into(), rect: Rect(0, 0, 100, 100) });
    com_dialogo.foreground = "w9".into();
    let mut sem_dialogo = aberto.clone();
    sem_dialogo.observation_id = "o2".into();
    let con = FakeConnector::new(vec![com_dialogo, sem_dialogo]);
    let (mut op, server) = setup(vec![select("Salvar"), done()]).await;
    op.texto = "fechar a Calculadora".into();
    op.dados = Map::new();
    run(op, &con).await;
    let decisoes: Vec<Value> = bodies(&server).await.into_iter().filter(|b| b["questions"].get("action").is_some()).collect();
    assert!(tem_launch(&decisoes[1]));

    // From the bare desktop (pid 0 pseudo-window), opening an app is not "an app closed".
    let mut mesa = aberto.clone();
    mesa.windows = vec![hcc_protocolo::Window { id: "desktop".into(), name: "Área de trabalho".into(), process_id: 0,
        class_name: "desktop".into(), rect: Rect(0, 0, 800, 600) }];
    mesa.foreground = "desktop".into();
    let mut app = aberto.clone();
    app.observation_id = "o2".into();
    let con = FakeConnector::new(vec![mesa, app]);
    let (mut op, server) = setup(vec![select("Salvar"), done()]).await;
    op.texto = "abrir a Calculadora".into();
    op.dados = Map::new();
    run(op, &con).await;
    let decisoes: Vec<Value> = bodies(&server).await.into_iter().filter(|b| b["questions"].get("action").is_some()).collect();
    assert!(tem_launch(&decisoes[1]), "the desktop pseudo-window disappearing is not an app closing");

    // A dialog of another process (file portal) closing blocks nothing it does not name.
    let mut portal = aberto.clone();
    portal.windows.push(hcc_protocolo::Window { id: "w8".into(), name: "Salvar arquivo".into(), process_id: 30,
        class_name: "xdg-desktop-portal-gtk".into(), rect: Rect(0, 0, 100, 100) });
    portal.foreground = "w8".into();
    let mut sem_portal = aberto.clone();
    sem_portal.observation_id = "o2".into();
    let con = FakeConnector::new(vec![portal, sem_portal]);
    let (mut op, server) = setup(vec![select("Salvar"), done()]).await;
    op.texto = "salvar o arquivo e abrir a Calculadora".into();
    op.dados = Map::new();
    run(op, &con).await;
    let decisoes: Vec<Value> = bodies(&server).await.into_iter().filter(|b| b["questions"].get("action").is_some()).collect();
    assert!(tem_launch(&decisoes[1]), "a portal dialog closing is not the Calculator closing");
}
