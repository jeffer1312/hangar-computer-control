//! Observation bookkeeping: one observation authorizes exactly one action.

use hcc_protocolo::{Desktop, DesktopError, ErrorKind, Observation, Scope};
use std::time::{Duration, Instant};

const VALIDADE: Duration = Duration::from_secs(120);

struct Observada {
    id: String,
    em: Instant,
    observacao: Observation,
    /// Read by id, so it may stay behind other windows; it only has to still exist.
    janela: Option<String>,
}

#[derive(Default)]
pub struct Registro {
    atual: Option<Observada>,
}

fn expirada() -> DesktopError {
    DesktopError::new(ErrorKind::RuntimeError, "observação expirada; observe novamente")
}

impl Registro {
    pub fn observar(&mut self, d: &mut impl Desktop, scope: &Scope) -> Result<Observation, DesktopError> {
        // A failed observe must not leave the previous observation usable (windows_uia.py:79).
        self.atual = None;
        let mut observacao = d.observe(scope)?;
        observacao.observation_id = uuid::Uuid::new_v4().simple().to_string();
        self.atual = Some(Observada {
            id: observacao.observation_id.clone(),
            em: Instant::now(),
            observacao: observacao.clone(),
            janela: scope.window.clone(),
        });
        Ok(observacao)
    }

    /// Consumed before returning Ok, so a failing action still spends it (windows_uia.py:245-246).
    pub fn validar_e_consumir(&mut self, d: &mut impl Desktop, observation_id: &str) -> Result<Observation, DesktopError> {
        let atual = self.atual.as_ref().ok_or_else(expirada)?;
        if observation_id.is_empty() || observation_id != atual.id || atual.em.elapsed() > VALIDADE {
            return Err(expirada());
        }
        match &atual.janela {
            Some(janela) if !d.has_window(janela)? => {
                return Err(DesktopError::new(ErrorKind::RuntimeError, "a janela fechou; observe novamente antes de agir"));
            }
            None if d.foreground()? != atual.observacao.foreground => {
                return Err(DesktopError::new(ErrorKind::RuntimeError, "o foco mudou; observe novamente antes de agir"));
            }
            _ => {}
        }
        self.atual.take().map(|o| o.observacao).ok_or_else(expirada)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcc_protocolo::{Action, Delivery, Screen};

    struct Mesa {
        foreground: String,
        falhar_observe: bool,
        abertas: Vec<String>,
    }

    impl Desktop for Mesa {
        fn session_id(&self) -> u32 {
            1
        }
        fn available(&mut self) -> Result<(), DesktopError> {
            Ok(())
        }
        fn foreground(&mut self) -> Result<String, DesktopError> {
            Ok(self.foreground.clone())
        }
        fn has_window(&mut self, id: &str) -> Result<bool, DesktopError> {
            Ok(self.abertas.iter().any(|a| a == id))
        }
        fn observe(&mut self, scope: &Scope) -> Result<Observation, DesktopError> {
            if self.falhar_observe {
                return Err(DesktopError::new(ErrorKind::ComError, "x"));
            }
            Ok(Observation {
                observation_id: String::new(),
                connected: true,
                session_id: 1,
                foreground: scope.window.clone().unwrap_or_else(|| self.foreground.clone()),
                windows: vec![],
                elements: vec![],
                apps: vec![],
                truncated: false,
                timestamp: 0.0,
                screen: Screen { width: 1, height: 1 },
            })
        }
        fn act(&mut self, _: &Observation, _: &Action) -> Result<Delivery, DesktopError> {
            Ok(Delivery::DeliveredUnverified)
        }
        fn screenshot_png(&mut self, _: Option<&str>) -> Result<Vec<u8>, DesktopError> {
            Ok(vec![])
        }
    }

    fn mesa() -> Mesa {
        Mesa { foreground: "w1".into(), falhar_observe: false, abertas: vec!["w1".into(), "w5".into()] }
    }

    #[test]
    fn window_read_by_id_ignores_focus_but_refuses_once_closed() {
        let (mut r, mut d) = (Registro::default(), mesa());
        let escopo = Scope { window: Some("w5".into()), ..Default::default() };
        let id = r.observar(&mut d, &escopo).unwrap().observation_id;
        d.foreground = "w2".into();
        assert_eq!(r.validar_e_consumir(&mut d, &id).unwrap().foreground, "w5");
        let id = r.observar(&mut d, &escopo).unwrap().observation_id;
        d.abertas.retain(|a| a != "w5");
        assert_eq!(r.validar_e_consumir(&mut d, &id).unwrap_err().to_string(), "RuntimeError: a janela fechou; observe novamente antes de agir");
    }

    #[test]
    fn focus_change_refuses_and_keeps_observation() {
        let (mut r, mut d) = (Registro::default(), mesa());
        let id = r.observar(&mut d, &Scope::default()).unwrap().observation_id;
        d.foreground = "w2".into();
        assert_eq!(r.validar_e_consumir(&mut d, &id).unwrap_err().to_string(), "RuntimeError: o foco mudou; observe novamente antes de agir");
        d.foreground = "w1".into();
        assert_eq!(r.validar_e_consumir(&mut d, &id).unwrap().observation_id, id);
    }

    #[test]
    fn wrong_empty_or_old_id_is_expired() {
        let (mut r, mut d) = (Registro::default(), mesa());
        let msg = "RuntimeError: observação expirada; observe novamente";
        assert_eq!(r.validar_e_consumir(&mut d, "").unwrap_err().to_string(), msg);
        let id = r.observar(&mut d, &Scope::default()).unwrap().observation_id;
        assert_eq!(r.validar_e_consumir(&mut d, "outro").unwrap_err().to_string(), msg);
        assert_eq!(r.validar_e_consumir(&mut d, "").unwrap_err().to_string(), msg);
        r.atual.as_mut().unwrap().em = Instant::now() - Duration::from_secs(121);
        assert_eq!(r.validar_e_consumir(&mut d, &id).unwrap_err().to_string(), msg);
    }

    #[test]
    fn failed_observe_drops_previous_observation() {
        let (mut r, mut d) = (Registro::default(), mesa());
        let id = r.observar(&mut d, &Scope::default()).unwrap().observation_id;
        d.falhar_observe = true;
        assert_eq!(r.observar(&mut d, &Scope::default()).unwrap_err().to_string(), "COMError: x");
        assert_eq!(r.validar_e_consumir(&mut d, &id).unwrap_err().to_string(), "RuntimeError: observação expirada; observe novamente");
    }
}
