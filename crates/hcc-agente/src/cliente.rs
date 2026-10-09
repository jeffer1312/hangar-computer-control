//! HTTP long-poll client to the controller.

use hcc_protocolo::{AgentPost, Connection};
use std::time::Duration;

pub struct Cliente {
    // Used only around the POST: Desktop methods run outside any block_on, so a desktop may
    // own its runtime (the Linux AT-SPI tree does).
    rt: tokio::runtime::Runtime,
    http: reqwest::Client,
    url: String,
    autorizacao: String,
}

/// `Display` of reqwest hides the cause ("error sending request"); the chain says why.
fn com_causas(e: &dyn std::error::Error) -> String {
    let mut texto = e.to_string();
    let mut causa = e.source();
    while let Some(c) = causa {
        texto.push_str(": ");
        texto.push_str(&c.to_string());
        causa = c.source();
    }
    texto
}

impl Cliente {
    pub fn new(conexao: &Connection) -> Result<Self, String> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| com_causas(&e))?;
        // Proxy env vars must not divert a loopback/tunnel connection (windows_uia.py:387).
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| com_causas(&e))?;
        Ok(Self { rt, http, url: conexao.url.clone(), autorizacao: format!("Bearer {}", conexao.token) })
    }

    /// Posts the body; the reply body is the next command.
    pub fn enviar(&self, corpo: &AgentPost) -> Result<serde_json::Value, String> {
        self.rt.block_on(async {
            let resposta = self
                .http
                .post(&self.url)
                .header(reqwest::header::AUTHORIZATION, &self.autorizacao)
                .json(corpo)
                .send()
                .await
                .map_err(|e| com_causas(&e))?;
            let status = resposta.status();
            if !status.is_success() {
                // The controller's reason (e.g. another process replacing the agent) is in the body.
                let motivo = resposta.text().await.unwrap_or_default();
                return Err(format!("controlador respondeu {status}: {}", motivo.trim()));
            }
            resposta.json::<serde_json::Value>().await.map_err(|e| com_causas(&e))
        })
    }
}
