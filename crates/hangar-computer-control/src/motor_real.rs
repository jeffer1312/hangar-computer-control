//! The real engine: the objective loop over the controller, which starts this same binary as agent.

use std::path::PathBuf;
use std::time::Instant;

use hcc_controlador::AgentConnector;
use hcc_laco::executar::{Opcoes, executar};
use hcc_laco::jev::{JEV_URL, Jev};
use hcc_laco::llm::Llm;
use hcc_laco::tipos::Progresso;
use hcc_protocolo::{Connector, Session};
use tokio_util::sync::CancellationToken;

use crate::motor::{Futuro, Motor, PedidoObjetivo, rotulo};

pub struct MotorReal;

fn conector(config: Option<PathBuf>) -> AgentConnector {
    // The agent is this executable: `<exe> agent --config <connection file>`.
    // An empty program makes the start fail later; the real cause goes to stderr now.
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|e| {
        eprintln!("executável atual indisponível: {e}");
        String::new()
    });
    AgentConnector { config_path: config, default_command: vec![exe, "agent".to_owned()] }
}

fn jev() -> Option<Jev> {
    let chave = std::env::var("TYPESAFE_API_KEY").ok().filter(|k| !k.is_empty())?;
    // HCC_JEV_URL: test/ops override of the TypeSafe endpoint, read only when set.
    let url = std::env::var("HCC_JEV_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| JEV_URL.to_owned());
    Some(Jev::new(url, chave))
}

/// `tempfile.mkdtemp(prefix="hcu-tela-")`: a new private directory in the temp dir.
fn pasta_nova() -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir();
    let semente = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut construtor = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut construtor, 0o700);
    for n in 0..100u128 {
        let pasta = base.join(format!("hcu-tela-{}-{:x}", std::process::id(), semente.wrapping_add(n)));
        match construtor.create(&pasta) {
            Ok(()) => return Ok(pasta),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "nenhum nome livre para hcu-tela-*"))
}

async fn capturar(config: Option<PathBuf>, cancel: CancellationToken) -> Result<String, String> {
    let inicio = Instant::now();
    let mut sessao = conector(config).connect(&cancel).await.map_err(|e| e.to_string())?;
    let imagem = tokio::select! {
        biased;
        () = cancel.cancelled() => Err("controle cancelado ou encerrado".to_owned()),
        r = sessao.screenshot() => r.map_err(|e| e.to_string()),
    };
    let fechamento = sessao.close().await;
    let png = imagem?;
    fechamento?;
    let caminho = pasta_nova().map_err(|e| e.to_string())?.join("tela.png");
    tokio::fs::write(&caminho, png).await.map_err(|e| format!("{}: {e}", caminho.display()))?;
    Ok(format!("{}; {:.2}s", caminho.display(), inicio.elapsed().as_secs_f64()))
}

async fn consultar(config: Option<PathBuf>, cancel: CancellationToken) -> String {
    let mut sessao = match conector(config).connect(&cancel).await {
        Ok(s) => s,
        Err(e) => return format!("indisponível: {e}"),
    };
    let observacao = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(hcc_protocolo::SessionError::Cancelled("controle cancelado ou encerrado".to_owned())),
        r = sessao.observe() => r,
    };
    let texto = match observacao {
        Ok(o) if !o.connected => "indisponível: agente desconectado".to_owned(),
        Ok(o) => format!("disponível via acessibilidade, {}x{}", o.screen.width, o.screen.height),
        Err(e) => format!("indisponível: {e}"),
    };
    if let Err(e) = sessao.close().await {
        eprintln!("estado: falha ao fechar sessão: {e}");
    }
    texto
}

impl Motor for MotorReal {
    fn objetivo(
        &self,
        p: PedidoObjetivo,
        cancel: CancellationToken,
        progresso: Box<dyn Fn(String) + Send + Sync>,
    ) -> Futuro<String> {
        Box::pin(async move {
            let conector = conector(p.config.clone());
            let op = Opcoes {
                texto: p.texto,
                max_passos: p.max_passos,
                dados: p.dados,
                limite: p.limite,
                config: p.config,
                jev: jev(),
                llm: Llm::from_env(),
            };
            let etapa = move |e: Progresso| progresso(rotulo(&e));
            executar(op, &conector, cancel, &etapa).await.resumo()
        })
    }

    fn ver_tela(&self, config: Option<PathBuf>, cancel: CancellationToken) -> Futuro<Result<String, String>> {
        Box::pin(capturar(config, cancel))
    }

    fn estado(&self, config: Option<PathBuf>, cancel: CancellationToken) -> Futuro<String> {
        Box::pin(consultar(config, cancel))
    }
}
