use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hcc_protocolo::{Action, ActionType, Connector, Observation, Session, SessionError};
use regex::Regex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::time::{Instant, sleep, timeout, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::barreiras::{Barreiras, aviso, fecha_janela, objetivo_pede_fechar};
use crate::candidatos::{candidatos, py_str, segredos};
use crate::descrever::{descrever, intencao, py_repr};
use crate::estado::{assinatura, estado_compacto, sem_controles};
use crate::geometria::{dentro, para_tela};
use crate::http::py_dumps;
use crate::jev::Jev;
use crate::llm::Llm;
use crate::resultado::Resultado;
use crate::tipos::{Ajuda, Candidato, Decisao, Escolha, Progresso, Registro};

const CANCELADO: &str = "RuntimeError: objetivo cancelado; nenhuma nova ação será enviada";
const LIMITE: &str = "TimeoutError: limite de duração atingido; objetivo não confirmado";

pub struct Opcoes {
    pub texto: String,
    pub max_passos: u32,
    pub dados: Map<String, Value>,
    pub limite: Duration,
    pub config: Option<PathBuf>,
    pub jev: Option<Jev>,
    pub llm: Llm,
}

struct Controle<'a> {
    prazo: Instant,
    cancel: &'a CancellationToken,
    progresso: &'a (dyn Fn(Progresso) + Send + Sync),
    tempos: Vec<String>,
}

impl Controle<'_> {
    fn restante(&self) -> Result<Duration, String> {
        if self.cancel.is_cancelled() { return Err(CANCELADO.into()); }
        self.prazo.checked_duration_since(Instant::now()).filter(|r| !r.is_zero()).ok_or_else(|| LIMITE.into())
    }

    async fn rodar<T>(&self, tarefa: impl Future<Output = Result<T, String>>) -> Result<T, String> {
        self.restante()?;
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(CANCELADO.into()),
            r = timeout_at(self.prazo, tarefa) => r.unwrap_or_else(|_| Err(LIMITE.into())),
        }
    }

    async fn medir<T>(&mut self, nome: &str, progresso: Progresso, tarefa: impl Future<Output = Result<T, String>>) -> Result<T, String> {
        self.restante()?;
        (self.progresso)(progresso);
        let inicio = Instant::now();
        let r = self.rodar(tarefa).await;
        self.tempos.push(format!("{nome}={:.2}s", inicio.elapsed().as_secs_f64()));
        r
    }

    async fn esperar(&self, tempo: Duration) -> Result<(), String> {
        self.rodar(async { sleep(tempo).await; Ok(()) }).await
    }
}

struct Segredos {
    valores: HashSet<String>,
    padrao: Option<Regex>,
}

impl Segredos {
    fn new(dados: &Map<String, Value>) -> Result<Self, String> {
        let valores = segredos(dados);
        let mut partes: Vec<&str> = valores.iter().filter(|s| !s.is_empty()).map(String::as_str).collect();
        partes.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        let partes: Vec<String> = partes.into_iter().map(regex::escape).collect();
        let padrao = if partes.is_empty() { None } else {
            Some(Regex::new(&partes.join("|")).map_err(|_| "ValueError: não foi possível preparar a proteção dos segredos".to_owned())?)
        };
        Ok(Self { valores, padrao })
    }

    fn texto(&self, texto: &str) -> String {
        self.padrao.as_ref().map_or_else(|| texto.into(), |r| r.replace_all(texto, "<senha>").into_owned())
    }

    fn valor(&self, valor: &mut Value) {
        match valor {
            Value::String(s) => *s = self.texto(s),
            Value::Array(a) => a.iter_mut().for_each(|v| self.valor(v)),
            Value::Object(m) => {
                let original = std::mem::take(m);
                for (k, mut v) in original {
                    self.valor(&mut v);
                    m.insert(self.texto(&k), v);
                }
            }
            _ => {}
        }
    }

    fn numeros(&self, valor: &mut Value) {
        match valor {
            Value::Number(_) | Value::Bool(_) if self.valores.contains(&py_str(valor)) => *valor = json!("<senha>"),
            Value::Array(a) => a.iter_mut().for_each(|v| self.numeros(v)),
            Value::Object(m) => m.values_mut().for_each(|v| self.numeros(v)),
            _ => {}
        }
    }

    fn estado(&self, op: &Opcoes, obs: &Observation, recentes: &[Registro], dica: Option<&str>) -> Value {
        let mut estado = estado_compacto(&op.texto, &op.dados, obs, recentes, dica);
        self.numeros(&mut estado["data"]);
        estado
    }

    fn descricao(&self, acao: &Action, obs: &Observation, com_valor: bool) -> String {
        let mut publica = acao.clone();
        if let Some(v) = publica.value.as_mut().filter(|v| !self.valores.contains(*v)) { *v = self.texto(v); }
        for s in [&mut publica.rotulo, &mut publica.application].into_iter().flatten() { *s = self.texto(s); }
        for s in publica.args.iter_mut().flatten() { *s = self.texto(s); }
        for s in publica.keys.iter_mut().flatten() { *s = self.texto(s); }
        let mut arvore = obs.clone();
        for w in &mut arvore.windows { w.name = self.texto(&w.name); }
        for e in &mut arvore.elements { e.name = self.texto(&e.name); }
        descrever(&publica, &arvore, &self.valores, com_valor)
    }

    fn candidato(&self, acao: Action, obs: &Observation) -> Candidato {
        Candidato {
            descricao: self.descricao(&acao, obs, true),
            intencao: intencao(&acao, obs),
            acao,
        }
    }
}

fn validar(op: &Opcoes) -> Result<&Jev, String> {
    if op.texto.trim().is_empty() || !(1..=50).contains(&op.max_passos) {
        return Err("ValueError: objetivo vazio ou max_passos fora de 1..50".into());
    }
    op.jev.as_ref().filter(|j| !j.key.is_empty()).filter(|_| op.config.is_some())
        .ok_or_else(|| "ValueError: faltam TYPESAFE_API_KEY ou HCC_AGENT_CONFIG".into())
}

fn erro_sessao(e: SessionError) -> String { format!("{}: {e}", e.tipo()) }
fn erro_io(e: io::Error) -> String { format!("OSError: {e}") }
fn reconectavel(e: &SessionError) -> bool { matches!(e, SessionError::Timeout(_) | SessionError::Disconnected(_)) }
fn escolha(e: Escolha) -> String {
    match e {
        Escolha::Indice(i) => i.to_string(), Escolha::Done => "DONE".into(),
        Escolha::Wait => "WAIT".into(), Escolha::Blocked => "BLOCKED".into(),
    }
}
fn percentual(p: f64) -> String { format!("{:.0}%", p * 100.0) }

async fn observar<S: Session>(sessao: &mut S) -> Result<Observation, SessionError> {
    for tentativa in 0..3 {
        match sessao.observe().await {
            Ok(obs) => {
                return if obs.connected && !obs.observation_id.is_empty() { Ok(obs) }
                else { Err(SessionError::Agent("desktop desconectado ou observação sem referência".into())) };
            }
            Err(e) if tentativa < 2 && e.tipo() == "RuntimeError"
                && (e.to_string().contains("janela ativa mudou") || e.to_string().contains("COMError")) => sleep(Duration::from_millis(300)).await,
            Err(e) => return Err(e),
        }
    }
    unreachable!("três tentativas sempre retornam")
}

async fn registrar(pasta: &Path, mut valor: Value, secretos: &Segredos) -> Result<(), String> {
    secretos.valor(&mut valor);
    let mut log = tokio::fs::OpenOptions::new().create(true).append(true).open(pasta.join("jev.jsonl")).await.map_err(erro_io)?;
    log.write_all(format!("{}\n", py_dumps(&valor)).as_bytes()).await.map_err(erro_io)?;
    log.flush().await.map_err(erro_io)
}

async fn ver<S: Session>(op: &Opcoes, sessao: &mut S, controle: &mut Controle<'_>, r: &mut Resultado,
    obs: &Observation, estado: &Value, secretos: &Segredos) -> Result<Ajuda, String> {
    let imagem = controle.medir("captura", Progresso::Captura, async { sessao.screenshot().await.map_err(erro_sessao) }).await?;
    let captura = r.registro.as_ref().ok_or("RuntimeError: registro indisponível")?.join("tela.png");
    controle.rodar(async { tokio::fs::write(&captura, &imagem).await.map_err(erro_io) }).await?;
    r.captura = Some(captura);
    let limite = controle.restante()?.min(Duration::from_secs(45));
    let mut ajuda = controle.medir("llm", Progresso::Llm, async {
        match op.llm.pedir(estado, &imagem, limite).await {
            Ok(a) => Ok(a),
            Err(f) if matches!(f.tipo, "KeyError" | "IndexError" | "AttributeError") => Err(format!("{}: {}", f.tipo, secretos.texto(&f.msg))),
            Err(f) => Ok(Ajuda {
                interpretacao: format!("ajuda visual indisponível: {}: {}", f.tipo, secretos.texto(&f.msg).chars().take(300).collect::<String>()),
                acoes: vec![], impedimento: String::new(),
            }),
        }
    }).await?;
    ajuda.interpretacao = secretos.texto(&ajuda.interpretacao);
    ajuda.impedimento = secretos.texto(&ajuda.impedimento);
    ajuda.acoes = ajuda.acoes.iter().map(|a| para_tela(a, &obs.screen, &op.texto)).collect();
    Ok(ajuda)
}

async fn reconectar<C: Connector>(conector: &C, sessao: &mut Option<C::S>, controle: &mut Controle<'_>) -> Result<(), String> {
    if let Some(s) = sessao.as_mut() {
        controle.rodar(async { s.close().await.map_err(|e| format!("RuntimeError: {e}")) }).await?;
    }
    *sessao = None;
    *sessao = Some(controle.medir("reconexao", Progresso::Reconexao,
        async { conector.connect(controle.cancel).await.map_err(erro_sessao) }).await?);
    Ok(())
}

async fn ciclo<C: Connector>(op: &Opcoes, conector: &C, sessao: &mut Option<C::S>, controle: &mut Controle<'_>,
    r: &mut Resultado, secretos: &Segredos) -> Result<(), String> {
    let jev = validar(op)?;
    let pasta = tempfile::Builder::new().prefix("hcu-").tempdir().map_err(erro_io)?.keep();
    r.registro = Some(pasta.clone());
    *sessao = Some(controle.medir("conexao", Progresso::Conexao,
        async { conector.connect(controle.cancel).await.map_err(erro_sessao) }).await?);
    let mut recentes: Vec<Registro> = Vec::new();
    let mut anterior = None;
    let mut barreiras = Barreiras::default();
    for ciclo in 0..=op.max_passos {
        let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
        let obs = controle.medir("observacao", Progresso::Observacao,
            async { observar(s).await.map_err(erro_sessao) }).await?;
        let atual = assinatura(&obs);
        if let Some(ultima) = recentes.last_mut().filter(|r| r.screen_changed.is_none()) {
            ultima.screen_changed = Some(anterior.as_ref() != Some(&atual));
        }
        anterior = Some(atual.clone());
        let mut gravada = serde_json::to_value(&obs).map_err(|e| format!("ValueError: {e}"))?;
        secretos.valor(&mut gravada);
        controle.rodar(async { tokio::fs::write(pasta.join("observacao.json"), py_dumps(&gravada)).await.map_err(erro_io) }).await?;
        if barreiras.aviso_repetido(&obs).is_some() {
            let texto: String = secretos.texto(&aviso(&obs).unwrap_or_default()).chars().take(400).collect();
            r.motivo = format!("o aplicativo respondeu duas vezes com o mesmo aviso: {texto}"); return Ok(());
        }
        let cands = candidatos(&obs, &op.dados, &secretos.valores).into_iter().map(|mut c| {
            c.descricao = secretos.descricao(&c.acao, &obs, true); c
        }).collect();
        if let Some(motivo) = Barreiras::tres_sem_efeito(&recentes) {
            r.motivo = motivo; return Ok(());
        }
        let mut acoes = Barreiras::barrar(cands, &recentes);
        if let Some(mut clique) = Barreiras::reclique(&mut recentes, &obs) {
            clique.descricao = secretos.texto(&clique.descricao);
            acoes.push(clique);
        }
        let (mut dica, mut ajudou, mut direta) = (String::new(), false, None);
        if sem_controles(&obs) {
            let estado = secretos.estado(op, &obs, &recentes, None);
            let ajuda = ver(op, sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?, controle, r, &obs, &estado, secretos).await?;
            if !ajuda.impedimento.is_empty() { r.motivo = ajuda.impedimento; return Ok(()); }
            dica = ajuda.interpretacao; ajudou = true;
            let propostas = ajuda.acoes.into_iter().filter(|a| matches!(a.kind, ActionType::Mouse | ActionType::Keys))
                .map(|a| secretos.candidato(a, &obs)).collect();
            let propostas = Barreiras::barrar(propostas, &recentes);
            for proposta in propostas.iter().filter(|p| dentro(&p.acao, &obs)) {
                if !acoes.iter().any(|a| a.acao == proposta.acao) { acoes.push(proposta.clone()); }
            }
            if let Some(proposta) = propostas.first().filter(|p| dentro(&p.acao, &obs)) {
                let limite = controle.restante()?.min(Duration::from_secs(30));
                let risco = controle.medir("jev", Progresso::Jev, jev.arriscado(&estado, &proposta.descricao, limite)).await?;
                controle.rodar(registrar(&pasta, json!({"ciclo":ciclo, "direta":proposta.descricao, "risky":risco, "dica":dica}), secretos)).await?;
                let i = acoes.iter().position(|a| a.acao == proposta.acao).ok_or("IndexError: list index out of range")?;
                direta = Some(Decisao { escolha: Escolha::Indice(i), p: 1.0, risco, top: vec![] });
            }
        }
        let mut decisao = loop {
            if let Some(d) = direta.take() { break d; }
            let estado = secretos.estado(op, &obs, &recentes, Some(&dica));
            let limite = controle.restante()?.min(Duration::from_secs(30));
            let d = controle.medir("jev", Progresso::Jev, jev.decidir(&estado, &acoes, limite)).await?;
            let opcao = escolha(d.escolha);
            // Python conta caracteres do JSON ASCII, apesar do nome bytes_estado.
            let ascii = py_dumps(&estado).chars().map(|c| if c.is_ascii() { 1 } else if u32::from(c) > 0xffff { 12 } else { 6 }).sum::<usize>();
            let mut log = json!({"ciclo":ciclo, "n_acoes":acoes.len(), "bytes_estado":ascii,
                "choice":opcao, "probability":d.p, "risky":d.risco});
            if !d.top.is_empty() { log["top"] = json!(d.top); }
            controle.rodar(registrar(&pasta, log, secretos)).await?;
            controle.restante()?;
            if d.escolha == Escolha::Done && d.p >= 0.75 {
                if ajudou { r.motivo = format!("não confirmado: só a leitura do print indica que terminou. {dica}"); }
                else {
                    r.ok = true;
                    let janela = estado["window"].as_str().map_or_else(|| "None".into(), py_repr);
                    r.motivo = format!("objetivo confirmado ({}) na janela {janela}", percentual(d.p));
                }
                return Ok(());
            }
            let esperando = recentes.iter().rev().take(3).filter(|r| r.action == "WAIT").count() >= 3;
            let fraco = d.escolha == Escolha::Blocked || d.p < if d.escolha == Escolha::Done { 0.75 } else { 0.25 }
                || (d.escolha == Escolha::Wait && esperando);
            if !fraco && (!obs.elements.is_empty() && !obs.truncated || ajudou) { break d; }
            if ajudou {
                r.motivo = format!("sem ação segura: Jev escolheu {opcao} com {}. {dica}", percentual(d.p)).trim().into();
                return Ok(());
            }
            ajudou = true;
            let ajuda = ver(op, sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?, controle, r, &obs, &estado, secretos).await?;
            dica = ajuda.interpretacao;
            if !ajuda.impedimento.is_empty() { r.motivo = ajuda.impedimento; return Ok(()); }
            let propostas = ajuda.acoes.into_iter().filter(|a| a.kind != ActionType::Focus && a.target.as_ref().is_none_or(|id|
                obs.windows.iter().any(|w| &w.id == id) || obs.elements.iter().any(|e| &e.id == id)))
                .map(|a| secretos.candidato(a, &obs)).collect();
            for proposta in Barreiras::barrar(propostas, &recentes) {
                if !acoes.iter().any(|a| a.acao == proposta.acao) { acoes.push(proposta); }
            }
        };
        if decisao.escolha == Escolha::Wait {
            controle.esperar(Duration::from_millis(500)).await?;
            recentes.push(Registro { action: "WAIT".into(), result: "esperou 0,5 s".into(), screen_changed: None,
                target_rect: None, clique: None, executada: false });
            continue;
        }
        let Escolha::Indice(i) = decisao.escolha else { return Err("ValueError: opção inválida".into()); };
        let candidato = acoes.get(i).ok_or("IndexError: list index out of range")?;
        let acao = &candidato.acao;
        let nome = &candidato.descricao;
        if fecha_janela(acao, &obs) && !objetivo_pede_fechar(&op.texto) { decisao.risco = decisao.risco.max(1.0); }
        if decisao.risco >= 0.5 {
            r.motivo = format!("ação arriscada não executada: {nome} (risco {}). Autorize no objetivo.", percentual(decisao.risco));
            return Ok(());
        }
        if ciclo == op.max_passos { break; }
        let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
        let rotulo = secretos.descricao(acao, &obs, false);
        let retorno = controle.medir(&format!("acao:{}", acao.kind), Progresso::Acao(rotulo),
            async { Ok(s.act(&obs.observation_id, acao).await) }).await?;
        match retorno {
            Ok(ret) => {
                let alvo = acao.target.as_ref().and_then(|id| obs.elements.iter().rev().find(|e| &e.id == id));
                let clique = match (acao.kind, acao.mode, acao.x, acao.y) {
                    (ActionType::Mouse, Some(m), Some(x), Some(y)) => Some((m, x, y)), _ => None,
                };
                recentes.push(Registro { action: nome.clone(), result: if ret.ok { "ok".into() } else { "{'ok': False}".into() },
                    screen_changed: None, target_rect: alvo.and_then(|e| e.rect), clique, executada: true });
            }
            Err(e) => {
                recentes.push(Registro { action: nome.clone(), result: secretos.texto(&format!("NOT executed: {e}")),
                    screen_changed: Some(false), target_rect: None, clique: None, executada: false });
                if reconectavel(&e) && !controle.cancel.is_cancelled() { reconectar(conector, sessao, controle).await?; }
            }
        }
        r.passos.push(format!("{}. {nome} [{}]", r.passos.len() + 1, percentual(decisao.p)));
        let prazo = Instant::now() + Duration::from_secs(if acao.kind == ActionType::Launch { 45 } else { 3 });
        controle.esperar(Duration::from_millis(800)).await?;
        while Instant::now() < prazo {
            let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
            match controle.rodar(async { Ok(observar(s).await) }).await? {
                Ok(nova) if assinatura(&nova) != atual => break,
                Ok(_) => {}
                Err(e) if reconectavel(&e) => reconectar(conector, sessao, controle).await?,
                Err(e) if e.tipo() == "RuntimeError" => eprintln!("observação durante espera ignorada: {}", secretos.texto(&e.to_string())),
                Err(e) => return Err(erro_sessao(e)),
            }
            controle.esperar(Duration::from_secs(1)).await?;
        }
    }
    r.motivo = format!("limite de {} ciclos; objetivo não confirmado", op.max_passos);
    Ok(())
}

pub async fn executar<C: Connector>(mut op: Opcoes, conector: &C, cancel: CancellationToken,
    progresso: &(dyn Fn(Progresso) + Send + Sync)) -> Resultado
where C::S: 'static {
    let inicio = Instant::now();
    let mut r = Resultado::default();
    let mut secretos = None;
    op.config = op.config.filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HCC_AGENT_CONFIG").filter(|v| !v.is_empty()).map(PathBuf::from));
    let user = ["LOGNAME", "USER", "LNAME", "USERNAME"].iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty())).unwrap_or_default();
    let config = op.config.as_ref().map_or_else(|| "None".into(), |p| p.to_string_lossy().into_owned());
    let digest: String = Sha256::digest(format!("{user}:{config}").as_bytes()).iter().take(6).map(|b| format!("{b:02x}")).collect();
    let trava = std::env::temp_dir().join(format!("hangar-computer-control-{digest}.lock"));
    let mut controle = Controle { prazo: inicio.checked_add(op.limite).unwrap_or(inicio), cancel: &cancel, progresso, tempos: vec![] };
    let mut sessao = None;
    let lock = controle.rodar(async {
        let file = tokio::fs::OpenOptions::new().create(true).append(true).open(trava).await.map_err(erro_io)?;
        Ok(file.into_std().await)
    }).await;
    match lock {
        Err(e) => r.motivo = e,
        Ok(file) => {
            match file.try_lock() {
                Err(std::fs::TryLockError::WouldBlock) => r.motivo = "outro objetivo ainda está controlando o desktop".into(),
                Err(std::fs::TryLockError::Error(e)) => r.motivo = erro_io(e),
                Ok(()) => {
                    match validar(&op).and_then(|_| Segredos::new(&op.dados)) {
                        Ok(s) => {
                            if let Err(e) = ciclo(&op, conector, &mut sessao, &mut controle, &mut r, &s).await { r.motivo = e; }
                            secretos = Some(s);
                        }
                        Err(e) => r.motivo = e,
                    }
                    let mut lenta = false;
                    if let Some(s) = &mut sessao {
                        let restante = controle.prazo.saturating_duration_since(Instant::now());
                        let fechamento = match timeout(restante, s.close()).await {
                            Ok(r) => r,
                            Err(_) => { lenta = true; Ok(()) }
                        };
                        if let Err(e) = fechamento {
                            r.ok = false;
                            r.motivo.push_str(&format!("; falha ao fechar sessão: {e}"));
                        }
                    }
                    if lenta {
                        if let Some(s) = sessao.take() {
                            // O Drop pode esperar a limpeza remota; o lock fica ocupado até ela terminar.
                            let limpeza = tokio::task::spawn_blocking(move || { drop(s); drop(file); });
                            drop(limpeza);
                        }
                    } else { drop(sessao.take()); }
                }
            }
        }
    }
    if let Some(secretos) = &secretos { r.motivo = secretos.texto(&r.motivo); }
    r.tempos = controle.tempos;
    r.tempos.push(format!("total={:.2}s", inicio.elapsed().as_secs_f64()));
    r
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn log_write_error_is_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/dev/full", dir.path().join("jev.jsonl")).unwrap();
        let secretos = Segredos::new(&Map::new()).unwrap();
        let r = registrar(dir.path(), json!({"choice":"DONE"}), &secretos).await;
        assert!(r.is_err(), "erro da gravação precisa chegar ao loop");
    }
}
