use hcc_protocolo::Connection;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().is_some_and(|argument| argument == "--tunnel") {
        std::future::pending::<()>().await;
    }
    let mode = arguments.first().map(String::as_str).unwrap_or("normal");
    let index = arguments.iter().position(|argument| argument == "--config").ok_or("missing --config")?;
    let path = PathBuf::from(arguments.get(index + 1).ok_or("missing config path")?);
    let bytes = tokio::fs::read(&path).await?;
    let connection: Connection = serde_json::from_slice(&bytes)?;
    tokio::fs::remove_file(&path).await?;
    if mode == "crash" {
        tokio::fs::write(path.with_extension("error.log"), b"earlier line\nboom\n").await?;
        std::process::exit(1);
    }
    if mode == "record-pid" || mode == "no-hello" {
        tokio::fs::write(arguments.get(1).ok_or("missing pid path")?, std::process::id().to_string()).await?;
    }
    if mode == "no-hello" { std::future::pending::<()>().await; }
    let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build()?;
    let mut body = json!({"hello":true,"boot":"agent-a","session_id":1,"pid":std::process::id(),"protocol":2});
    let mut pause_after_reply = false;
    loop {
        let command: Value = client.post(&connection.url).bearer_auth(&connection.token)
            .json(&body).send().await?.error_for_status()?.json().await?;
        if pause_after_reply && mode == "pause-polls" {
            let directory = PathBuf::from(arguments.get(1).ok_or("missing pause directory")?);
            tokio::fs::write(directory.join("paused"), b"paused").await?;
            tokio::time::timeout(Duration::from_secs(10), async {
                while !tokio::fs::try_exists(directory.join("release")).await? {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok::<_, std::io::Error>(())
            }).await??;
            pause_after_reply = false;
        }
        body = json!({"boot":"agent-a","session_id":1,"pid":std::process::id()});
        if command["operation"] == "observe" { pause_after_reply = true; }
        match command["operation"].as_str().ok_or("missing operation")? {
            "stop" => return Ok(()),
            "heartbeat" => continue,
            "screenshot" if mode == "exit-on-screenshot" => return Ok(()),
            operation => {
                body["id"] = command["id"].clone();
                if mode == "agent-error" {
                    body["error"] = json!("ValueError: observação inválida");
                    continue;
                }
                body["result"] = match operation {
                    "observe" => json!({"observation_id":"observation-a","connected":true,"session_id":1,
                        "foreground":"desktop","windows":[],"elements":[{"id":"e1","name":"ação","role":"Edit",
                        "value":null,"enabled":true,"rect":null,"focused":true,"actions":["set_value"]}],
                        "truncated":false,"timestamp":0.0,"screen":{"width":1280,"height":720}}),
                    "act" => {
                        if mode == "record-act" {
                            tokio::fs::write(arguments.get(1).ok_or("missing act marker")?, b"act").await?;
                        }
                        if mode == "pause-polls" {
                            let directory = PathBuf::from(arguments.get(1).ok_or("missing pause directory")?);
                            tokio::fs::write(directory.join("act.marker"), b"act").await?;
                        }
                        json!({"ok":true})
                    }
                    "screenshot" => json!({"png":"cG5n"}),
                    _ => return Err("unknown operation".into()),
                };
            }
        }
    }
}
