//! Desktop (agent side) and Session/Connector (controller side) contracts.

use crate::acao::Action;
use crate::comando::ActResult;
use crate::observacao::Observation;
use std::fmt;
use std::future::Future;
use tokio_util::sync::CancellationToken;

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
    /// `observation_id` is filled by the runtime.
    fn observe(&mut self) -> Result<Observation, DesktopError>;
    fn act(&mut self, observed: &Observation, action: &Action) -> Result<(), DesktopError>;
    /// Already at most 1280 px wide.
    fn screenshot_png(&mut self) -> Result<Vec<u8>, DesktopError>;
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
}

impl SessionError {
    /// Python exception class this error was raised as (laco_uia.py:563 writes it in `motivo`).
    pub fn tipo(&self) -> &'static str {
        match self {
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
            | Self::Config(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for SessionError {}

/// Controller side, used by the loop.
pub trait Session: Send {
    fn observe(&mut self) -> impl Future<Output = Result<Observation, SessionError>> + Send;
    fn act(&mut self, observation_id: &str, action: &Action) -> impl Future<Output = Result<ActResult, SessionError>> + Send;
    fn screenshot(&mut self) -> impl Future<Output = Result<Vec<u8>, SessionError>> + Send;
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
        let e = DesktopError { kind: ErrorKind::ComError, msg: "falhou".into() };
        assert_eq!(e.to_string(), "COMError: falhou");
        let e = DesktopError { kind: ErrorKind::TimeoutError, msg: "comando expirou antes de chegar ao desktop".into() };
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
        ];
        for (e, tipo) in casos {
            assert_eq!(e.to_string(), "a");
            assert_eq!(e.tipo(), tipo);
        }
    }
}
