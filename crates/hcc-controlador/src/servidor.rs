use axum::body::to_bytes;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::post};
use hcc_protocolo::{AgentPost, Command, Connection, PROTOCOL, SessionError};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Default)]
struct AgentState {
    boot: Option<String>,
    pid: Option<u32>,
    last_seen: Option<Instant>,
    startup_error: Option<SessionError>,
    pending: HashMap<String, oneshot::Sender<AgentPost>>,
}

struct Shared {
    token: String,
    cancel: CancellationToken,
    closed: CancellationToken,
    state: Mutex<AgentState>,
    commands: AsyncMutex<mpsc::Receiver<Command>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, AgentState> {
        self.state.lock().unwrap_or_else(|error| {
            eprintln!("estado do agente interrompido: {error}");
            error.into_inner()
        })
    }

    fn stopped(&self) -> bool {
        self.closed.is_cancelled() || self.cancel.is_cancelled()
    }

    fn live(&self, command: &Command) -> bool {
        match command {
            Command::Observe { id, .. } | Command::Act { id, .. } | Command::Screenshot { id, .. } =>
                self.lock().pending.get(id).is_some_and(|reply| !reply.is_closed()),
            Command::Heartbeat | Command::Stop => true,
        }
    }
}

pub(crate) struct PendingRpc {
    shared: Arc<Shared>,
    id: String,
}

impl Drop for PendingRpc {
    fn drop(&mut self) { self.shared.lock().pending.remove(&self.id); }
}

pub struct Server {
    connection: Connection,
    shared: Arc<Shared>,
    sender: mpsc::Sender<Command>,
    task: Option<JoinHandle<()>>,
}

impl Server {
    pub async fn start(cancel: CancellationToken) -> Result<Self, SessionError> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await
            .map_err(|error| SessionError::Startup(error.to_string()))?;
        let address = listener.local_addr().map_err(|error| SessionError::Startup(error.to_string()))?;
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let connection = Connection { url: format!("http://{address}/next"), token: token.clone() };
        let (sender, commands) = mpsc::channel(64);
        let shared = Arc::new(Shared {
            token,
            cancel,
            closed: CancellationToken::new(),
            state: Mutex::new(AgentState::default()),
            commands: AsyncMutex::new(commands),
        });
        let router = Router::new().route("/next", post(next))
            .fallback(|| async { StatusCode::FORBIDDEN })
            .layer(DefaultBodyLimit::max(16 * 1024 * 1024)).with_state(shared.clone());
        let shutdown = shared.closed.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned()).await {
                eprintln!("servidor do agente encerrou: {error}");
            }
        });
        Ok(Self { connection, shared, sender, task: Some(task) })
    }

    pub fn connection(&self) -> &Connection { &self.connection }

    pub fn startup_status(&self) -> Result<bool, SessionError> {
        let state = self.shared.lock();
        if let Some(error) = &state.startup_error { return Err(error.clone()); }
        Ok(state.boot.is_some())
    }

    pub(crate) fn pid(&self) -> Option<u32> { self.shared.lock().pid }

    pub(crate) fn check(&self) -> Result<(), SessionError> {
        if self.shared.stopped() {
            return Err(SessionError::Cancelled("controle cancelado ou encerrado".into()));
        }
        if self.shared.lock().last_seen.is_none_or(|seen| seen.elapsed() > Duration::from_secs(10)) {
            return Err(SessionError::Disconnected("agente desconectado; nenhum estado antigo será reutilizado".into()));
        }
        Ok(())
    }

    pub(crate) fn register(&self, id: String) -> (PendingRpc, oneshot::Receiver<AgentPost>) {
        let (sender, receiver) = oneshot::channel();
        self.shared.lock().pending.insert(id.clone(), sender);
        (PendingRpc { shared: self.shared.clone(), id }, receiver)
    }

    pub(crate) async fn send(&self, command: Command) -> Result<(), SessionError> {
        self.check()?;
        self.sender.send(command).await
            .map_err(|_| SessionError::Cancelled("controle cancelado ou encerrado".into()))
    }

    pub(crate) fn begin_close(&self) {
        self.shared.closed.cancel();
        self.shared.lock().pending.clear();
    }

    pub async fn close(&mut self) {
        self.begin_close();
        if let Some(task) = self.task.as_mut() {
            match tokio::time::timeout(Duration::from_secs(3), task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("servidor do agente encerrou: {error}"),
                Err(_) => { if let Some(task) = &self.task { task.abort(); } }
            }
        }
        self.task = None;
    }

    pub(crate) fn close_sync(&mut self) {
        self.begin_close();
        if let Some(task) = self.task.take() { task.abort(); }
    }
}

impl Drop for Server {
    fn drop(&mut self) { self.close_sync(); }
}

fn same_token(actual: &[u8], expected: &[u8]) -> bool {
    let mut difference = actual.len() ^ expected.len();
    for (index, expected) in expected.iter().enumerate() {
        difference |= usize::from(actual.get(index).copied().unwrap_or(0) ^ expected);
    }
    difference == 0
}

async fn next(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let expected = format!("Bearer {}", shared.token);
    let actual = request.headers().get("Authorization").map_or(&[][..], |header| header.as_bytes());
    if !same_token(actual, expected.as_bytes()) { return StatusCode::FORBIDDEN.into_response(); }
    let body = tokio::select! {
        biased;
        () = shared.closed.cancelled() => return Json(Command::Stop).into_response(),
        () = shared.cancel.cancelled() => return Json(Command::Stop).into_response(),
        body = tokio::time::timeout(Duration::from_secs(10), to_bytes(request.into_body(), 16 * 1024 * 1024)) => match body {
            Ok(Ok(body)) => body,
            Ok(Err(_)) | Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        },
    };
    let post: AgentPost = match serde_json::from_slice(&body) {
        Ok(post) => post,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    if post.session_id == 0 { return StatusCode::BAD_REQUEST.into_response(); }
    {
        let mut state = shared.lock();
        if state.boot.is_none() {
            if !post.hello { return StatusCode::BAD_REQUEST.into_response(); }
            if post.protocol != Some(PROTOCOL) {
                let protocol = post.protocol.map_or("None".into(), |number| number.to_string());
                state.startup_error = Some(SessionError::Startup(format!(
                    "agente incompatível (protocolo {protocol}); atualize windows-agent.exe")));
                return StatusCode::BAD_REQUEST.into_response();
            }
            state.boot = Some(post.boot.clone());
            state.pid = Some(post.pid);
        }
        if state.boot.as_ref() != Some(&post.boot) { return StatusCode::BAD_REQUEST.into_response(); }
        state.last_seen = Some(Instant::now());
        if let Some(sender) = post.id.as_ref().and_then(|id| state.pending.remove(id)) {
            // O RPC pode ter sido cancelado depois que o agente enviou a resposta.
            let _ = sender.send(post);
        }
    }
    if shared.stopped() { return Json(Command::Stop).into_response(); }
    let command = tokio::select! {
        biased;
        () = shared.closed.cancelled() => Command::Stop,
        () = shared.cancel.cancelled() => Command::Stop,
        command = tokio::time::timeout(Duration::from_secs(2), async {
            let mut commands = shared.commands.lock().await;
            loop {
                match commands.recv().await {
                    Some(command) if shared.live(&command) => return command,
                    Some(_) => {}
                    None => return Command::Stop,
                }
            }
        }) => command.unwrap_or(Command::Heartbeat),
    };
    Json(if shared.stopped() { Command::Stop } else { command }).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    #[tokio::test]
    async fn discarded_close_future_preserves_task_for_drop() {
        let mut server = Server::start(CancellationToken::new()).await.unwrap();
        let original = server.task.take().unwrap();
        original.abort();
        assert!(original.await.unwrap_err().is_cancelled());
        let (started_tx, started) = oneshot::channel();
        let (stopped_tx, stopped) = oneshot::channel();
        struct OnDrop(Option<oneshot::Sender<()>>);
        impl Drop for OnDrop {
            fn drop(&mut self) { let _ = self.0.take().unwrap().send(()); }
        }
        server.task = Some(tokio::spawn(async move {
            let _on_drop = OnDrop(Some(stopped_tx));
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        started.await.unwrap();
        let mut close = Box::pin(server.close());
        std::future::poll_fn(|context| {
            assert!(close.as_mut().poll(context).is_pending());
            std::task::Poll::Ready(())
        }).await;
        drop(close);
        drop(server);
        tokio::time::timeout(Duration::from_secs(1), stopped).await
            .expect("server task detached after cancelled close").unwrap();
    }
}
