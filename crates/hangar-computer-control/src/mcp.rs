//! MCP server over stdio: three tools, schemas byte-equal to the Python server's.

use std::borrow::Cow;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, JsonObject, ListToolsResult, PaginatedRequestParams,
    ProgressNotificationParam, ProtocolVersion, ServerCapabilities, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::alvos;
use crate::motor::{Motor, PedidoObjetivo};
use crate::progresso;

const DESC_OBJETIVO: &str = "Realiza um objetivo no desktop Windows, do começo ao fim.\n\n    Descreva o que quer em português, como pediria a uma pessoa (\"abrir o\n    Bloco de Notas e digitar olá\"). O laço interno lê a árvore de acessibilidade,\n    o Jev decide cada passo e o agente executa até concluir ou desistir.\n\n    `dados` leva valores conhecidos (nome, texto, caminho) que o laço pode digitar.\n    Só conclui com evidência ou devolve um impedimento específico. Ação arriscada\n    (apagar, sobrescrever, enviar) não autorizada no texto para e volta pra você.\n    limite_segundos é o orçamento total; max_passos limita os ciclos internos.";
const DESC_VER_TELA: &str = "Captura a tela do desktop e devolve o caminho do arquivo PNG.\n\n    Para quando o `objetivo` parou e você precisa olhar o que há na tela.\n    Cite o caminho devolvido na sua resposta para que o usuário veja a imagem.";
const DESC_ESTADO: &str = "Diz se o desktop está acessível e em qual resolução.";

const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
const PROGRESS_SEND: Duration = Duration::from_secs(2);

fn objeto(v: Value) -> Arc<JsonObject> {
    match v {
        Value::Object(m) => Arc::new(m),
        _ => unreachable!("schema literals are objects"),
    }
}

fn alvo_schema() -> Value {
    json!({"anyOf": [{"type": "string"}, {"type": "null"}], "default": null, "title": "Alvo"})
}

fn ferramenta(nome: &'static str, descricao: String, entrada: Value) -> Tool {
    let saida = json!({"properties": {"result": {"title": "Result", "type": "string"}},
        "required": ["result"], "type": "object", "title": format!("{nome}Output")});
    Tool::new(nome, descricao, objeto(entrada)).with_raw_output_schema(objeto(saida))
}

/// Tool list computed once at start, like the Python `ALVOS` suffix.
fn ferramentas() -> Vec<Tool> {
    let nomes: Vec<String> = alvos::alvos().into_iter().map(|(n, _)| n).collect();
    let lista = if nomes.is_empty() { "nenhum".to_owned() } else { nomes.join(", ") };
    let sufixo = format!(
        "\n\n`alvo` escolhe o desktop controlado. Disponíveis: {lista}; sem `alvo`, usa o padrão ({}).",
        alvos::padrao()
    );
    let so_alvo = |nome: &str| {
        json!({"properties": {"alvo": alvo_schema()}, "type": "object",
            "title": format!("{nome}Arguments")})
    };
    vec![
        ferramenta(
            "objetivo",
            format!("{DESC_OBJETIVO}{sufixo}"),
            json!({
                "properties": {
                    "texto": {"title": "Texto", "type": "string"},
                    "max_passos": {"default": 12, "title": "Max Passos", "type": "integer"},
                    "dados": {"anyOf": [{"additionalProperties": true, "type": "object"},
                        {"type": "null"}], "default": null, "title": "Dados"},
                    "limite_segundos": {"default": 240, "title": "Limite Segundos", "type": "number"},
                    "alvo": alvo_schema()
                },
                "required": ["texto"], "type": "object", "title": "objetivoArguments"
            }),
        ),
        ferramenta("ver_tela", format!("{DESC_VER_TELA}{sufixo}"), so_alvo("ver_tela")),
        ferramenta("estado", format!("{DESC_ESTADO}{sufixo}"), so_alvo("estado")),
    ]
}

fn max_passos_padrao() -> i64 {
    12
}

fn limite_padrao() -> f64 {
    240.0
}

#[derive(Deserialize)]
struct ArgsObjetivo {
    texto: String,
    #[serde(default = "max_passos_padrao")]
    max_passos: i64,
    #[serde(default)]
    dados: Option<Map<String, Value>>,
    #[serde(default = "limite_padrao")]
    limite_segundos: f64,
    #[serde(default)]
    alvo: Option<String>,
}

#[derive(Deserialize)]
struct ArgsAlvo {
    #[serde(default)]
    alvo: Option<String>,
}

fn sucesso(texto: String) -> CallToolResult {
    let mut r = CallToolResult::success(vec![ContentBlock::text(texto.clone())]);
    r.structured_content = Some(json!({"result": texto}));
    r
}

fn erro(texto: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(texto)])
}

struct Servidor {
    motor: Arc<dyn Motor>,
    ferramentas: Vec<Tool>,
    /// Cancels every running tool (stdin EOF or signal).
    desligar: CancellationToken,
    /// Each running tool holds a read guard; shutdown takes the write side to wait for them.
    vivos: Arc<RwLock<()>>,
}

impl Servidor {
    fn cancelamento(&self, do_cliente: CancellationToken) -> CancellationToken {
        let cancel = self.desligar.child_token();
        let ponte = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = do_cliente.cancelled() => ponte.cancel(),
                () = ponte.cancelled() => {}
            }
        });
        cancel
    }

    async fn consultar(&self, nome: &str, a: ArgsAlvo, ctx: RequestContext<RoleServer>) -> CallToolResult {
        let config = match alvos::config_do(a.alvo.as_deref()) {
            Ok(c) => c,
            Err(e) => return erro(e),
        };
        let Ok(guarda) = self.vivos.clone().try_read_owned() else {
            return erro("servidor encerrando".to_owned());
        };
        let cancel = self.cancelamento(ctx.ct.clone());
        let _cancela_ao_sair = cancel.clone().drop_guard();
        let motor = self.motor.clone();
        let tela = nome == "ver_tela";
        let tarefa = tokio::spawn(async move {
            let r = if tela {
                match motor.ver_tela(config, cancel).await {
                    Ok(t) => sucesso(t),
                    Err(e) => erro(e),
                }
            } else {
                sucesso(motor.estado(config, cancel).await)
            };
            drop(guarda);
            r
        });
        match tarefa.await {
            Ok(r) => r,
            Err(e) => erro(format!("falha interna: {e}")),
        }
    }

    async fn objetivo(&self, a: ArgsObjetivo, ctx: RequestContext<RoleServer>) -> CallToolResult {
        let config = match alvos::config_do(a.alvo.as_deref()) {
            Ok(c) => c,
            Err(e) => return erro(e),
        };
        let Ok(guarda) = self.vivos.clone().try_read_owned() else {
            return erro("servidor encerrando".to_owned());
        };
        let pedido = PedidoObjetivo {
            texto: a.texto,
            // Out-of-range values still reach the loop, which answers `parou:` for them.
            max_passos: u32::try_from(a.max_passos.max(0)).unwrap_or(u32::MAX),
            dados: a.dados.unwrap_or_default(),
            limite: Duration::try_from_secs_f64(a.limite_segundos).unwrap_or(
                if a.limite_segundos > 0.0 { Duration::MAX } else { Duration::ZERO },
            ),
            config,
        };

        let cancel = self.cancelamento(ctx.ct.clone());
        // If rmcp drops this handler, the motor still sees the cancel.
        let _cancela_ao_sair = cancel.clone().drop_guard();

        let arquivo = progresso::arquivo(ctx.meta.get("claudecode/toolUseId").and_then(Value::as_str));
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let encaminhador = ctx.meta.get_progress_token().map(|token| {
            let peer = ctx.peer.clone();
            tokio::spawn(async move {
                let mut n = 0.0;
                while let Some(m) = rx.recv().await {
                    n += 1.0;
                    let p = ProgressNotificationParam::new(token.clone(), n).with_message(m);
                    let _ = tokio::time::timeout(PROGRESS_SEND, peer.notify_progress(p)).await;
                }
            })
        });
        let ao_progredir = Box::new(move |m: String| {
            if let Some(a) = &arquivo {
                progresso::anotar(a, &m);
            }
            let _ = tx.send(m);
        });

        let futuro = self.motor.objetivo(pedido, cancel.clone(), ao_progredir);
        // Spawned so shutdown can wait for it even after rmcp let go of the request.
        let tarefa = tokio::spawn(async move {
            let r = futuro.await;
            drop(guarda);
            r
        });
        let resultado = tarefa.await;
        // The motor future dropped the callback, so the channel closes once the last step is sent:
        // every progress notification goes out before the response, as in the Python.
        if let Some(mut e) = encaminhador
            && tokio::time::timeout(PROGRESS_SEND, &mut e).await.is_err()
        {
            // A stuck send must not leak a progress notification after the response.
            e.abort();
        }
        match resultado {
            Ok(resumo) => sucesso(resumo),
            Err(e) => erro(format!("falha interna: {e}")),
        }
    }
}

impl ServerHandler for Servidor {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("hangar-computer-control", env!("CARGO_PKG_VERSION")))
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_06_18))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult { tools: self.ferramentas.clone(), ..Default::default() })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = Value::Object(request.arguments.unwrap_or_default());
        let invalido = |e: serde_json::Error| erro(format!("argumentos inválidos para {}: {e}", request.name));
        let r = match request.name.as_ref() {
            "objetivo" => match serde_json::from_value::<ArgsObjetivo>(args) {
                Ok(a) => self.objetivo(a, context).await,
                Err(e) => invalido(e),
            },
            nome @ ("ver_tela" | "estado") => match serde_json::from_value::<ArgsAlvo>(args) {
                Err(e) => invalido(e),
                Ok(a) => self.consultar(nome, a, context).await,
            },
            outro => erro(format!("ferramenta desconhecida: {outro}")),
        };
        Ok(r.into())
    }
}

/// stdin that reports its end: rmcp waits up to 5 s for in-flight handlers after EOF, and the
/// running objectives only finish once they are cancelled.
struct Entrada {
    stdin: tokio::io::Stdin,
    fim: CancellationToken,
}

impl AsyncRead for Entrada {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let antes = buf.filled().len();
        let pedido = buf.remaining() > 0;
        let r = Pin::new(&mut self.stdin).poll_read(cx, buf);
        if let Poll::Ready(res) = &r
            && (res.is_err() || (pedido && buf.filled().len() == antes))
        {
            self.fim.cancel();
        }
        r
    }
}

#[cfg(unix)]
async fn sinal() {
    use tokio::signal::unix::{SignalKind, signal};
    match (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) {
        (Ok(mut term), Ok(mut int)) => {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
        }
        _ => std::future::pending().await,
    }
}

#[cfg(not(unix))]
async fn sinal() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Serves stdio until EOF or SIGTERM/SIGINT, then cancels running objectives and waits ≤5 s.
/// The caller exits the process right after: the blocking stdin read never returns on its own.
pub async fn servir<M: Motor>(motor: M) -> Result<(), String> {
    let desligar = CancellationToken::new();
    let vivos = Arc::new(RwLock::new(()));
    let servidor = Servidor {
        motor: Arc::new(motor),
        ferramentas: ferramentas(),
        desligar: desligar.clone(),
        vivos: vivos.clone(),
    };
    let entrada = Entrada { stdin: tokio::io::stdin(), fim: desligar.clone() };
    // One future for the whole run, so no signal falls between handshake and serving.
    let sinal = sinal();
    tokio::pin!(sinal);
    // Every shutdown wait shares one deadline: exit within SHUTDOWN_WAIT of EOF or signal.
    let mut prazo = None;
    tokio::select! {
        r = servidor.serve((entrada, tokio::io::stdout())) => match r {
            Ok(servico) => {
                let espera = servico.waiting();
                tokio::pin!(espera);
                tokio::select! {
                    _ = &mut espera => {}
                    () = &mut sinal => {}
                    // stdin ended: objectives are cancelled now; rmcp still flushes the
                    // responses already in flight before `waiting` returns.
                    () = desligar.cancelled() => {
                        let fim = tokio::time::Instant::now() + SHUTDOWN_WAIT;
                        prazo = Some(fim);
                        tokio::select! {
                            _ = tokio::time::timeout_at(fim, &mut espera) => {}
                            () = &mut sinal => {}
                        }
                    }
                }
            }
            Err(_) if desligar.is_cancelled() => {}
            Err(e) => return Err(format!("MCP não iniciou: {e}")),
        },
        () = &mut sinal => {}
    }
    desligar.cancel();
    let fim = prazo.unwrap_or_else(|| tokio::time::Instant::now() + SHUTDOWN_WAIT);
    let _ = tokio::time::timeout_at(fim, vivos.write()).await;
    Ok(())
}
