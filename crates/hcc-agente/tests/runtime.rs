//! The agent runtime against a scripted controller (axum on 127.0.0.1:0) and FakeDesktop.

use axum::{Json, Router, extract::State, routing::post};
use hcc_agente::fake::FakeDesktop;
use hcc_protocolo::{Action, DesktopError, Desktop, Observation};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type Roteiro = Arc<dyn Fn(&[Value]) -> Value + Send + Sync>;

#[derive(Clone)]
struct Controlador {
    recebidos: Arc<Mutex<Vec<Value>>>,
    roteiro: Roteiro,
    token: String,
}

async fn proximo(State(c): State<Controlador>, headers: axum::http::HeaderMap, Json(corpo): Json<Value>) -> Json<Value> {
    assert_eq!(headers["authorization"], format!("Bearer {}", c.token).as_str());
    let mut recebidos = c.recebidos.lock().unwrap();
    recebidos.push(corpo);
    Json((c.roteiro)(&recebidos))
}

/// Serves the script; returns the connection URL and the bodies the agent posted.
async fn controlador(roteiro: impl Fn(&[Value]) -> Value + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<Value>>>) {
    let recebidos = Arc::new(Mutex::new(Vec::new()));
    let estado = Controlador { recebidos: recebidos.clone(), roteiro: Arc::new(roteiro), token: "tok".into() };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/next", listener.local_addr().unwrap());
    let app = Router::new().route("/next", post(proximo)).with_state(estado);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, recebidos)
}

fn observacao(foreground: &str) -> Value {
    json!({"observation_id": "", "connected": true, "session_id": 7, "foreground": foreground,
           "windows": [], "elements": [{"id": "e1", "name": "Salvar", "role": "Button", "value": null,
           "enabled": true, "rect": [0, 0, 10, 10], "focused": false, "actions": ["invoke"]}],
           "truncated": false, "timestamp": 1.5, "screen": {"width": 800, "height": 600}})
}

/// Writes the fake script and the connection file; returns (script, connection).
fn preparar(dir: &Path, url: &str, prefixo: &str) -> (PathBuf, PathBuf) {
    let script = dir.join("desktop.json");
    std::fs::write(&script, json!({"observations": [observacao("w1")], "fail_screenshot": false}).to_string()).unwrap();
    let conexao = dir.join("connection.json");
    std::fs::write(&conexao, format!("{prefixo}{}", json!({"url": url, "token": "tok"}))).unwrap();
    (script, conexao)
}

fn acoes(script: &Path) -> Vec<Value> {
    let caminho = script.with_extension("acts.jsonl");
    std::fs::read_to_string(caminho)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

async fn rodar<D: Desktop + Send + 'static>(desktop: D, conexao: PathBuf) -> i32 {
    tokio::task::spawn_blocking(move || hcc_agente::run(desktop, &conexao)).await.unwrap()
}

fn act(id: &str, observation_id: &Value, ttl_ms: u64) -> Value {
    json!({"operation": "act", "id": id, "ttl_ms": ttl_ms, "observation_id": observation_id,
           "action": {"type": "invoke", "target": "e1"}})
}

fn observe(id: &str) -> Value {
    json!({"operation": "observe", "id": id, "ttl_ms": 15000})
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_first_then_observe_act_stop() {
    let (url, recebidos) = controlador(|r| match r.len() {
        1 => observe("c1"),
        2 => act("c2", &r[1]["result"]["observation_id"], 15000),
        _ => json!({"operation": "stop"}),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (script, conexao) = preparar(dir.path(), &url, "");

    let codigo = rodar(FakeDesktop::load(&script).unwrap(), conexao.clone()).await;

    assert_eq!(codigo, 0);
    assert!(!conexao.exists(), "o arquivo de conexão deve ser apagado");
    let r = recebidos.lock().unwrap();
    assert_eq!(r[0]["hello"], true);
    assert_eq!(r[0]["protocol"], 2);
    assert_eq!(r[0]["session_id"], 7);
    assert_eq!(r[0]["pid"], std::process::id());
    assert_eq!(r[1]["id"], "c1");
    let id = r[1]["result"]["observation_id"].as_str().unwrap();
    assert_eq!(id.len(), 32, "uuid4 hex: {id}");
    assert_eq!(r[1]["result"]["elements"][0]["name"], "Salvar");
    assert_eq!(r[2]["id"], "c2");
    assert_eq!(r[2]["result"], json!({"ok": true}));
    assert!(r[1].get("hello").is_none() && r[1].get("protocol").is_none());
    assert_eq!(r[1]["boot"], r[0]["boot"]);
    let feitas = acoes(&script);
    assert_eq!(feitas.len(), 1);
    assert_eq!(feitas[0]["action"], json!({"type": "invoke", "target": "e1"}));
}

#[tokio::test(flavor = "multi_thread")]
async fn second_act_on_same_observation_is_refused() {
    let (url, recebidos) = controlador(|r| match r.len() {
        1 => observe("c1"),
        2 => act("c2", &r[1]["result"]["observation_id"], 15000),
        3 => act("c3", &r[1]["result"]["observation_id"], 15000),
        _ => json!({"operation": "stop"}),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (script, conexao) = preparar(dir.path(), &url, "");

    assert_eq!(rodar(FakeDesktop::load(&script).unwrap(), conexao).await, 0);

    let r = recebidos.lock().unwrap();
    assert_eq!(r[2]["result"], json!({"ok": true}));
    assert_eq!(r[3]["id"], "c3");
    assert_eq!(r[3]["error"], "RuntimeError: observação expirada; observe novamente");
    assert!(r[3].get("result").is_none());
    assert_eq!(acoes(&script).len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_command_is_not_executed() {
    let (url, recebidos) = controlador(|r| match r.len() {
        1 => observe("c1"),
        2 => act("c2", &r[1]["result"]["observation_id"], 0),
        _ => json!({"operation": "stop"}),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (script, conexao) = preparar(dir.path(), &url, "");

    assert_eq!(rodar(FakeDesktop::load(&script).unwrap(), conexao).await, 0);

    let r = recebidos.lock().unwrap();
    assert_eq!(r[2]["error"], "TimeoutError: comando expirou antes de chegar ao desktop");
    assert!(acoes(&script).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_posts_again_and_unknown_operation_is_value_error() {
    let (url, recebidos) = controlador(|r| match r.len() {
        1 => json!({"operation": "heartbeat"}),
        2 => json!({"operation": "dance", "id": "c9", "ttl_ms": 15000}),
        3 => json!({"operation": "screenshot", "id": "c4", "ttl_ms": 15000}),
        _ => json!({"operation": "stop"}),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (script, conexao) = preparar(dir.path(), &url, "");

    assert_eq!(rodar(FakeDesktop::load(&script).unwrap(), conexao).await, 0);

    let r = recebidos.lock().unwrap();
    assert!(r[1].get("id").is_none() && r[1].get("hello").is_none(), "heartbeat: {}", r[1]);
    assert_eq!(r[2]["id"], "c9");
    assert_eq!(r[2]["error"], "ValueError: operação inválida");
    assert_eq!(r[3]["id"], "c4");
    let png = r[3]["result"]["png"].as_str().unwrap();
    assert!(png.starts_with("iVBORw0KGgo"), "base64 de PNG: {png}");
}

#[test]
fn startup_failure_writes_error_log() {
    let dir = tempfile::tempdir().unwrap();
    let conexao = dir.path().join("connection.json");
    std::fs::write(&conexao, "{não é json").unwrap();
    let script = dir.path().join("desktop.json");
    std::fs::write(&script, json!({"observations": [observacao("w1")]}).to_string()).unwrap();

    assert_eq!(hcc_agente::run(FakeDesktop::load(&script).unwrap(), &conexao), 1);

    let log = std::fs::read_to_string(dir.path().join("connection.error.log")).unwrap();
    assert!(log.contains("connection.json") && log.contains("line 1"), "log: {log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn bom_connection_file_accepted() {
    let (url, recebidos) = controlador(|_| json!({"operation": "stop"})).await;
    let dir = tempfile::tempdir().unwrap();
    let (script, conexao) = preparar(dir.path(), &url, "\u{feff}");

    assert_eq!(rodar(FakeDesktop::load(&script).unwrap(), conexao.clone()).await, 0);

    assert_eq!(recebidos.lock().unwrap().len(), 1);
    assert!(!conexao.exists());
}

/// A desktop that blocks on its own runtime inside `observe` (the Linux AT-SPI tree does).
struct ComRuntimeProprio(FakeDesktop);

impl Desktop for ComRuntimeProprio {
    fn session_id(&self) -> u32 {
        self.0.session_id()
    }
    fn available(&mut self) -> Result<(), DesktopError> {
        self.0.available()
    }
    fn foreground(&mut self) -> Result<String, DesktopError> {
        self.0.foreground()
    }
    fn observe(&mut self) -> Result<Observation, DesktopError> {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {});
        self.0.observe()
    }
    fn act(&mut self, observed: &Observation, action: &Action) -> Result<(), DesktopError> {
        self.0.act(observed, action)
    }
    fn screenshot_png(&mut self) -> Result<Vec<u8>, DesktopError> {
        self.0.screenshot_png()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn desktop_may_block_on_its_own_runtime() {
    let (url, recebidos) = controlador(|r| match r.len() {
        1 => observe("c1"),
        _ => json!({"operation": "stop"}),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (script, conexao) = preparar(dir.path(), &url, "");

    assert_eq!(rodar(ComRuntimeProprio(FakeDesktop::load(&script).unwrap()), conexao).await, 0);

    assert!(recebidos.lock().unwrap()[1]["result"]["observation_id"].is_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_act_still_spends_the_observation() {
    let (url, recebidos) = controlador(|r| match r.len() {
        1 => observe("c1"),
        2 => json!({"operation": "act", "id": "c2", "ttl_ms": 15000, "observation_id": r[1]["result"]["observation_id"],
                    "action": {"type": "invoke", "target": "e1", "senha": "segredo"}}),
        3 => act("c3", &r[1]["result"]["observation_id"], 15000),
        _ => json!({"operation": "stop"}),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (script, conexao) = preparar(dir.path(), &url, "");

    assert_eq!(rodar(FakeDesktop::load(&script).unwrap(), conexao).await, 0);

    let r = recebidos.lock().unwrap();
    assert!(r[2]["error"].as_str().unwrap().starts_with("ValueError: comando inválido: "), "{}", r[2]);
    assert_eq!(r[3]["error"], "RuntimeError: observação expirada; observe novamente");
    assert!(acoes(&script).is_empty());
}

#[test]
fn command_without_id_is_fatal_and_log_omits_the_body() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (url, _recebidos) = rt.block_on(controlador(|_| json!({"operation": "act", "action": {"type": "text", "value": "segredo"}})));
    let dir = tempfile::tempdir().unwrap();
    let (script, conexao) = preparar(dir.path(), &url, "");

    assert_eq!(hcc_agente::run(FakeDesktop::load(&script).unwrap(), &conexao), 1);

    let log = std::fs::read_to_string(dir.path().join("connection.error.log")).unwrap();
    assert!(log.contains("act") && !log.contains("segredo"), "log: {log}");
}
