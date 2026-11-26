//! The turn on the keyboard and mouse: several agents on one desktop act one at a time.

use crate::sessao::{DesktopError, ErrorKind, recusa};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

/// Held while one agent acts; dropping it lets the next one in.
pub struct Vez {
    arquivo: File,
}

impl Vez {
    /// Waits up to `espera` for the agent holding it; refused (not delivered) when it does not let go.
    pub fn pegar(caminho: &Path, espera: Duration) -> Result<Vez, DesktopError> {
        let falha = |e: std::io::Error| DesktopError::new(ErrorKind::RuntimeError, format!("{}: {e}", caminho.display())).nao_entregue();
        let arquivo = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(caminho).map_err(falha)?;
        let fim = Instant::now() + espera;
        loop {
            match arquivo.try_lock() {
                Ok(()) => return Ok(Vez { arquivo }),
                Err(TryLockError::WouldBlock) if Instant::now() < fim => std::thread::sleep(Duration::from_millis(50)),
                Err(TryLockError::WouldBlock) => return Err(DesktopError::new(ErrorKind::RuntimeError, recusa::ENTRADA_OCUPADA).nao_entregue()),
                Err(TryLockError::Error(e)) => return Err(falha(e)),
            }
        }
    }

    /// The input clock (Windows `GetLastInputInfo` ticks) of the last input any agent sent, kept in the file.
    pub fn tick(&mut self) -> Option<u32> {
        let mut b = [0u8; 4];
        self.arquivo.seek(SeekFrom::Start(0)).and_then(|_| self.arquivo.read_exact(&mut b)).ok().map(|()| u32::from_le_bytes(b))
    }

    pub fn gravar_tick(&mut self, tick: u32) {
        // Unwritten, the next agent takes this input for the user's and only waits for it to stop: no harm done.
        if let Err(e) = self.arquivo.seek(SeekFrom::Start(0)).and_then(|_| self.arquivo.write_all(&tick.to_le_bytes())) {
            eprintln!("vez: entrada do agente não gravada: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_turn_is_refused_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let caminho = dir.path().join("vez.lock");
        let primeira = Vez::pegar(&caminho, Duration::from_secs(5)).unwrap();
        let e = Vez::pegar(&caminho, Duration::from_millis(50)).err().expect("ocupada");
        assert!(e.to_string().contains(recusa::ENTRADA_OCUPADA) && e.delivery == crate::comando::Delivery::NotDelivered, "{e}");
        drop(primeira);
        assert!(Vez::pegar(&caminho, Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn the_next_holder_reads_the_last_agent_input() {
        let dir = tempfile::tempdir().unwrap();
        let caminho = dir.path().join("vez.lock");
        let mut a = Vez::pegar(&caminho, Duration::from_secs(5)).unwrap();
        assert_eq!(a.tick(), None);
        a.gravar_tick(4_000_000_123);
        drop(a);
        assert_eq!(Vez::pegar(&caminho, Duration::from_secs(5)).unwrap().tick(), Some(4_000_000_123));
    }
}
