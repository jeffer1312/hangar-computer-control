//! Types shared by the loop modules.

use hcc_protocolo::{Action, MouseMode, Rect};
use serde::Deserialize;

#[derive(Clone, Debug, PartialEq)]
pub struct Candidato {
    pub acao: Action,
    pub descricao: String,
    pub intencao: Intencao,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Intencao {
    pub tipo: String,
    pub alvo: String,
    pub valor: Option<String>,
    pub teclas: Vec<String>,
    pub aplicacao: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Registro {
    pub action: String,
    pub result: String,
    pub screen_changed: Option<bool>,
    pub target_rect: Option<Rect>,
    pub clique: Option<(MouseMode, i32, i32)>,
    pub executada: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Escolha {
    Indice(usize),
    Done,
    Wait,
    Blocked,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Decisao {
    pub escolha: Escolha,
    pub p: f64,
    pub risco: f64,
    pub top: Vec<(String, f64, String)>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Ajuda {
    pub interpretacao: String,
    #[serde(default)]
    pub acoes: Vec<Action>,
    #[serde(default)]
    pub impedimento: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progresso {
    Conexao,
    Reconexao,
    Observacao,
    Jev,
    Captura,
    Llm,
    Acao(String),
}
