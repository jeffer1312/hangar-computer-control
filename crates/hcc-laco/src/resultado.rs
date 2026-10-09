use std::path::PathBuf;

#[derive(Default)]
pub struct Resultado {
    pub ok: bool,
    pub motivo: String,
    pub passos: Vec<String>,
    pub tempos: Vec<String>,
    pub captura: Option<PathBuf>,
    pub registro: Option<PathBuf>,
}

impl Resultado {
    pub fn resumo(&self) -> String {
        let mut linhas = vec![format!("{}: {}", if self.ok { "concluído" } else { "parou" }, self.motivo)];
        linhas.extend(self.passos.iter().cloned());
        linhas.push(format!("tempos: {}", self.tempos.join("; ")));
        linhas.push(self.captura.as_ref().map_or_else(|| "sem captura".into(), |p| format!("captura: {}", p.display())));
        if let Some(p) = &self.registro { linhas.push(format!("registro: {}", p.display())); }
        linhas.join("\n")
    }
}
