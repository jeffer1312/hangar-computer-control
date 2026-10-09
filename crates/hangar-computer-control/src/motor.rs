//! The engine behind the MCP tools; Task 10 plugs in the real loop.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use hcc_laco::tipos::Progresso;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub type Futuro<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

pub struct PedidoObjetivo {
    pub texto: String,
    pub max_passos: u32,
    pub dados: serde_json::Map<String, Value>,
    pub limite: Duration,
    pub config: Option<PathBuf>,
}

pub trait Motor: Send + Sync + 'static {
    /// Returns the `resumo` text; must return soon after `cancel` fires.
    fn objetivo(
        &self,
        p: PedidoObjetivo,
        cancel: CancellationToken,
        progresso: Box<dyn Fn(String) + Send + Sync>,
    ) -> Futuro<String>;
    /// `"<png path>; <secs>s"`.
    fn ver_tela(&self, config: Option<PathBuf>) -> Futuro<Result<String, String>>;
    fn estado(&self, config: Option<PathBuf>) -> Futuro<String>;
}

pub fn rotulo(p: &Progresso) -> String {
    match p {
        Progresso::Conexao => "conectando ao Windows",
        Progresso::Reconexao => "reconectando ao Windows",
        Progresso::Observacao => "lendo a tela",
        Progresso::Jev => "decidindo o próximo passo",
        Progresso::Captura => "capturando a tela",
        Progresso::Llm => "pedindo ajuda visual",
        Progresso::Acao(d) => d,
    }
    .to_owned()
}

/// Echoes its inputs so the MCP tests see what reached the engine.
#[cfg(feature = "motor-fake")]
pub struct FakeMotor;

#[cfg(feature = "motor-fake")]
impl Motor for FakeMotor {
    fn objetivo(
        &self,
        p: PedidoObjetivo,
        cancel: CancellationToken,
        progresso: Box<dyn Fn(String) + Send + Sync>,
    ) -> Futuro<String> {
        Box::pin(async move {
            progresso(rotulo(&Progresso::Observacao));
            if p.texto == "ignorar cancelamento" {
                tokio::time::sleep(p.limite).await;
                return "fim".to_owned();
            }
            if p.texto == "esperar cancelamento" {
                return tokio::select! {
                    () = cancel.cancelled() => {
                        eprintln!("fake: cancelado");
                        "parou: cancelado".to_owned()
                    }
                    () = tokio::time::sleep(p.limite) => "parou: tempo esgotado".to_owned(),
                };
            }
            format!(
                "concluído: fake\ntexto={}; max_passos={}; dados={}; limite={}s; config={}",
                p.texto,
                p.max_passos,
                Value::Object(p.dados),
                p.limite.as_secs_f64(),
                p.config.map(|c| c.display().to_string()).unwrap_or_default()
            )
        })
    }

    fn ver_tela(&self, config: Option<PathBuf>) -> Futuro<Result<String, String>> {
        Box::pin(async move {
            config.map(|c| format!("{}; 0.00s", c.display())).ok_or_else(|| "sem config".to_owned())
        })
    }

    fn estado(&self, config: Option<PathBuf>) -> Futuro<String> {
        Box::pin(async move {
            let Some(c) = config else { return "indisponível: sem config".to_owned() };
            let lido = std::fs::read_to_string(&c).map_err(|e| e.to_string()).and_then(|t| {
                serde_json::from_str::<Value>(&t).map_err(|e| e.to_string())
            });
            match lido {
                Ok(_) => "disponível via acessibilidade, 1280x800".to_owned(),
                Err(e) => format!("indisponível: {}: {e}", c.display()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotulos_iguais_ao_python() {
        let casos = [
            (Progresso::Conexao, "conectando ao Windows"),
            (Progresso::Reconexao, "reconectando ao Windows"),
            (Progresso::Observacao, "lendo a tela"),
            (Progresso::Jev, "decidindo o próximo passo"),
            (Progresso::Captura, "capturando a tela"),
            (Progresso::Llm, "pedindo ajuda visual"),
            (Progresso::Acao("digitar *** em Nome".into()), "digitar *** em Nome"),
        ];
        for (p, esperado) in casos {
            assert_eq!(rotulo(&p), esperado);
        }
    }
}
