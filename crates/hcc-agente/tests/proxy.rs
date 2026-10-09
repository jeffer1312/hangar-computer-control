//! Own test binary: it sets proxy env vars, which must not leak into the other tests' process.

use axum::{Json, Router, routing::post};
use hcc_agente::fake::FakeDesktop;
use serde_json::{Value, json};

#[tokio::test(flavor = "multi_thread")]
async fn proxy_env_ignored() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/next", listener.local_addr().unwrap());
    let app = Router::new().route("/next", post(|Json(_): Json<Value>| async { Json(json!({"operation": "stop"})) }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("desktop.json");
    std::fs::write(&script, r#"{"observations": []}"#).unwrap();
    let conexao = dir.path().join("connection.json");
    std::fs::write(&conexao, json!({"url": url, "token": "tok"}).to_string()).unwrap();
    // SAFETY: only test in this binary, set before the agent thread starts.
    unsafe {
        for var in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
            std::env::set_var(var, "http://127.0.0.1:9");
        }
        std::env::remove_var("NO_PROXY");
        std::env::remove_var("no_proxy");
    }

    let codigo = tokio::task::spawn_blocking(move || hcc_agente::run(FakeDesktop::load(&script).unwrap(), &conexao))
        .await
        .unwrap();

    assert_eq!(codigo, 0);
}
