//! Desktop (agent side) and Session/Connector (controller side) contracts.

use crate::acao::Action;
use crate::comando::{ActResult, Delivery, Scope};
use crate::observacao::Observation;
use std::fmt;
use std::future::Future;
use tokio_util::sync::CancellationToken;

/// Refusal texts the agents write and the loop reads to decide a retry: kept in one place, so rewording one
/// cannot silently break the retry.
pub mod recusa {
    pub const USUARIO_ATIVO: &str = "usuário ativo";
    pub const ENTRADA_REAL: &str = "precisa de entrada real";
    pub const JANELA_ATRAS: &str = "a janela está atrás; use entrada polite";
    pub const JANELA_FECHOU: &str = "a janela fechou";
    pub const VALOR_NAO_CONFIRMADO: &str = "valor digitado não confirmado";
    pub const REGIAO_SUMIU: &str = "região sumiu";
}

/// Python exception class an error travels as (`"<Kind>: <msg>"` on the wire).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    ValueError,
    RuntimeError,
    TimeoutError,
    ConnectionError,
    ComError,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ValueError => "ValueError",
            Self::RuntimeError => "RuntimeError",
            Self::TimeoutError => "TimeoutError",
            Self::ConnectionError => "ConnectionError",
            Self::ComError => "COMError",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopError {
    pub kind: ErrorKind,
    pub msg: String,
    /// Only meaningful for a failed `act`.
    pub delivery: Delivery,
}

impl DesktopError {
    pub fn new(kind: ErrorKind, msg: impl Into<String>) -> Self {
        Self { kind, msg: msg.into(), delivery: Delivery::Unknown }
    }

    /// The action was refused before touching the app: repeating it is safe.
    pub fn nao_entregue(self) -> Self {
        Self { delivery: Delivery::NotDelivered, ..self }
    }
}

impl fmt::Display for DesktopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.msg)
    }
}

impl std::error::Error for DesktopError {}

/// Agent side, implemented by the Linux, Windows and fake desktops; blocking.
pub trait Desktop {
    fn session_id(&self) -> u32;
    fn available(&mut self) -> Result<(), DesktopError>;
    fn foreground(&mut self) -> Result<String, DesktopError>;
    /// Whether the window `id` (a `Window.id`) is still open, in front or not.
    fn has_window(&mut self, id: &str) -> Result<bool, DesktopError>;
    /// `observation_id` is filled by the runtime. With `scope.window`, `foreground` is that window.
    fn observe(&mut self, scope: &Scope) -> Result<Observation, DesktopError>;
    fn act(&mut self, observed: &Observation, action: &Action) -> Result<Delivery, DesktopError>;
    /// Already at most 1280 px wide; `window` captures only that window.
    fn screenshot_png(&mut self, window: Option<&str>) -> Result<Vec<u8>, DesktopError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    Agent(String),
    Timeout(String),
    Disconnected(String),
    Cancelled(String),
    Startup(String),
    StartupTimeout(String),
    Config(String),
    /// The agent refused the action before touching the app: repeating it is safe.
    NotDelivered(String),
}

impl SessionError {
    /// Python exception class this error was raised as (laco_uia.py:563 writes it in `motivo`).
    pub fn tipo(&self) -> &'static str {
        match self {
            Self::NotDelivered(_) => "NotDelivered",
            Self::Agent(_) | Self::Cancelled(_) | Self::Startup(_) => "RuntimeError",
            Self::Timeout(_) | Self::StartupTimeout(_) => "TimeoutError",
            Self::Disconnected(_) => "ConnectionError",
            Self::Config(_) => "ValueError",
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(m)
            | Self::Timeout(m)
            | Self::Disconnected(m)
            | Self::Cancelled(m)
            | Self::Startup(m)
            | Self::StartupTimeout(m)
            | Self::Config(m)
            | Self::NotDelivered(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for SessionError {}

/// Controller side, used by the loop.
pub trait Session: Send {
    fn observe(&mut self, scope: &Scope) -> impl Future<Output = Result<Observation, SessionError>> + Send;
    fn act(&mut self, observation_id: &str, action: &Action) -> impl Future<Output = Result<ActResult, SessionError>> + Send;
    fn screenshot(&mut self, window: Option<&str>) -> impl Future<Output = Result<Vec<u8>, SessionError>> + Send;
    fn close(&mut self) -> impl Future<Output = Result<(), String>> + Send;
}

pub trait Connector: Send + Sync {
    type S: Session;
    fn connect(&self, cancel: &CancellationToken) -> impl Future<Output = Result<Self::S, SessionError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_error_travels_with_python_class_name() {
        let e = DesktopError::new(ErrorKind::ComError, "falhou");
        assert_eq!(e.to_string(), "COMError: falhou");
        let e = DesktopError::new(ErrorKind::TimeoutError, "comando expirou antes de chegar ao desktop").nao_entregue();
        assert_eq!(e.delivery, Delivery::NotDelivered);
        assert_eq!(e.to_string(), "TimeoutError: comando expirou antes de chegar ao desktop");
    }

    #[test]
    fn session_error_display_is_inner_message_and_tipo_is_python_class() {
        let casos = [
            (SessionError::Agent("a".into()), "RuntimeError"),
            (SessionError::Cancelled("a".into()), "RuntimeError"),
            (SessionError::Startup("a".into()), "RuntimeError"),
            (SessionError::Timeout("a".into()), "TimeoutError"),
            (SessionError::StartupTimeout("a".into()), "TimeoutError"),
            (SessionError::Disconnected("a".into()), "ConnectionError"),
            (SessionError::Config("a".into()), "ValueError"),
            (SessionError::NotDelivered("a".into()), "NotDelivered"),
        ];
        for (e, tipo) in casos {
            assert_eq!(e.to_string(), "a");
            assert_eq!(e.tipo(), tipo);
        }
    }
}
