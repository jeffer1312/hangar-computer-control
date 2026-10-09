use hcc_controlador::servidor::Server;
use hcc_protocolo::SessionError;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn client() -> Client {
    Client::builder().no_proxy().timeout(Duration::from_secs(4)).build().unwrap()
}

fn hello() -> Value {
    json!({"hello":true,"boot":"agent-a","session_id":1,"pid":123,"protocol":2})
}

async fn post(server: &Server, body: Value) -> reqwest::Response {
    client().post(&server.connection().url)
        .bearer_auth(&server.connection().token).json(&body).send().await.unwrap()
}

#[tokio::test]
async fn bad_token_is_403() {
    let mut server = Server::start(CancellationToken::new()).await.unwrap();
    let response = client().post(&server.connection().url).bearer_auth("wrong")
        .json(&hello()).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(!server.startup_status().unwrap());
    server.close().await;
}

#[tokio::test]
async fn bad_path_is_403() {
    let mut server = Server::start(CancellationToken::new()).await.unwrap();
    let response = client().post(server.connection().url.replace("/next", "/other"))
        .bearer_auth(&server.connection().token).json(&hello()).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    server.close().await;
}

#[tokio::test]
async fn different_boot_is_400() {
    let mut server = Server::start(CancellationToken::new()).await.unwrap();
    assert_eq!(post(&server, hello()).await.status(), StatusCode::OK);
    let body = json!({"boot":"agent-b","session_id":1,"pid":123});
    assert_eq!(post(&server, body).await.status(), StatusCode::BAD_REQUEST);
    server.close().await;
}

#[tokio::test]
async fn hello_without_protocol_2_fails_startup_with_update_message() {
    for protocol in [json!(1), Value::Null] {
        let mut server = Server::start(CancellationToken::new()).await.unwrap();
        let mut body = hello();
        body["protocol"] = protocol.clone();
        assert_eq!(post(&server, body).await.status(), StatusCode::BAD_REQUEST);
        let error = server.startup_status().unwrap_err();
        assert!(matches!(error, SessionError::Startup(_)));
        let number = if protocol.is_null() { "None" } else { "1" };
        assert_eq!(error.to_string(), format!("agente incompatível (protocolo {number}); atualize windows-agent.exe"));
        server.close().await;
    }
}

#[tokio::test]
async fn body_over_16mib_rejected() {
    let mut server = Server::start(CancellationToken::new()).await.unwrap();
    let response = client().post(&server.connection().url)
        .bearer_auth(&server.connection().token)
        .body(vec![b' '; 16 * 1024 * 1024 + 1]).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!server.startup_status().unwrap());
    server.close().await;
}

#[tokio::test]
async fn malformed_identity_or_missing_hello_is_400() {
    let mut server = Server::start(CancellationToken::new()).await.unwrap();
    for body in [json!({"boot":1,"session_id":1,"pid":123}),
        json!({"boot":"agent-a","session_id":0,"pid":123}),
        json!({"boot":"agent-a","session_id":1,"pid":123})] {
        assert_eq!(post(&server, body).await.status(), StatusCode::BAD_REQUEST);
        assert!(!server.startup_status().unwrap());
    }
    server.close().await;
}

#[tokio::test]
async fn idle_poll_gets_heartbeat_within_2_5s() {
    let mut server = Server::start(CancellationToken::new()).await.unwrap();
    let began = Instant::now();
    let response = post(&server, hello()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Value>().await.unwrap(), json!({"operation":"heartbeat"}));
    assert!(began.elapsed() <= Duration::from_millis(2500));
    assert!(server.startup_status().unwrap());
    server.close().await;
}

#[tokio::test]
async fn cancellation_returns_stop_without_waiting_for_heartbeat() {
    let cancel = CancellationToken::new();
    let mut server = Server::start(cancel.clone()).await.unwrap();
    cancel.cancel();
    let response = post(&server, hello()).await;
    assert_eq!(response.json::<Value>().await.unwrap(), json!({"operation":"stop"}));
    server.close().await;
}

async fn partial_request(server: &Server, token: &str) -> tokio::net::TcpStream {
    use tokio::io::AsyncWriteExt;
    let url = reqwest::Url::parse(&server.connection().url).unwrap();
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", url.port().unwrap())).await.unwrap();
    let headers = format!("POST /next HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: 1024\r\nExpect: 100-continue\r\n\r\n");
    stream.write_all(headers.as_bytes()).await.unwrap();
    stream
}

async fn read_status(stream: &mut tokio::net::TcpStream, timeout: Duration) -> String {
    use tokio::io::AsyncReadExt;
    let mut bytes = vec![0; 1024];
    let length = tokio::time::timeout(timeout, stream.read(&mut bytes)).await
        .expect("HTTP request remained blocked").unwrap();
    String::from_utf8(bytes[..length].to_vec()).unwrap()
}

#[tokio::test]
async fn bad_token_is_rejected_before_body_is_read() {
    let mut server = Server::start(CancellationToken::new()).await.unwrap();
    let mut stream = partial_request(&server, "wrong").await;
    let response = read_status(&mut stream, Duration::from_secs(1)).await;
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    server.close().await;
}

#[tokio::test]
async fn partial_body_has_ten_second_read_limit() {
    let mut server = Server::start(CancellationToken::new()).await.unwrap();
    let mut stream = partial_request(&server, &server.connection().token).await;
    let interim = read_status(&mut stream, Duration::from_secs(1)).await;
    assert!(interim.starts_with("HTTP/1.1 100"), "{interim}");
    let response = read_status(&mut stream, Duration::from_secs(12)).await;
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    server.close().await;
}

#[tokio::test]
async fn close_interrupts_partial_body_read() {
    let mut server = Server::start(CancellationToken::new()).await.unwrap();
    let mut stream = partial_request(&server, &server.connection().token).await;
    let interim = read_status(&mut stream, Duration::from_secs(1)).await;
    assert!(interim.starts_with("HTTP/1.1 100"), "{interim}");
    tokio::time::timeout(Duration::from_secs(1), server.close()).await
        .expect("server close did not interrupt body read");
    let response = read_status(&mut stream, Duration::from_secs(1)).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("stop"));
}
