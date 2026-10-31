//! The real engine: the objective loop over the controller, which starts this same binary as agent.

use std::path::PathBuf;
use std::time::Instant;

use hcc_controlador::AgentConnector;
use hcc_laco::executar::{Opcoes, executar};
use hcc_laco::jev::{JEV_MODEL, JEV_URL, Jev};
use hcc_laco::llm::Llm;
use hcc_laco::tipos::Progresso;
use hcc_protocolo::{Connector, Session};
use tokio_util::sync::CancellationToken;

use crate::motor::{Futuro, Motor, PedidoObjetivo, imagem, rotulo};

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
    let var = |nome| std::env::var(nome).unwrap_or_default();
    resolver_jev(var("TYPESAFE_API_KEY"), var("JEV_ENDPOINT"), var("JEV_MODEL"))
}

/// Shared Jev configuration: empty endpoint or model falls back to the TypeSafe default.
fn resolver_jev(chave: String, endpoint: String, modelo: String) -> Option<Jev> {
    let ou = |v: String, padrao: &str| if v.is_empty() { padrao.to_owned() } else { v };
    (!chave.is_empty()).then(|| Jev::new(ou(endpoint, JEV_URL), chave, ou(modelo, JEV_MODEL)))
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
        r = sessao.screenshot(None) => r.map_err(|e| e.to_string()),
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
    let escopo = hcc_protocolo::Scope::default();
    let observacao = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(hcc_protocolo::SessionError::Cancelled("controle cancelado ou encerrado".to_owned())),
        r = sessao.observe(&escopo) => r,
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

/// The `act` answer: the pick, its runner-up, the risk, the delivery and whether the screen changed.
fn um_passo_json(r: &hcc_laco::resultado::Resultado) -> String {
    let u = r.ultima.clone().unwrap_or_default();
    // A timeout may have acted: "incerto" tells the caller not to simply try again.
    let decisao = match u.entrega.as_deref() {
        Some(e) if e.starts_with("delivered") => "agiu",
        Some("unknown") => "incerto",
        _ => "parou",
    };
    // Why it stopped: the action's own result when it was tried, the run's reason otherwise.
    let motivo = (decisao != "agiu").then(|| u.resultado.clone().filter(|_| u.entrega.is_some()).unwrap_or_else(|| r.motivo.clone()));
    serde_json::json!({
        "decisao": decisao,
        "motivo": motivo,
        "acao": r.ultima.as_ref().map(|u| &u.acao),
        "confianca": r.ultima.as_ref().map(|u| u.confianca),
        "segundo": u.segundo.as_ref().map(|(acao, p)| serde_json::json!({"acao": acao, "confianca": p})),
        "risco": r.ultima.as_ref().map(|u| u.risco),
        "entrega": u.entrega,
        "tela_mudou": u.tela_mudou,
    }).to_string()
}

impl Motor for MotorReal {
    fn objetivo(
        &self,
        p: PedidoObjetivo,
        cancel: CancellationToken,
        progresso: Box<dyn Fn(String, Option<PathBuf>) + Send + Sync>,
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
                // Never the caller's repository: a script recorded there dirties a project that is not ours.
                roteiros: std::env::var_os("HCC_ROTEIROS").filter(|v| !v.is_empty()).map(PathBuf::from)
                    .or_else(|| std::env::home_dir().map(|h| h.join(".cache/hangar-computer-control/roteiros"))),
                refazer: p.refazer,
                ver: p.ver,
                so_decidir: p.passo_unico == Some(false),
            };
            let etapa = move |e: Progresso| progresso(rotulo(&e), imagem(&e));
            let r = executar(op, &conector, cancel, &etapa).await;
            if p.passo_unico.is_some() { um_passo_json(&r) } else { r.resumo() }
        })
    }

    fn ver_tela(&self, config: Option<PathBuf>, cancel: CancellationToken) -> Futuro<Result<String, String>> {
        Box::pin(capturar(config, cancel))
    }

    fn estado(&self, config: Option<PathBuf>, cancel: CancellationToken) -> Futuro<String> {
        Box::pin(consultar(config, cancel))
    }
}

#[cfg(test)]
mod tests {
    use super::resolver_jev;

    #[test]
    fn default_endpoint_and_model_when_env_empty() {
        let jev = resolver_jev("k".into(), String::new(), String::new()).unwrap();
        assert_eq!((jev.url.as_str(), jev.model.as_str()), ("https://api.typesafe.ai/v1/systemone", "jev-latest"));
        let jev = resolver_jev("k".into(), "http://x/decisions".into(), "~typesafe/jev-latest".into()).unwrap();
        assert_eq!((jev.url.as_str(), jev.model.as_str()), ("http://x/decisions", "~typesafe/jev-latest"));
        assert!(resolver_jev(String::new(), String::new(), String::new()).is_none());
    }
}
