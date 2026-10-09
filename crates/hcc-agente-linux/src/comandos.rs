use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::runtime::Runtime;

pub trait Comandos {
    fn run(&self, argv: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, String>;
    fn exists(&self, name: &str) -> bool;
}

pub struct ComandosSistema { runtime: Runtime }

impl ComandosSistema {
    pub fn new() -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        Ok(Self { runtime })
    }
}

impl Comandos for ComandosSistema {
    fn run(&self, argv: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
        let program = argv.first().ok_or("comando vazio")?;
        self.runtime.block_on(async {
            let mut command = tokio::process::Command::new(program);
            command.args(&argv[1..]).stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
                .stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
            let task = async {
                let mut child = command.spawn().map_err(|e| format!("{program}: {e}"))?;
                let pipe = child.stdin.take();
                let write = async {
                    if let (Some(mut pipe), Some(bytes)) = (pipe, input)
                        && let Err(error) = pipe.write_all(bytes).await
                        && error.kind() != std::io::ErrorKind::BrokenPipe {
                        return Err(format!("{program}: {error}"));
                    }
                    Ok::<(), String>(())
                };
                let wait = async { child.wait_with_output().await.map_err(|e| format!("{program}: {e}")) };
                let (_, output) = tokio::try_join!(write, wait)?;
                if !output.status.success() {
                    let rc = output.status.code().unwrap_or_else(|| {
                        use std::os::unix::process::ExitStatusExt;
                        -output.status.signal().unwrap_or(1)
                    });
                    let bytes = if output.stderr.is_empty() { &output.stdout } else { &output.stderr };
                    let text = String::from_utf8_lossy(bytes);
                    let text = text.trim();
                    let detail: String = text.chars().skip(text.chars().count().saturating_sub(500)).collect();
                    return Err(format!("{program} falhou ({rc}): {detail}"));
                }
                Ok(output.stdout)
            };
            tokio::time::timeout(Duration::from_secs(15), task).await.map_err(|_| format!("{program} excedeu 15 segundos"))?
        })
    }

    fn exists(&self, name: &str) -> bool {
        let executable = |path: &Path| path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
        if name.contains('/') { executable(Path::new(name)) }
        else { std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| executable(&dir.join(name)))) }
    }
}
