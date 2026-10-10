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

use crate::barreiras::{Barreiras, aviso, escolhe_foco, fecha_janela, itens_pendentes, objetivo_pede_fechar, pendencia, segurar_segredos, texto_sem_alvo_barrado};
use crate::candidatos::{apps_do_objetivo, candidatos,descrever_acao, py_str, segredos, texto_no_editavel};
use crate::descrever::{intencao, py_repr};
use crate::estado::{assinatura_tela, estado_compacto, padrao_segredos, sem_controles};
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
        let padrao = padrao_segredos(&valores);
        if padrao.is_none() && valores.iter().any(|s| !s.is_empty()) {
            return Err("ValueError: não foi possível preparar a proteção dos segredos".to_owned());
        }
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
        descrever_acao(&publica, &arvore, &self.valores, com_valor)
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

/// Classes of the apps in front when an action ran that are now closed: the window and every other window of
/// its process are gone. A dialog closing leaves its main window; pid 0 is the agent's pseudo-window for the desktop.
fn apps_fechados(em_foco: &[hcc_protocolo::Window], agora: &Observation) -> Vec<String> {
    em_foco.iter()
        .filter(|w| w.process_id != 0 && !agora.windows.iter().any(|a| a.id == w.id || a.process_id == w.process_id))
        .filter_map(|w| w.class_name.rsplit('.').next().map(str::to_lowercase).filter(|c| c.chars().count() >= 3))
        .collect()
}

/// After the loop closed an app, launching it again reopened it and Jev called that DONE.
// ponytail: matched by window class inside the launch's executable or name; generic classes (TMainForm,
// ApplicationFrameWindow) match nothing, so those apps can still be reopened.
fn sem_reabrir(cands: Vec<Candidato>, fechados: &[String]) -> Vec<Candidato> {
    if fechados.is_empty() { return cands; }
    let reabre = |a: &Action| {
        let exe = a.application.as_deref().unwrap_or_default().rsplit(['/', '\\']).next().unwrap_or_default().to_lowercase();
        let exe = exe.strip_suffix(".exe").unwrap_or(&exe).to_owned();
        let rotulo = a.rotulo.as_deref().unwrap_or_default().to_lowercase();
        // Never class-contains-exe: Chrome_WidgetWin_1 is shared by VS Code, Teams and Edge, and contains "chrome".
        fechados.iter().any(|c| exe.contains(c.as_str()) || rotulo.contains(c.as_str()))
    };
    cands.into_iter().filter(|c| c.acao.kind != ActionType::Launch || !reabre(&c.acao)).collect()
}

fn erro_sessao(e: SessionError) -> String { format!("{}: {e}", e.tipo()) }
fn erro_io(e: io::Error) -> String { format!("OSError: {e}") }
fn erro_trava(e: io::Error) -> String {
    #[cfg(windows)]
    if e.kind() == io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(32 | 33)) {
        return "outro objetivo ainda está controlando o desktop".into();
    }
    erro_io(e)
}
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
    obs: &Observation, estado: &Value, secretos: &Segredos) -> Result<(Ajuda, bool), String> {
    let imagem = controle.medir("captura", Progresso::Captura, async { sessao.screenshot().await.map_err(erro_sessao) }).await?;
    let captura = r.registro.as_ref().ok_or("RuntimeError: registro indisponível")?.join("tela.png");
    controle.rodar(async { tokio::fs::write(&captura, &imagem).await.map_err(erro_io) }).await?;
    r.captura = Some(captura);
    let limite = controle.restante()?.min(Duration::from_secs(45));
    let (mut ajuda, leitura_valida) = controle.medir("llm", Progresso::Llm, async {
        match op.llm.pedir(estado, &imagem, limite).await {
            Ok(a) => Ok((a, true)),
            Err(f) if matches!(f.tipo, "KeyError" | "IndexError" | "AttributeError") => Err(format!("{}: {}", f.tipo, secretos.texto(&f.msg))),
            Err(f) => Ok((Ajuda {
                interpretacao: format!("ajuda visual indisponível: {}: {}", f.tipo, secretos.texto(&f.msg).chars().take(300).collect::<String>()),
                acoes: vec![], impedimento: String::new(),
            }, false)),
        }
    }).await?;
    ajuda.interpretacao = secretos.texto(&ajuda.interpretacao);
    ajuda.impedimento = secretos.texto(&ajuda.impedimento);
    ajuda.acoes = ajuda.acoes.iter().map(|a| para_tela(a, &obs.screen, &op.texto)).collect();
    Ok((ajuda, leitura_valida))
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
    let mut recusas = HashSet::new();
    let mut foco_escolhido = false;
    let mut em_foco_nas_acoes: Vec<hcc_protocolo::Window> = Vec::new();
    let mut fechados: Vec<String> = Vec::new();
    for ciclo in 0..=op.max_passos {
        let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
        let obs = controle.medir("observacao", Progresso::Observacao,
            async { observar(s).await.map_err(erro_sessao) }).await?;
        let atual = assinatura_tela(&obs);
        if let Some(ultima) = recentes.last_mut().filter(|r| r.screen_changed.is_none()) {
            ultima.screen_changed = Some(anterior.as_ref() != Some(&atual));
        }
        anterior = Some(atual.clone());
        fechados.extend(apps_fechados(&std::mem::take(&mut em_foco_nas_acoes), &obs));
        let mut gravada = serde_json::to_value(&obs).map_err(|e| format!("ValueError: {e}"))?;
        secretos.valor(&mut gravada);
        controle.rodar(async { tokio::fs::write(pasta.join("observacao.json"), py_dumps(&gravada)).await.map_err(erro_io) }).await?;
        if barreiras.aviso_repetido(&obs).is_some() {
            let texto: String = secretos.texto(&aviso(&obs).unwrap_or_default()).chars().take(400).collect();
            r.motivo = format!("o aplicativo respondeu duas vezes com o mesmo aviso: {texto}"); return Ok(());
        }
        let cands = sem_reabrir(apps_do_objetivo(candidatos(&obs, &op.dados, &secretos.valores), &op.texto), &fechados).into_iter().map(|mut c| {
            c.descricao = secretos.descricao(&c.acao, &obs, true); c
        }).collect();
        if let Some(motivo) = Barreiras::tres_sem_efeito(&recentes) {
            r.motivo = motivo; return Ok(());
        }
        let mut acoes: Vec<Candidato> = segurar_segredos(Barreiras::barrar(cands, &recentes), &op.texto, &recentes, &secretos.valores)
            .into_iter().filter(|c| !texto_sem_alvo_barrado(&c.acao, foco_escolhido)).collect();
        if let Some(mut clique) = Barreiras::reclique(&mut recentes, &obs) {
            clique.descricao = secretos.texto(&clique.descricao);
            acoes.push(clique);
        }
        let (mut dica, mut ajudou, mut direta) = (String::new(), false, None);
        let mut leitura_valida = false;
        let mut fila: Vec<Candidato> = Vec::new();
        let mut falta_dica = None;
        if sem_controles(&obs) {
            let estado = secretos.estado(op, &obs, &recentes, None);
            let (ajuda, leu) = ver(op, sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?, controle, r, &obs, &estado, secretos).await?;
            leitura_valida = leu;
            if !ajuda.impedimento.is_empty() { r.motivo = ajuda.impedimento; return Ok(()); }
            dica = ajuda.interpretacao; ajudou = true;
            let grupos: Vec<Vec<Action>> = ajuda.acoes.into_iter()
                .filter(|a| matches!(a.kind, ActionType::Mouse | ActionType::Keys | ActionType::Text))
                .map(|a| texto_no_editavel(a, &obs)).collect();
            // The first text fits several fields: Jev picks the field, no direct guess.
            let ambigua = grupos.first().is_some_and(|g| g.len() > 1);
            let mut foco_na_fila = foco_escolhido;
            let teclado: Vec<Action> = grupos.iter()
                .take_while(|g| {
                    if g.len() != 1 || !matches!(g[0].kind, ActionType::Keys | ActionType::Text)
                        || texto_sem_alvo_barrado(&g[0], foco_na_fila) { return false; }
                    foco_na_fila |= escolhe_foco(&g[0]);
                    true
                })
                .take(3).map(|g| g[0].clone()).collect();
            let propostas = grupos.into_iter().flatten().map(|a| secretos.candidato(a, &obs)).collect();
            let propostas: Vec<Candidato> = segurar_segredos(Barreiras::barrar(propostas, &recentes), &op.texto, &recentes, &secretos.valores);
            // A proposal dropped by the guards breaks the run: the rest waits for the next cycle.
            let seguidas = propostas.iter().zip(&teclado).take_while(|(p, a)| p.acao == **a && dentro(&p.acao, &obs)).count();
            if seguidas >= 2 { fila = propostas[1..seguidas].to_vec(); }
            for proposta in propostas.iter().filter(|p| dentro(&p.acao, &obs) && !texto_sem_alvo_barrado(&p.acao, foco_escolhido)) {
                if !acoes.iter().any(|a| a.acao == proposta.acao) { acoes.push(proposta.clone()); }
            }
            if let Some(proposta) = propostas.first().filter(|p| !ambigua && dentro(&p.acao, &obs) && !texto_sem_alvo_barrado(&p.acao, foco_escolhido)) {
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
                let mut confirmado = (!ajudou).then_some(d.p);
                let mut leitura_forte = false;
                if ajudou {
                    let sem_dica = secretos.estado(op, &obs, &recentes, None);
                    let limite = controle.restante()?.min(Duration::from_secs(30));
                    let d2 = controle.medir("jev", Progresso::Jev, jev.decidir(&sem_dica, &acoes, limite)).await?;
                    controle.rodar(registrar(&pasta, json!({"ciclo":ciclo, "sem_dica":true,
                        "choice":escolha(d2.escolha), "probability":d2.p, "risky":d2.risco}), secretos)).await?;
                    controle.restante()?;
                    if leitura_valida && d.p >= 0.95 && matches!(d2.escolha, Escolha::Done | Escolha::Wait) {
                        confirmado = Some(d.p);
                        leitura_forte = true;
                    } else if d2.escolha == Escolha::Done && d2.p >= 0.75 { confirmado = Some(d2.p); }
                    // The print reading plus every explicit item of the goal on screen is evidence enough.
                    else if itens_pendentes(&op.texto, &obs) > 0 && pendencia(&op.texto, &obs, &recentes, &secretos.valores).is_none() {
                        confirmado = Some(d.p);
                    }
                }
                match confirmado {
                    Some(p) => {
                        if let Some((indice, falta)) = pendencia(&op.texto, &obs, &recentes, &secretos.valores) {
                            recentes.push(Registro { action: format!("DONE recusado: {falta}"), result: "não confirmado".into(),
                                screen_changed: Some(false), target_rect: None, clique: None, executada: false });
                            if !recusas.insert(indice) {
                                r.motivo = format!("não confirmado: {falta}");
                                return Ok(());
                            }
                            dica = if dica.is_empty() { falta.clone() } else { format!("{falta}\n{dica}") };
                            falta_dica = Some(falta);
                            continue;
                        }
                        r.ok = true;
                        let janela = estado["window"].as_str().map_or_else(|| "None".into(), py_repr);
                        r.motivo = if leitura_forte {
                            format!("objetivo confirmado pela leitura da tela ({}) na janela {janela}", percentual(p))
                        } else { format!("objetivo confirmado ({}) na janela {janela}", percentual(p)) };
                    }
                    None => r.motivo = format!("não confirmado: só a leitura do print indica que terminou. {dica}"),
                }
                return Ok(());
            }
            let esperando = recentes.iter().rev().filter(|r| !r.action.starts_with("DONE recusado: "))
                .take(3).filter(|r| r.action == "WAIT").count() >= 3;
            let fraco = d.escolha == Escolha::Blocked || d.p < if d.escolha == Escolha::Done { 0.75 } else { 0.25 }
                || (d.escolha == Escolha::Wait && esperando);
            if !fraco && (!obs.elements.is_empty() && !obs.truncated || ajudou) { break d; }
            if ajudou {
                r.motivo = format!("sem ação segura: Jev escolheu {opcao} com {}. {dica}", percentual(d.p)).trim().into();
                return Ok(());
            }
            ajudou = true;
            let (ajuda, leu) = ver(op, sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?, controle, r, &obs, &estado, secretos).await?;
            leitura_valida = leu;
            dica = falta_dica.as_ref().map_or_else(|| ajuda.interpretacao.clone(), |f| format!("{f}\n{}", ajuda.interpretacao));
            if !ajuda.impedimento.is_empty() { r.motivo = ajuda.impedimento; return Ok(()); }
            let propostas = ajuda.acoes.into_iter().filter(|a| a.kind != ActionType::Focus && a.target.as_ref().is_none_or(|id|
                obs.windows.iter().any(|w| &w.id == id) || obs.elements.iter().any(|e| &e.id == id)))
                .flat_map(|a| texto_no_editavel(a, &obs)).map(|a| secretos.candidato(a, &obs)).collect();
            for proposta in segurar_segredos(Barreiras::barrar(sem_reabrir(apps_do_objetivo(propostas, &op.texto), &fechados), &recentes), &op.texto, &recentes, &secretos.valores)
                .into_iter().filter(|p| !texto_sem_alvo_barrado(&p.acao, foco_escolhido)) {
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
        let mut candidato = acoes.get(i).ok_or("IndexError: list index out of range")?.clone();
        let mut fila = fila.into_iter();
        let primeiro_plano = obs.foreground.clone();
        let mut vista = obs;
        let mut atual = atual;
        loop {
            let acao = &candidato.acao;
            let nome = &candidato.descricao;
            if fecha_janela(acao, &vista) && !objetivo_pede_fechar(&op.texto) { decisao.risco = decisao.risco.max(1.0); }
            if decisao.risco >= 0.5 {
                r.motivo = format!("ação arriscada não executada: {nome} (risco {}). Autorize no objetivo.", percentual(decisao.risco));
                return Ok(());
            }
            if ciclo == op.max_passos { break; }
            let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
            let rotulo = secretos.descricao(acao, &vista, false);
            let retorno = controle.medir(&format!("acao:{}", acao.kind), Progresso::Acao(rotulo),
                async { Ok(s.act(&vista.observation_id, acao).await) }).await?;
            let falhou = match retorno {
                Ok(ret) => {
                    if ret.ok {
                        foco_escolhido |= escolhe_foco(acao);
                        em_foco_nas_acoes.extend(vista.windows.iter().find(|w| w.id == vista.foreground).cloned());
                    }
                    let alvo = acao.target.as_ref().and_then(|id| vista.elements.iter().rev().find(|e| &e.id == id));
                    let clique = match (acao.kind, acao.mode, acao.x, acao.y) {
                        (ActionType::Mouse, Some(m), Some(x), Some(y)) => Some((m, x, y)), _ => None,
                    };
                    recentes.push(Registro { action: nome.clone(), result: if ret.ok { "ok".into() } else { "{'ok': False}".into() },
                        screen_changed: None, target_rect: alvo.and_then(|e| e.rect), clique, executada: true });
                    !ret.ok
                }
                Err(e) => {
                    recentes.push(Registro { action: nome.clone(), result: secretos.texto(&format!("NOT executed: {e}")),
                        screen_changed: Some(false), target_rect: None, clique: None, executada: false });
                    if reconectavel(&e) && !controle.cancel.is_cancelled() { reconectar(conector, sessao, controle).await?; }
                    true
                }
            };
            r.passos.push(format!("{}. {nome} [{}]", r.passos.len() + 1, percentual(decisao.p)));
            let prazo = Instant::now() + Duration::from_secs(if acao.kind == ActionType::Launch { 45 } else { 3 });
            controle.esperar(Duration::from_millis(800)).await?;
            while Instant::now() < prazo {
                let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
                match controle.rodar(async { Ok(observar(s).await) }).await? {
                    Ok(nova) if assinatura_tela(&nova) != atual => break,
                    Ok(_) => {}
                    Err(e) if reconectavel(&e) => reconectar(conector, sessao, controle).await?,
                    Err(e) if e.tipo() == "RuntimeError" => eprintln!("observação durante espera ignorada: {}", secretos.texto(&e.to_string())),
                    Err(e) => return Err(erro_sessao(e)),
                }
                controle.esperar(Duration::from_secs(1)).await?;
            }
            let Some(proxima) = fila.next().filter(|_| !falhou) else { break };
            // The agent consumes each observation id: the next key/text needs a fresh one.
            let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
            vista = controle.medir("observacao", Progresso::Observacao,
                async { observar(s).await.map_err(erro_sessao) }).await?;
            let nova = assinatura_tela(&vista);
            if let Some(ultima) = recentes.last_mut() { ultima.screen_changed = Some(nova != atual); }
            atual = nova;
            anterior = Some(atual.clone());
            let mut gravada = serde_json::to_value(&vista).map_err(|e| format!("ValueError: {e}"))?;
            secretos.valor(&mut gravada);
            controle.rodar(async { tokio::fs::write(pasta.join("observacao.json"), py_dumps(&gravada)).await.map_err(erro_io) }).await?;
            // A new window or a message box takes the queued keys: the next cycle looks at it first.
            if vista.foreground != primeiro_plano || aviso(&vista).is_some() { break; }
            if texto_sem_alvo_barrado(&proxima.acao, foco_escolhido)
                || Barreiras::tres_sem_efeito(&recentes).is_some() || Barreiras::barrar(vec![proxima.clone()], &recentes).is_empty() { break; }
            let estado = secretos.estado(op, &vista, &recentes, None);
            let limite = controle.restante()?.min(Duration::from_secs(30));
            decisao.risco = controle.medir("jev", Progresso::Jev, jev.arriscado(&estado, &proxima.descricao, limite)).await?;
            controle.rodar(registrar(&pasta, json!({"ciclo":ciclo, "direta":proxima.descricao, "risky":decisao.risco, "dica":dica}), secretos)).await?;
            candidato = proxima;
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
        let mut abrir = tokio::fs::OpenOptions::new();
        abrir.create(true).append(true);
        // Windows LockFileEx needs read or write access; append-only is refused.
        #[cfg(windows)]
        abrir.read(true);
        let file = abrir.open(trava).await.map_err(erro_trava)?;
        Ok(file.into_std().await)
    }).await;
    match lock {
        Err(e) => r.motivo = e,
        Ok(file) => {
            match file.try_lock() {
                Err(std::fs::TryLockError::WouldBlock) => r.motivo = "outro objetivo ainda está controlando o desktop".into(),
                Err(std::fs::TryLockError::Error(e)) => r.motivo = erro_trava(e),
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
