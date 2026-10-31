use std::path::PathBuf;

#[derive(Default)]
pub struct Resultado {
    pub ok: bool,
    pub motivo: String,
    pub passos: Vec<String>,
    pub tempos: Vec<String>,
    pub captura: Option<PathBuf>,
    pub registro: Option<PathBuf>,
    pub roteiro: Option<String>,
    pub ultima: Option<UmPasso>,
}

impl Resultado {
    pub fn resumo(&self) -> String {
        let mut linhas = vec![format!("{}: {}", if self.ok { "concluído" } else { "parou" }, self.motivo)];
        linhas.extend(self.passos.iter().cloned());
        linhas.push(format!("tempos: {}", self.tempos.join("; ")));
        linhas.push(self.captura.as_ref().map_or_else(|| "sem captura".into(), |p| format!("captura: {}", p.display())));
        if let Some(p) = &self.registro { linhas.push(format!("registro: {}", p.display())); }
        if let Some(t) = &self.roteiro { linhas.push(format!("roteiro: {t}")); }
        linhas.join("\n")
    }
}

/// The last pick of a run, for the one-step `act` tool.
#[derive(Default, Clone, Debug)]
pub struct UmPasso {
    pub acao: String,
    pub confianca: f64,
    pub segundo: Option<(String, f64)>,
    pub risco: f64,
    pub entrega: Option<String>,
    /// What happened to the action, as recentActions shows it ("NOT executed: … usuário ativo").
    pub resultado: Option<String>,
    pub tela_mudou: Option<bool>,
}
