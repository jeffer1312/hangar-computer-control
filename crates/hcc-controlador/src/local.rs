use hcc_protocolo::{Connection, SessionError};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::process::{Child, Command};

pub(crate) struct LocalTransport {
    directory: Option<TempDir>,
    child: Child,
    path: PathBuf,
}

impl LocalTransport {
    pub(crate) async fn start(command: &[String], connection: &Connection) -> Result<Self, SessionError> {
        let (program, arguments) = command.split_first()
            .ok_or_else(|| SessionError::Config("command deve conter um executável".into()))?;
        let directory = tempfile::Builder::new().prefix("hcc-agent-").tempdir()
            .map_err(|error| SessionError::Startup(error.to_string()))?;
        let path = directory.path().join("connection.json");
        write_connection(&path, connection).await?;
        let child = Command::new(program).args(arguments).arg("--config").arg(&path)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit())
            .kill_on_drop(true).spawn().map_err(|error| SessionError::Startup(error.to_string()))?;
        Ok(Self { directory: Some(directory), child, path })
    }

    pub(crate) fn try_wait(&mut self) -> Result<Option<i32>, String> {
        self.child.try_wait().map(|status| status.map(crate::sessao::exit_code))
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn last_error(&self) -> Option<String> {
        match tokio::fs::read(self.path.with_extension("error.log")).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).trim().lines().last().map(str::to_owned),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                eprintln!("não foi possível ler motivo do agente: {error}");
                None
            }
        }
    }

    pub(crate) async fn close(&mut self) -> Result<(), String> {
        if self.directory.is_none() { return Ok(()); }
        if self.child.try_wait().map_err(|error| error.to_string())?.is_none() {
            self.child.start_kill().map_err(|error| error.to_string())?;
        }
        tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await
            .map_err(|_| "processo do agente não encerrou".to_string())?
            .map_err(|error| error.to_string())?;
        if let Some(directory) = self.directory.take() {
            directory.close().map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(crate) fn close_sync(&mut self) -> Result<(), String> {
        if self.directory.is_none() { return Ok(()); }
        if self.child.try_wait().map_err(|error| error.to_string())?.is_none() {
            self.child.start_kill().map_err(|error| error.to_string())?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.child.try_wait().map_err(|error| error.to_string())?.is_none() {
                if Instant::now() >= deadline { return Err("processo do agente não encerrou".into()); }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        if let Some(directory) = self.directory.take() {
            directory.close().map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

async fn write_connection(path: &Path, connection: &Connection) -> Result<(), SessionError> {
    let body = serde_json::to_vec(connection).map_err(|error| SessionError::Startup(error.to_string()))?;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await.map_err(|error| SessionError::Startup(error.to_string()))?;
    tokio::io::AsyncWriteExt::write_all(&mut file, &body).await
        .map_err(|error| SessionError::Startup(error.to_string()))?;
    // Tokio pode concluir write_all antes de terminar a escrita no arquivo.
    tokio::io::AsyncWriteExt::flush(&mut file).await
        .map_err(|error| SessionError::Startup(error.to_string()))?;
    drop(file);
    Ok(())
}

impl Drop for LocalTransport {
    fn drop(&mut self) {
        if let Err(error) = self.close_sync() { eprintln!("não foi possível encerrar agente: {error}"); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::mpsc;

    fn hold_pool_slot() -> (mpsc::Sender<()>, mpsc::Receiver<()>) {
        let (release, blocked) = mpsc::channel();
        let (entered, started) = mpsc::channel();
        tokio::task::spawn_blocking(move || {
            let _ = entered.send(());
            let _ = blocked.recv();
        });
        (release, started)
    }

    #[test]
    fn connection_is_complete_and_private_when_write_returns() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all()
            .max_blocking_threads(1).build().unwrap();
        runtime.block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("connection.json");
            let connection = Connection { url: "http://127.0.0.1:12345/next".into(), token: "fixture-token".into() };
            let (release_open, opening) = hold_pool_slot();
            opening.recv_timeout(Duration::from_secs(3)).unwrap();
            let mut writing = Box::pin(write_connection(&path, &connection));
            std::future::poll_fn(|context| {
                assert!(writing.as_mut().poll(context).is_pending());
                std::task::Poll::Ready(())
            }).await;
            let (release_write, queued) = hold_pool_slot();
            release_open.send(()).unwrap();
            queued.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(path.exists());
            std::future::poll_fn(|context| {
                assert!(writing.as_mut().poll(context).is_pending(), "write returned before pending IO completed");
                std::task::Poll::Ready(())
            }).await;
            release_write.send(()).unwrap();
            writing.await.unwrap();
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(serde_json::from_slice::<Connection>(&bytes).unwrap(), connection);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            }
        });
    }
}
