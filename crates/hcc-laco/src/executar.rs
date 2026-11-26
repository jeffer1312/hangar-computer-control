use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hcc_protocolo::{ActResult, Action, ActionType, Connector, Delivery, InputMode, Observation, RootSpec, Scope, Session, SessionError, recusa};
use regex::Regex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::time::{Instant, sleep, timeout, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::barreiras::{Barreiras, aviso, escolhe_foco, literais_atendidos, sem_acao_pedida, sem_localizadores, fecha_janela, itens_pendentes, objetivo_pede_fechar, pendencia, segurar_segredos, texto_sem_alvo_barrado};
use crate::modo::modo_entrada;
use crate::candidatos::{apps_do_objetivo, candidatos, descrever_acao, mesmo_alvo, py_str, segredos, sem_app_aberto, texto_no_editavel};
use crate::descrever::{intencao, py_repr};
use crate::estado::{AssinaturaTela, assinatura_tela, estado_compacto, padrao_segredos, sem_controles};
use crate::geometria::{dentro, para_janela, para_tela};
use crate::http::py_dumps;
use crate::jev::Jev;
use crate::llm::{Imagem, Llm};
use crate::objetivo::{literais, nome_parecido, nomes_em_janela_nova, nomes_no_titulo, pede_leitura, so_leitura, texto_janela, valores_do_objetivo};
use crate::trecho;
use crate::resultado::Resultado;
use crate::roteiro;
use crate::tipos::{Ajuda, Candidato, Decisao, Envio, Escolha, Progresso, Registro};

const CANCELADO: &str = "RuntimeError: objetivo cancelado; nenhuma nova ação será enviada";
/// How long the loop waits for the user to stop using the computer before giving up an action.
const USUARIO_ATIVO: Duration = Duration::from_secs(60);
const SEM_PROGRESSO: u32 = 6;
const LIMITE: &str = "TimeoutError: limite de duração atingido; objetivo não confirmado";
// Limits for a decision Jev made seeing the screenshot: equal to the tree-only ones until calibrated on live runs.
const FRACO_COM_PRINT: f64 = 0.25;
const DONE_COM_PRINT: f64 = 0.75;
const TRUNCADA_COM_PRINT: f64 = 0.6;
const LIMITE_BLOQUEIO: f64 = 0.5;
/// `imagem` and `motivo` for the record, the reason masked before the cut so no piece of a secret survives it.
fn envio_registro(envio: &Envio, secretos: &Segredos) -> (&'static str, Option<String>) {
    (envio.imagem, (!envio.motivo.is_empty()).then(|| secretos.texto(&envio.motivo).chars().take(300).collect()))
}

/// The lowest probability that still counts for a pick, a DONE and a pick on a truncated tree.
fn limites(d: &Decisao) -> (f64, f64, f64) {
    if d.com_imagem { (FRACO_COM_PRINT, DONE_COM_PRINT, TRUNCADA_COM_PRINT) } else { (0.25, 0.75, 0.6) }
}

pub struct Opcoes {
    pub texto: String,
    pub max_passos: u32,
    pub dados: Map<String, Value>,
    pub limite: Duration,
    pub config: Option<PathBuf>,
    pub jev: Option<Jev>,
    pub llm: Llm,
    /// Folder of recorded scripts; `None` turns record and replay off.
    pub roteiros: Option<PathBuf>,
    pub refazer: bool,
    /// The app window stays in front, actions use real input and the agent draws the cursor: the user watches.
    pub ver: bool,
    /// Decide the first step and stop before acting (the `act` tool with executar=false).
    pub so_decidir: bool,
}

struct Controle<'a> {
    prazo: Instant,
    cancel: &'a CancellationToken,
    progresso: &'a (dyn Fn(Progresso) + Send + Sync),
    tempos: Vec<String>,
    /// Start of the lock file names of this desktop (`<temp>/hangar-computer-control-<digest>`).
    trava: String,
    /// The window this goal works in, locked against the other goals of the desktop.
    janelas: HashMap<String, std::fs::File>,
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

    /// Acts after taking the app's window `lida` (the one read by id), if any: every action of the goal goes here.
    async fn agir<S: Session>(&mut self, s: &mut S, vista: &Observation, acao: &Action, progresso: Progresso, lida: Option<&str>)
        -> Result<Result<ActResult, SessionError>, String> {
        // The window read by id is taken; before the goal has one, the window in front (or the one activated) is only
        // checked. Another goal working there refuses the action before it leaves, like a busy keyboard: the caller
        // looks again and resends, so the Start menu and the taskbar go to one goal at a time.
        let (alvo, tomar) = match (acao.kind, lida) {
            (ActionType::Launch, _) => (None, false),
            // Bringing a window to the front reaches whoever works in it: always the target's.
            (ActionType::Activate, _) => (acao.target.as_deref(), false),
            (_, Some(janela)) => (Some(janela), true),
            (_, None) => (Some(vista.foreground.as_str()), false),
        };
        if let Some(janela) = alvo && !(if tomar { self.tomar(janela)? } else { !self.de_outro(janela)? }) {
            return Ok(Err(SessionError::NotDelivered(format!("RuntimeError: {}", ocupada(janela, vista)))));
        }
        self.medir(&format!("acao:{}", acao.kind), progresso, async { Ok(s.act(&vista.observation_id, acao).await) }).await
    }

    /// Another goal of the desktop took `janela`; checked without taking it.
    fn de_outro(&self, janela: &str) -> Result<bool, String> {
        if self.janelas.contains_key(janela) { return Ok(false); }
        let arquivo = hcc_protocolo::vez::abrir_trava(&self.trava_de(janela)).map_err(erro_io)?;
        match arquivo.try_lock() {
            Ok(()) => arquivo.unlock().map(|()| false).map_err(erro_io),
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(std::fs::TryLockError::Error(e)) => Err(erro_io(e)),
        }
    }

    fn trava_de(&self, janela: &str) -> PathBuf {
        let hash: String = Sha256::digest(janela.as_bytes()).iter().take(6).map(|b| format!("{b:02x}")).collect();
        PathBuf::from(format!("{}-{hash}.lock", self.trava))
    }

    /// Takes `janela` as the window this goal works in, letting go of the one before; false when another goal of
    /// the desktop has it.
    fn tomar(&mut self, janela: &str) -> Result<bool, String> {
        if self.janelas.contains_key(janela) { return Ok(true); }
        let arquivo = hcc_protocolo::vez::abrir_trava(&self.trava_de(janela)).map_err(erro_io)?;
        match arquivo.try_lock() {
            Ok(()) => {
                self.janelas.clear();
                self.janelas.insert(janela.to_owned(), arquivo);
                Ok(true)
            }
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(e)) => Err(erro_io(e)),
        }
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

    fn tem_segredo(&self, valor: &Value) -> bool {
        let casa = |s: &str| self.padrao.as_ref().is_some_and(|r| r.is_match(s));
        match valor {
            Value::String(s) => casa(s),
            Value::Array(a) => a.iter().any(|v| self.tem_segredo(v)),
            Value::Object(m) => m.iter().any(|(k, v)| casa(k) || self.tem_segredo(v)),
            _ => false,
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

    /// The model sees secrets as `<senha>` and asks to type that: with one secret it is that one; with several
    /// nobody knows which, and the action goes. The marker itself is never typed.
    fn desmascarar(&self, mut acao: Action) -> Option<Action> {
        let Some(valor) = acao.value.as_deref().filter(|v| v.contains("<senha>")) else { return Some(acao); };
        let [unico] = self.valores.iter().filter(|s| !s.is_empty()).collect::<Vec<_>>()[..] else { return None; };
        acao.value = Some(valor.replace("<senha>", unico));
        Some(acao)
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
    if op.texto.trim().is_empty() || !(1..=300).contains(&op.max_passos) {
        return Err("ValueError: objetivo vazio ou max_passos fora de 1..300".into());
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

/// A shortcut with a modifier (Ctrl+F, Ctrl+K, Ctrl+L) that just ran put the focus where the next text belongs, often
/// a field the tree does not show (Slack's message search): untargeted text stays there instead of being aimed at
/// another editable field.
fn no_campo(acao: Action, obs: &Observation, recentes: &[Registro]) -> Vec<Action> {
    // Only shortcuts that open a search or an address field: Ctrl+A or Ctrl+W leave no field to type in.
    const FOCAM: &[&str] = &["ctrl+f", "ctrl+k", "ctrl+l", "ctrl+e", "ctrl+g", "ctrl+shift+f", "ctrl+shift+k", "alt+d"];
    let atalho = recentes.iter().rev().find(|r| r.executada).is_some_and(|r| r.result == "ok"
        && r.action.starts_with("keys ") && r.action.rsplit(' ').next().is_some_and(|k| FOCAM.contains(&k.to_lowercase().as_str())));
    let acoes = if atalho && acao.kind == ActionType::Text && acao.target.is_none() { vec![acao] } else { texto_no_editavel(acao, obs) };
    // Text appends: a field already holding it would end up with it twice.
    acoes.into_iter().filter(|a| a.kind != ActionType::Text || !a.target.as_ref().is_some_and(|id| {
        let valor = a.value.as_deref().unwrap_or_default();
        !valor.is_empty() && obs.elements.iter().any(|e| &e.id == id && e.value.as_deref().is_some_and(|v| v.ends_with(valor)))
    })).collect()
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

/// A typing aimed at a named field: its role, name and the value it should leave there.
/// A secret is never checked: a field left with part of it would echo that part in the messages.
fn digitacao(acao: &Action, obs: &Observation, segredos: &HashSet<String>) -> Option<(String, String, String)> {
    if !matches!(acao.kind, ActionType::SetValue | ActionType::Text) { return None; }
    let valor = acao.value.as_deref().filter(|v| !v.trim().is_empty())?;
    if segredos.iter().any(|s| !s.is_empty() && valor.contains(s.as_str())) { return None; }
    let e = acao.target.as_ref().and_then(|id| obs.elements.iter().find(|e| &e.id == id))?;
    (!e.password && !e.name.is_empty()).then(|| (e.role.clone(), e.name.clone(), valor.to_owned()))
}

/// What the field shows when it is not the typed value. Fields that hide their value, or that are gone, say nothing.
fn campo_diferente(obs: &Observation, papel: &str, nome: &str, valor: &str) -> Option<String> {
    let lidos: Vec<&str> = obs.elements.iter().filter(|e| e.role == papel && e.name == nome && !e.password)
        .filter_map(|e| e.value.as_deref()).collect();
    // Masks (CPF, phone, date) add punctuation: only letters and digits are compared.
    let normal = |t: &str| t.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    // A value made only of punctuation ("---") would compare as empty and always match.
    let normal = |t: &str| if normal(valor).is_empty() { t.trim().to_owned() } else { normal(t) };
    // ponytail: `contains` lets a text appended to an existing value pass; an exact check would fail those goals.
    if lidos.is_empty() || lidos.iter().any(|l| normal(l).contains(&normal(valor))) { return None; }
    Some(lidos[0].to_owned())
}

/// `None` when the tree is empty on both sides: an unchanged title then proves nothing either way.
fn mudou(antes: &AssinaturaTela, depois: &AssinaturaTela) -> Option<bool> {
    if antes != depois { Some(true) } else if depois.2.is_empty() { None } else { Some(false) }
}

/// The vision model sometimes names a control that is not in the tree (or the tree is empty): aim at the focus.
fn alvo_observado(mut acao: Action, obs: &Observation) -> Action {
    if acao.target.as_ref().is_some_and(|id| !obs.windows.iter().any(|w| &w.id == id) && !obs.elements.iter().any(|e| &e.id == id)) {
        acao.target = None;
    }
    acao
}

/// The program's name in a launch command (`C:\\Windows\\notepad.exe`, `/usr/bin/gnome-calculator --x`): `notepad`.
fn programa(comando: &str) -> String {
    let exe = comando.split_whitespace().next().unwrap_or_default();
    let nome = exe.rsplit(['/', '\\']).next().unwrap_or(exe).to_lowercase();
    nome.strip_suffix(".exe").map(str::to_owned).unwrap_or(nome)
}

/// The window belongs to the program `app` (its executable on Windows, its class on Linux).
fn do_programa(w: &hcc_protocolo::Window, app: &str) -> bool {
    let processo = w.process.to_lowercase();
    !app.is_empty() && (processo.strip_suffix(".exe").unwrap_or(&processo) == app || w.class_name.to_lowercase().contains(app))
}

/// Invoke on an editable field puts the caret there, like a click.
fn invoca_editavel(acao: &Action, obs: &Observation) -> bool {
    acao.kind == ActionType::Invoke && acao.target.as_ref().is_some_and(|id| obs.elements.iter()
        .any(|e| &e.id == id && (e.role == "Edit" || e.actions.contains(&hcc_protocolo::ElementAction::SetValue))))
}

/// A recorded step goes through the same confirmation rule as a fresh pick: another goal may have authorized it.
fn confirmacao_barrada(acao: &Action, obs: &Observation, objetivo: &str, secretos: &Segredos) -> bool {
    Barreiras::sem_confirmar(vec![secretos.candidato(acao.clone(), obs)], obs, objetivo).0.is_empty()
}

fn com_dica(nome: &Option<String>, texto: String) -> String {
    match nome { Some(n) if texto.is_empty() => n.clone(), Some(n) => format!("{n}\n{texto}"), None => texto }
}

fn ocupada(janela: &str, vista: &Observation) -> String {
    let nome = vista.windows.iter().find(|w| w.id == janela).map_or(janela, |w| w.name.as_str());
    format!("{} '{nome}'", JANELA_OCUPADA)
}

const JANELA_OCUPADA: &str = "outro objetivo está controlando a janela";

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

/// The window read by id closed (a dialog the action dismissed).
fn fechou(e: &SessionError) -> bool { e.to_string().contains(recusa::JANELA_FECHOU) }

/// `acao-<ciclo>.png`: the cycle's print with the action's target box and click point; `None` without a print
/// or a place to mark. `janela`: the print is of that window only.
fn print_da_acao(pasta: &Path, ciclo: u32, acao: &Action, vista: &Observation, janela: Option<&str>, print: Option<&[u8]>) -> Option<PathBuf> {
    let caixa = acao.target.as_ref().and_then(|id| vista.elements.iter().find(|e| &e.id == id)).and_then(|e| e.rect);
    let ponto = match (acao.x, acao.y) { (Some(x), Some(y)) => Some((x, y)), _ => caixa.map(crate::candidatos::centro) };
    ponto?;
    let lida;
    let tela = match print { Some(p) => p, None => { lida = std::fs::read(pasta.join(format!("tela-{ciclo}.png"))).ok()?; &lida[..] } };
    let largura = crate::llm::dimensoes(tela)?.0 as f64;
    let (origem, base) = match janela.and_then(|j| vista.windows.iter().find(|w| w.id == j)) {
        Some(w) => ((w.rect.0, w.rect.1), f64::from(w.rect.2 - w.rect.0)),
        None => ((0, 0), f64::from(vista.screen.width)),
    };
    let marcado = crate::print::anotar(tela, caixa, ponto, origem, largura / base.max(1.0)).ok()?;
    let destino = pasta.join(format!("acao-{ciclo}.png"));
    std::fs::write(&destino, marcado).ok().map(|()| destino)
}

/// On a skeleton, a DRILL per named container with unread children; inside a region, WIDEN back to the skeleton.
fn regioes(obs: &Observation, leitura: &Scope) -> Vec<(String, Option<RootSpec>)> {
    if leitura.root.is_some() { return vec![("WIDEN back to the whole window".into(), None)]; }
    if !leitura.skeleton { return vec![]; }
    obs.elements.iter().enumerate()
        .filter(|(_, e)| e.unread.is_some_and(|n| n > 0) && !e.name.trim().is_empty())
        .map(|(i, e)| {
            let index = obs.elements[..i].iter().filter(|o| o.role == e.role && o.name == e.name).count() as u32;
            (format!("DRILL {} {} (read what is inside)", e.role, py_repr(&e.name)), Some(RootSpec { role: e.role.clone(), name: e.name.clone(), index }))
        })
        .collect()
}

async fn observar<S: Session>(sessao: &mut S, escopo: &Scope) -> Result<Observation, SessionError> {
    for tentativa in 0..3 {
        match sessao.observe(escopo).await {
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

fn agora() -> f64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    (t * 1000.0).round() / 1000.0
}

async fn registrar(pasta: &Path, mut valor: Value, secretos: &Segredos) -> Result<(), String> {
    secretos.valor(&mut valor);
    if let Value::Object(m) = &mut valor { m.insert("t".into(), json!(agora())); }
    let mut log = tokio::fs::OpenOptions::new().create(true).append(true).open(pasta.join("jev.jsonl")).await.map_err(erro_io)?;
    log.write_all(format!("{}\n", py_dumps(&valor)).as_bytes()).await.map_err(erro_io)?;
    log.flush().await.map_err(erro_io)
}

/// The screen as PNG, saved to `arquivo`. The outer error stops the run (cancel, time, disk); the inner one is the agent's.
/// With `janela`, only that window: the rest of the user's screen never reaches the vision model.
async fn capturar<S: Session>(sessao: &mut S, controle: &mut Controle<'_>, arquivo: &Path, janela: Option<&str>) -> Result<Result<Vec<u8>, SessionError>, String> {
    let png = controle.medir("captura", Progresso::Captura, async { Ok(sessao.screenshot(janela).await) }).await?;
    if let Ok(png) = &png { controle.rodar(async { tokio::fs::write(arquivo, png).await.map_err(erro_io) }).await?; }
    Ok(png)
}

/// `print` is the cycle's capture; without one (it failed) the screen is captured here, and a failure stops the run.
/// With `imagem`, Jev picks among the helper's proposals seeing it (the third value); the click points stay the
/// helper's.
#[allow(clippy::too_many_arguments)]
async fn ver<S: Session>(op: &Opcoes, sessao: &mut S, controle: &mut Controle<'_>, r: &mut Resultado, ciclo: u32,
    obs: &Observation, estado: &Value, secretos: &Segredos, print: Option<&[u8]>, jev: &Jev, imagem: Option<&Imagem>,
    janela: Option<&str>) -> Result<(Ajuda, bool, Option<(Action, f64)>), String> {
    let captura = r.registro.as_ref().ok_or("RuntimeError: registro indisponível")?.join(format!("tela-{ciclo}.png"));
    let capturada;
    let png = match print {
        Some(png) => png,
        None => { capturada = capturar(sessao, controle, &captura, janela).await?.map_err(erro_sessao)?; &capturada[..] }
    };
    r.captura = Some(captura);
    let limite = controle.restante()?.min(Duration::from_secs(45));
    let (mut ajuda, leitura_valida) = controle.medir("llm", Progresso::Llm, async {
        match op.llm.pedir(estado, png, limite).await {
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
    if let Some(pasta) = r.registro.clone() {
        let bruto = json!({"llm": ajuda.acoes, "interpretacao": ajuda.interpretacao, "impedimento": ajuda.impedimento});
        controle.rodar(registrar(&pasta, bruto, secretos)).await?;
    }
    let mut escolha = None;
    if let (Some(imagem), Some(pasta), false) = (imagem, r.registro.clone(), ajuda.acoes.is_empty()) {
        let propostas: Vec<String> = ajuda.acoes.iter().map(|a| secretos.descricao(a, obs, true)).collect();
        let limite = controle.restante()?.min(Duration::from_secs(30));
        let (envio, achado) = controle.medir("jev", Progresso::Jev, async { Ok(jev.escolher_proposta(estado, imagem, &propostas, limite).await) }).await?;
        let (envio, motivo) = envio_registro(&envio, secretos);
        let mut linha = json!({"ciclo": ciclo, "imagem": envio, "motivo": motivo});
        if let Some((i, p)) = achado {
            escolha = i.map(|i| (i, p));
            linha["escolha"] = json!(i.map(|i| &propostas[i]));
            linha["probability"] = json!(p);
        }
        controle.rodar(registrar(&pasta, linha, secretos)).await?;
    }
    let total = ajuda.acoes.len();
    // A print of one window: its pixels map to that window's place on the screen.
    let retangulo = janela.and_then(|j| obs.windows.iter().find(|w| w.id == j)).map(|w| w.rect);
    let largura = crate::llm::dimensoes(png).map_or(0, |(l, _)| u32::try_from(l).unwrap_or(0));
    let convertidas: Vec<Option<Action>> = ajuda.acoes.iter().map(|a| secretos.desmascarar(match retangulo {
        Some(j) => para_janela(a, j, largura, &op.texto),
        None => para_tela(a, &obs.screen, &op.texto),
    })).collect();
    let escolhida = escolha.and_then(|(i, p)| Some((convertidas.get(i)?.clone()?, p)));
    ajuda.acoes = convertidas.into_iter().flatten().collect();
    if ajuda.acoes.len() < total {
        ajuda.interpretacao.push_str(&format!(" [{} ação(ões) com <senha> descartada(s): não há um único segredo nos dados]", total - ajuda.acoes.len()));
    }
    Ok((ajuda, leitura_valida, escolhida))
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

/// The screen counts as changed by the action when the change holds: seen twice in a row, or seen once after
/// `ASSENTAR` (a menu or popover still filling up shows a half screen in the first few hundred ms).
fn efeito_visto(mudou: bool, igual_a_anterior: bool, desde_acao: Duration) -> bool {
    mudou && (igual_a_anterior || desde_acao >= ASSENTAR)
}

const PRIMEIRA_OLHADA: Duration = Duration::from_millis(200);
const ASSENTAR: Duration = Duration::from_millis(800);

#[allow(clippy::too_many_arguments)]
async fn esperar_efeito<C: Connector>(conector: &C, sessao: &mut Option<C::S>, controle: &mut Controle<'_>,
    acao: &Action, vista: &Observation, atual: &AssinaturaTela, secretos: &Segredos, escopo: &Scope) -> Result<bool, String> {
    let desde = Instant::now();
    // Blind (empty tree): only the title could change; no polling, the next observation looks.
    let normal = acao.kind != ActionType::Launch && !vista.elements.is_empty();
    let prazo = desde + if acao.kind == ActionType::Launch { Duration::from_secs(45) }
        else if vista.elements.is_empty() { Duration::from_millis(800) } else { Duration::from_secs(3) };
    controle.esperar(if normal { PRIMEIRA_OLHADA } else { ASSENTAR }).await?;
    let mut anterior: Option<AssinaturaTela> = None;
    while Instant::now() < prazo {
        let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
        let olhada = Instant::now();
        match controle.rodar(async { Ok(observar(s, escopo).await) }).await? {
            Ok(nova) => {
                let a = assinatura_tela(&nova);
                let mudou = a != *atual;
                if !normal && mudou || efeito_visto(mudou, anterior.as_ref() == Some(&a), desde.elapsed()) { return Ok(true); }
                anterior = mudou.then_some(a);
            }
            Err(e) if reconectavel(&e) => reconectar(conector, sessao, controle).await?,
            Err(e) if e.tipo() == "RuntimeError" => eprintln!("observação durante espera ignorada: {}", secretos.texto(&e.to_string())),
            Err(e) => return Err(erro_sessao(e)),
        }
        // A slow app (Delphi reads take seconds) is not read back to back.
        controle.esperar(if normal { PRIMEIRA_OLHADA.max(olhada.elapsed()) } else { Duration::from_secs(1) }).await?;
    }
    // Changed but still moving at the deadline: it did change, and a redo would undo a toggle.
    Ok(anterior.is_some())
}

/// The pieces of a run that went right, under `trechos/` of the scripts folder: each cut is a new piece file or
/// one more use of the same way. A piece with a click by coordinate cannot be replayed with a check, so it stays out.
fn gravar_trechos(op: &Opcoes, secretos: &Segredos, passos: &[roteiro::Passo], formas: &[Vec<String>], fim: Option<&Observation>) -> String {
    let Some(dir) = op.roteiros.as_deref() else { return String::new(); };
    let pasta = dir.join("trechos");
    let valores = valores_do_objetivo(&op.texto, &op.dados, &secretos.valores);
    let (mut gravados, mut falhas) = (0, Vec::new());
    let classe_final = fim.and_then(|o| o.windows.iter().find(|w| w.id == o.foreground)).map(|w| w.class_name.as_str());
    for r in trecho::cortar(passos, &valores, classe_final) {
        if r.passos.is_empty() || r.passos.iter().any(|p| p.acao.kind == ActionType::Mouse && p.alvo.is_none()) { continue; }
        // A browser's window class is the same on every site: a piece typing a secret could be offered on another.
        if r.passos.iter().any(|p| matches!(&p.valor, Some(roteiro::Valor::Dados(k)) if crate::candidatos::chave_secreta(k))) {
            falhas.push("trecho que digita segredo fica de fora".to_owned()); continue;
        }
        let descricao = trecho::resumo(&r.passos);
        let arquivo = trecho::arquivo(&pasta, &descricao, &r.contexto);
        // An existing file that does not read as this version may be newer or damaged: it is reported, never replaced.
        let anterior = match std::fs::read(&arquivo) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => { falhas.push(format!("{}: {e}", arquivo.display())); continue; }
            Ok(b) => match serde_json::from_slice::<trecho::Trecho>(&b) {
                Ok(t) if t.versao == trecho::VERSAO_TRECHO => Some(t),
                _ => { falhas.push(format!("{} ilegível ou de outra versão, mantido", arquivo.display())); continue; }
            },
        };
        let novo = trecho::Trecho { versao: trecho::VERSAO_TRECHO, tipo: "trecho".into(), descricao, parametros: r.parametros,
            contexto: r.contexto, forma: formas.get(r.inicio).cloned().unwrap_or_default(), variantes: vec![trecho::VarianteTrecho { passos: r.passos, usos: 0 }] };
        let junto = trecho::juntar(anterior, novo);
        if serde_json::to_value(&junto).map_or(true, |v| secretos.tem_segredo(&v)) { falhas.push("contém um valor secreto".to_owned()); continue; }
        match trecho::salvar(&arquivo, &junto) { Ok(()) => gravados += 1, Err(e) => falhas.push(e.to_string()) }
    }
    if gravados == 0 && falhas.is_empty() { return String::new(); }
    let mut texto = format!("trechos gravados: {gravados} em {}", pasta.display());
    if !falhas.is_empty() { texto.push_str(&format!(" (não gravados: {})", falhas.join(", "))); }
    texto
}

const TRECHO: &str = "Pick the recorded step sequence that is the right next part of `goal` on the CURRENT screen, \
with the values shown in it being the ones `goal` means (the person, item or term `goal` names). NONE when no option \
is a step toward `goal`, when its values are not the ones `goal` means, or when `goal` is already done.";

const PARECIDO: &str = "Pick the recorded goal that asks for exactly the same task as `goal`: same application, same \
target (person, record, screen) and same outcome; values the user supplies in `data` may differ. NONE when no recorded \
goal is that task, or when a goal only looks alike.";

/// A run that stopped (or failed) keeps the steps that went right, as a partial variant and as pieces: next time
/// they are replayed and Jev goes on from there. No judge decides: a step is kept for its effect (trecho::bons).
async fn ciclo<C: Connector>(op: &Opcoes, conector: &C, sessao: &mut Option<C::S>, controle: &mut Controle<'_>,
    r: &mut Resultado, secretos: &Segredos) -> Result<(), String> {
    let mut gravados = Vec::new();
    let mut recentes = Vec::new();
    let mut barrado = None;
    let res = ciclo_passos(op, conector, sessao, controle, r, secretos, &mut gravados, &mut recentes, &mut barrado).await;
    // A stop while the fixed rule held an option back: the caller must know the rule, not only Jev, decided.
    if !r.ok && let Some(b) = barrado { r.motivo = format!("{}; barrado pela regra de confirmação: {b}", r.motivo); }
    // A field left with another value is the likely cause of a stop: the caller must see it.
    if !r.ok && let Some(falha) = recentes.iter().rev().find(|x| x.action.starts_with("set_value ") || x.action.starts_with("text "))
        .and_then(|x| x.result.strip_prefix("NOT confirmed: ")).filter(|f| !r.motivo.contains(*f)) {
        r.motivo = format!("{}; {falha}", r.motivo);
    }
    // Each action's outcome, beside the `resultado` line `executar` writes last.
    if let Some(pasta) = r.registro.clone() {
        let acoes: Vec<Value> = recentes.iter().map(|x| json!({"acao": x.action, "resultado": x.result, "tela_mudou": x.screen_changed})).collect();
        if let Err(e) = registrar(&pasta, json!({"acoes": acoes}), secretos).await { eprintln!("ações não gravadas no registro: {e}"); }
    }
    let (Some(dir), Some(_), false) = (op.roteiros.as_deref(), r.registro.as_ref(), r.ok) else { return res; };
    let (feitos, formas) = feitos(&gravados, &recentes);
    // ponytail: the screen after the last step is unknown here, so an open-and-cancel pair at the very end stays.
    let bons = trecho::bons(&feitos, 0);
    let passos: Vec<roteiro::Passo> = bons.iter().map(|&i| feitos[i].passo.clone()).collect();
    let formas: Vec<Vec<String>> = bons.iter().map(|&i| formas[i].clone()).collect();
    if passos.is_empty() { return res; }
    let arquivo = roteiro::arquivo(dir, &op.texto, &op.dados);
    let n = passos.len();
    let pecas = gravar_trechos(op, secretos, &passos, &formas, None);
    let gravado = roteiro::anteriores(&arquivo).map(|a| roteiro::juntar(a, &op.texto, passos, true)).and_then(|novo| {
        if serde_json::to_value(&novo).map_or(true, |v| secretos.tem_segredo(&v)) { Err("contém um valor secreto".to_owned()) }
        else { roteiro::salvar(&arquivo, &novo).map_err(|e| e.to_string()) }
    });
    let texto = gravado.map_or_else(|e| format!("parcial não gravado: {e}"),
        |()| format!("parcial gravado: {n} passo(s) em {}", arquivo.display()));
    let texto = if pecas.is_empty() { texto } else { format!("{texto}; {pecas}") };
    r.roteiro = Some(r.roteiro.take().map_or_else(|| texto.clone(), |antes| format!("{antes}; {texto}")));
    res
}

/// Each recorded step with how it went: whether it changed the screen (the check its replay relies on) and ran ok.
/// With the shape of the screen each one started on.
fn feitos(gravados: &[Gravado], recentes: &[Registro]) -> (Vec<trecho::Feito>, Vec<Vec<String>>) {
    gravados.iter().filter_map(|(p, i, tela, forma)| i.and_then(|i| recentes.get(i)).map(|reg| {
        let mut passo = p.clone();
        passo.mudou_tela = reg.screen_changed;
        (trecho::Feito { passo, ok: reg.executada && reg.result.starts_with("ok"), tela: *tela }, forma.clone())
    })).unzip()
}

/// A step as recorded: the step, its entry in the run's registry, the screen before it and that screen's shape.
type Gravado = (roteiro::Passo, Option<usize>, u64, Vec<String>);

fn hash_tela(tela: &AssinaturaTela) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tela.hash(&mut h);
    h.finish()
}

/// Steps whose effect is now known go to `<roteiros>/diario/diario.jsonl` at once: a crash or a kill loses none.
fn anotar_diario(op: &Opcoes, r: &mut Resultado, secretos: &Segredos, gravados: &[Gravado],
    recentes: &[Registro], ate: &mut usize) {
    let (Some(dir), Some(pasta)) = (op.roteiros.as_deref(), r.registro.as_deref()) else { return; };
    let execucao = pasta.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut linhas = String::new();
    while let Some((p, i, _, _)) = gravados.get(*ate) {
        let reg = i.and_then(|i| recentes.get(i));
        if reg.is_some_and(|reg| reg.screen_changed.is_none()) { break; }
        *ate += 1;
        let Some(reg) = reg.filter(|reg| reg.screen_changed == Some(true) && reg.executada && reg.result.starts_with("ok")) else { continue };
        let mut passo = p.clone();
        passo.mudou_tela = reg.screen_changed;
        let linha = json!({"execucao": execucao, "objetivo": secretos.texto(&op.texto), "passo": passo});
        if secretos.tem_segredo(&linha) { continue; }
        linhas.push_str(&format!("{linha}\n"));
    }
    if linhas.is_empty() { return; }
    let arquivo = dir.join("diario").join("diario.jsonl");
    let escrito = std::fs::create_dir_all(dir.join("diario")).and_then(|()| std::fs::OpenOptions::new().append(true).create(true)
        .open(&arquivo)).and_then(|mut f| std::io::Write::write_all(&mut f, linhas.as_bytes()));
    // The run goes on, but the caller sees that what it learned was not kept.
    if let Err(e) = escrito { anotar_roteiro(r, format!("diário de passos não gravado em {}: {e}", arquivo.display())); }
}

fn anotar_roteiro(r: &mut Resultado, texto: String) {
    r.roteiro = Some(r.roteiro.take().map_or_else(|| texto.clone(), |antes| format!("{antes}; {texto}")));
}

/// Pieces recorded by any goal that fit `vista`: Jev picks one or none, its slots filled with the goal's values; each
/// step is checked, and the first divergence leaves the screen to Jev. `Ok(true)` when any step ran.
#[allow(clippy::too_many_arguments)]
async fn repetir_trecho<C: Connector>(op: &Opcoes, conector: &C, sessao: &mut Option<C::S>, controle: &mut Controle<'_>,
    r: &mut Resultado, secretos: &Segredos, jev: &Jev, pasta: &Path, pecas: &[(PathBuf, trecho::Trecho)],
    feitas: &mut Vec<(usize, BTreeMap<String, String>)>, vista: Observation, recentes: &mut Vec<Registro>,
    gravados: &mut Vec<Gravado>, anterior: &mut Option<AssinaturaTela>, foco_escolhido: &mut bool, valores: &[String],
    escopo: &Scope, aprendido: &crate::modo::Aprendido) -> Result<bool, String> {
    // A piece already replayed with the same values would only repeat itself.
    let opcoes: Vec<trecho::Opcao> = trecho::opcoes(pecas, &vista, &op.dados, valores).into_iter()
        .filter(|o| !feitas.contains(&(o.trecho, o.params.clone()))).collect();
    if opcoes.is_empty() { return Ok(false); }
    let textos: Vec<String> = opcoes.iter().map(|o| secretos.texto(&o.texto)).collect();
    let estado = secretos.estado(op, &vista, recentes, None);
    let limite = controle.restante()?.min(Duration::from_secs(30));
    let (i, p) = match controle.medir("jev", Progresso::Jev, jev.escolher(&estado, TRECHO, &textos, None, limite)).await {
        Ok(v) => v,
        Err(e) => { anotar_roteiro(r, format!("escolha de trecho falhou: {}", secretos.texto(&e))); return Ok(false); }
    };
    controle.rodar(registrar(pasta, json!({"trecho": i.map(|i| &textos[i]), "probability": p}), secretos)).await?;
    let Some(i) = i.filter(|_| p >= 0.8) else { return Ok(false); };
    let opcao = &opcoes[i];
    feitas.push((opcao.trecho, opcao.params.clone()));
    let passos = &pecas[opcao.trecho].1.variantes[opcao.variante].passos;
    let mut vista = vista;
    let (mut rodou, mut parou) = (false, None);
    for (n, passo) in passos.iter().enumerate() {
        if n > 0 {
            let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
            vista = match controle.medir("observacao", Progresso::Observacao, async { Ok(observar(s, escopo).await) }).await? {
                Ok(v) => v,
                Err(e) if fechou(&e) => { parou = Some(format!("trecho {} parou no passo {}: a janela fechou", textos[i], n + 1)); break; }
                Err(e) => return Err(erro_sessao(e)),
            };
            let agora = assinatura_tela(&vista);
            if let (Some(ultima), Some(a)) = (recentes.last_mut(), anterior.as_ref()) { ultima.screen_changed = mudou(a, &agora); }
            *anterior = Some(agora);
            if passos[n - 1].mudou_tela == Some(true) && recentes.last().is_some_and(|u| u.screen_changed == Some(false)) {
                parou = Some(format!("trecho {} parou no passo {}: a tela não mudou", textos[i], n + 1)); break;
            }
        }
        let atual = assinatura_tela(&vista);
        let Some(acao) = roteiro::resolver_com(passo, &vista, &op.dados, &opcao.params)
            .map(|a| Action { input: modo_entrada(&a, &vista, aprendido, op.ver), cursor: op.ver, ..a }) else {
            parou = Some(format!("trecho {} parou no passo {}: controle diferente do gravado", textos[i], n + 1)); break;
        };
        if confirmacao_barrada(&acao, &vista, &op.texto, secretos) {
            parou = Some(format!("trecho {} parou no passo {}: confirmação ou ação destrutiva que o objetivo não pede", textos[i], n + 1)); break;
        }
        let nome = secretos.descricao(&acao, &vista, true);
        let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
        let retorno = controle.agir(s, &vista, &acao, Progresso::Acao(secretos.descricao(&acao, &vista, false), None), escopo.window.as_deref()).await?;
        rodou = true;
        let ok = matches!(&retorno, Ok(ret) if ret.ok);
        let resultado = match &retorno { Ok(ret) if ret.ok => "ok".to_owned(), Ok(_) => "{'ok': False}".to_owned(),
            Err(e) => secretos.texto(&format!("NOT executed: {e}")) };
        recentes.push(Registro { action: nome.clone(), result: resultado.clone(), screen_changed: None, target_rect: None,
            clique: None, executada: retorno.is_ok() });
        r.passos.push(format!("{}. {nome} [trecho]", r.passos.len() + 1));
        if let Err(e) = &retorno && reconectavel(e) && !controle.cancel.is_cancelled() { reconectar(conector, sessao, controle).await?; }
        if !ok { parou = Some(format!("trecho {} parou no passo {}: {resultado}", textos[i], n + 1)); break; }
        *foco_escolhido |= escolhe_foco(&acao) || invoca_editavel(&acao, &vista);
        gravados.push((roteiro::passo(&acao, &vista, &op.dados), Some(recentes.len() - 1), hash_tela(&atual), trecho::forma(&vista, valores)));
        esperar_efeito(conector, sessao, controle, &acao, &vista, &atual, secretos, escopo).await?;
    }
    anotar_roteiro(r, parou.unwrap_or_else(|| format!("trecho repetido: {}", textos[i])));
    Ok(rodou)
}

#[allow(clippy::too_many_arguments)]
async fn ciclo_passos<C: Connector>(op: &Opcoes, conector: &C, sessao: &mut Option<C::S>, controle: &mut Controle<'_>,
    r: &mut Resultado, secretos: &Segredos, gravados: &mut Vec<Gravado>, recentes: &mut Vec<Registro>,
    barrado: &mut Option<String>) -> Result<(), String> {
    let jev = validar(op)?;
    let pasta = tempfile::Builder::new().prefix("hcu-").tempdir().map_err(erro_io)?.keep();
    r.registro = Some(pasta.clone());
    *sessao = Some(controle.medir("conexao", Progresso::Conexao,
        async { conector.connect(controle.cancel).await.map_err(erro_sessao) }).await?);
    let mut anterior = None;
    let mut barreiras = Barreiras::default();
    let mut recusas = HashSet::new();
    let mut conferir: Option<String> = None;
    // Windows of the run's first screen: one that appears later may be the app the goal opened.
    let mut iniciais: Option<HashSet<String>> = None;
    let mut foco_escolhido = false;
    let mut em_foco_nas_acoes: Vec<hcc_protocolo::Window> = Vec::new();
    let mut fechados: Vec<String> = Vec::new();
    let mut recusou_sem_acao = false;
    // The last typing into a named field (registry index, role, name, value): checked on the next screen.
    let mut digitado: Option<(usize, String, String, String)> = None;
    let mut diario_ate = 0usize;
    let valores_meta = valores_do_objetivo(&op.texto, &op.dados, &secretos.valores);
    // No step budget: the run stops when it keeps coming back to screens it has already seen.
    let mut vistas: HashSet<AssinaturaTela> = HashSet::new();
    let mut sem_tela_nova = 0u32;
    controle.rodar(registrar(&pasta, json!({"objetivo": op.texto, "dados": op.dados.keys().collect::<Vec<_>>()}), secretos)).await?;
    let arquivo = op.roteiros.as_deref().map(|p| roteiro::arquivo(p, &op.texto, &op.dados));
    let mut lido = arquivo.as_deref().filter(|_| !op.refazer).map(roteiro::carregar).transpose().map(Option::flatten);
    let mut origem = String::new();
    // The same request said in other words: Jev matches it to a recorded goal, or to none.
    if let (Ok(None), Some(arq), Some(dir), false, false) = (&lido, arquivo.as_deref(), op.roteiros.as_deref(), op.refazer, so_leitura(&op.texto)) {
        let achados = roteiro::parecidos(dir, &op.dados, arq);
        if !achados.is_empty() {
            let opcoes: Vec<String> = achados.iter().map(|(_, r)| secretos.texto(&r.objetivo)).collect();
            let estado = json!({"goal": secretos.texto(&op.texto), "data": op.dados.keys().collect::<Vec<_>>()});
            let limite = controle.restante()?.min(Duration::from_secs(30));
            match controle.medir("jev", Progresso::Jev, jev.escolher(&estado, PARECIDO, &opcoes, None, limite)).await {
                Ok((i, p)) => {
                    controle.rodar(registrar(&pasta, json!({"parecido": i.map(|i| &opcoes[i]), "probability": p}), secretos)).await?;
                    if let Some(i) = i.filter(|_| p >= 0.8) {
                        origem = format!("parecido com {}, ", py_repr(&opcoes[i]));
                        lido = Ok(achados.into_iter().nth(i).map(|(_, r)| r));
                    }
                }
                Err(e) => r.roteiro = Some(format!("busca de roteiro parecido falhou, seguindo pelo Jev: {}", secretos.texto(&e))),
            }
        }
    }
    // Replay: each step resolved on the current screen, no Jev; the first divergence hands over to the loop,
    // whose first cycle also asks Jev whether the goal is done.
    let sem_roteiro = matches!(lido, Ok(None));
    match lido {
        Err(e) => r.roteiro = Some(format!("ilegível, seguindo pelo Jev: {}", secretos.texto(&e))),
        Ok(None) => {}
        Ok(Some(gravado)) => {
            let mut parou = None;
            // A skipped step acted on nothing: the screen read after it says nothing about the last action.
            let mut pulou = false;
            let mut passos: &[roteiro::Passo] = &[];
            let mut variante = String::new();
            for n in 0.. {
                if n > 0 && n == passos.len() { break; }
                let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
                let vista = controle.medir("observacao", Progresso::Observacao,
                    async { observar(s, &Scope::default()).await.map_err(erro_sessao) }).await?;
                let atual = assinatura_tela(&vista);
                iniciais.get_or_insert_with(|| vista.windows.iter().map(|w| w.id.clone()).collect());
                if !pulou && let (Some(ultima), Some(a)) = (recentes.last_mut(), anterior.as_ref()) { ultima.screen_changed = mudou(a, &atual); }
                anterior = Some(atual.clone());
                if n == 0 {
                    let Some((i, escolhidos)) = roteiro::escolher(&gravado, &vista, &op.dados) else {
                        parou = Some(format!("nenhuma variação gravada ({}) começa nesta tela", gravado.variantes.len())); break;
                    };
                    (passos, variante) = (escolhidos, format!("variação {} de {}, ", i + 1, gravado.variantes.len()));
                }
                let passo = &passos[n];
                // Kept as recorded: the next run may find the app closed.
                if roteiro::ja_aberto(passos, n, &vista) { gravados.push((passo.clone(), None, hash_tela(&atual), vec![])); pulou = true; continue; }
                let motivo = |m: &str| Some(format!("parou no passo {}: {m}", n + 1));
                if !std::mem::take(&mut pulou) && n > 0 && passos[n - 1].mudou_tela == Some(true)
                    && recentes.last().is_some_and(|u| u.screen_changed == Some(false)) {
                    parou = motivo("a tela não mudou depois do passo anterior"); break;
                }
                let Some(acao) = roteiro::resolver(passo, &vista, &op.dados) else {
                    parou = motivo("janela da frente ou controle diferente do gravado"); break;
                };
                if confirmacao_barrada(&acao, &vista, &op.texto, secretos) {
                    parou = motivo("confirmação ou ação destrutiva que o objetivo não pede"); break;
                }
                let nome = secretos.descricao(&acao, &vista, true);
                let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
                let retorno = controle.agir(s, &vista, &acao, Progresso::Acao(secretos.descricao(&acao, &vista, false), None), None).await?;
                let resultado = match &retorno {
                    Ok(ret) if ret.ok => "ok".to_owned(),
                    Ok(_) => "{'ok': False}".to_owned(),
                    Err(e) => secretos.texto(&format!("NOT executed: {e}")),
                };
                recentes.push(Registro { action: nome.clone(), result: resultado.clone(),
                    screen_changed: None, target_rect: None, clique: None, executada: retorno.is_ok() });
                r.passos.push(format!("{}. {nome} [roteiro]", r.passos.len() + 1));
                match retorno {
                    Err(e) => {
                        parou = motivo(&resultado);
                        if reconectavel(&e) && !controle.cancel.is_cancelled() { reconectar(conector, sessao, controle).await?; }
                        break;
                    }
                    Ok(ret) if !ret.ok => { parou = motivo("o agente recusou a ação"); break; }
                    Ok(_) => {}
                }
                foco_escolhido |= escolhe_foco(&acao) || invoca_editavel(&acao, &vista);
                gravados.push((roteiro::passo(&acao, &vista, &op.dados), Some(recentes.len() - 1), hash_tela(&atual), trecho::forma(&vista, &valores_meta)));
                esperar_efeito(conector, sessao, controle, &acao, &vista, &atual, secretos, &Scope::default()).await?;
            }
            r.roteiro = Some(if passos.is_empty() { format!("{origem}{}", parou.unwrap_or_default()) } else {
                let pulados = gravados.iter().filter(|(_, i, _, _)| i.is_none()).count();
                let mut texto = format!("{origem}{variante}repetidos {} de {} passos", gravados.len() - pulados, passos.len());
                if pulados > 0 { texto.push_str(&format!(" ({pulados} pulado(s): app já aberto)")); }
                if let Some(p) = parou { texto.push_str(&format!("; {p}")); }
                texto
            });
        }
    }
    // Pieces recorded by any goal, offered where a sub-task starts: the first screen (when no script of this goal ran)
    // and each time a dialog opens or closes or another window comes to the front. A reading goal never acts.
    let pecas = match op.roteiros.as_deref() {
        Some(d) if !op.refazer && !so_leitura(&op.texto) => trecho::todos(&d.join("trechos")), _ => vec![] };
    let mut feitas: Vec<(usize, BTreeMap<String, String>)> = Vec::new();
    let mut escopo: Option<(String, Option<String>)> = None;
    // A window whose whole read was cut is read as a skeleton; `regiao` is where Jev drilled into.
    let mut esqueleto: Option<String> = None;
    let mut regiao: Option<RootSpec> = None;
    // The app's window, read (and shot) by id so it may stay behind the user's; while watching, the front one.
    let mut janela_app: Option<String> = None;
    let mut janelas_antes: HashSet<String> = HashSet::new();
    // The app's earlier windows, to go back to when a dialog of it closes.
    let mut janelas_do_app: Vec<String> = Vec::new();
    // Cycles left to take a new window as the launched app's, and that app's program name.
    let mut abriu = 0u32;
    let mut lancado: Option<String> = None;
    let mut aprendido = crate::modo::Aprendido::ler(op.roteiros.as_deref());
    for ciclo in 0..=op.max_passos {
        let mut base = Scope { window: janela_app.clone().filter(|_| !op.ver), ..Scope::default() };
        let mut lido_como = Scope { root: regiao.clone(), skeleton: esqueleto.is_some() && regiao.is_none(), ..base.clone() };
        let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
        let mut obs = match controle.medir("observacao", Progresso::Observacao, async { Ok(observar(s, &lido_como).await) }).await? {
            Ok(o) => o,
            // The app's window closed (a dialog was dismissed): back to the app's previous window, and only with none
            // left to whatever is in front.
            Err(e) if lido_como.window.is_some() && fechou(&e) => {
                (esqueleto, regiao) = (None, None);
                let mut lida = None;
                while let Some(antes) = janelas_do_app.pop() {
                    let escopo = Scope { window: Some(antes.clone()), ..Scope::default() };
                    let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
                    match controle.medir("observacao", Progresso::Observacao, async { Ok(observar(s, &escopo).await) }).await? {
                        Ok(o) => { lida = Some((antes, escopo, o)); break; }
                        Err(e) if fechou(&e) => {}
                        Err(e) => return Err(erro_sessao(e)),
                    }
                }
                match lida {
                    Some((antes, escopo, o)) => { janela_app = Some(antes); (base, lido_como) = (escopo.clone(), escopo); o }
                    None => {
                        janela_app = None;
                        (base, lido_como) = (Scope::default(), Scope::default());
                        let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
                        controle.medir("observacao", Progresso::Observacao, async { observar(s, &lido_como).await.map_err(erro_sessao) }).await?
                    }
                }
            }
            // The drilled region closed or re-rendered: back to the skeleton.
            Err(e) if lido_como.root.is_some() && e.to_string().contains(recusa::REGIAO_SUMIU) => {
                regiao = None;
                lido_como = Scope { skeleton: true, ..base.clone() };
                let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
                controle.medir("observacao", Progresso::Observacao, async { observar(s, &lido_como).await.map_err(erro_sessao) }).await?
            }
            Err(e) => return Err(erro_sessao(e)),
        };
        // A new window of the app (its dialog) becomes the window read.
        let pid_app = base.window.as_ref().and_then(|j| obs.windows.iter().find(|w| &w.id == j)).map(|w| w.process_id);
        let mut novas: Vec<&hcc_protocolo::Window> = obs.windows.iter().filter(|w| (abriu > 0 || Some(w.process_id) == pid_app)
            && Some(&w.id) != base.window.as_ref() && !janelas_antes.is_empty() && !janelas_antes.contains(&w.id)).collect();
        // Another goal launching at the same time shows its window too: the launched program's comes first, and a
        // window another goal already took is not this one's.
        if let Some(app) = lancado.as_deref() { novas.sort_by_key(|w| !do_programa(w, app)); }
        let mut adotada = None;
        // Another goal checking the window in front holds its lock for an instant: a few tries, not one.
        'adotar: for _ in 0..3 {
            for w in &novas {
                if controle.tomar(&w.id)? { adotada = Some(w.id.clone()); break 'adotar; }
            }
            controle.esperar(Duration::from_millis(20)).await?;
        }
        if adotada.is_none() && !novas.is_empty() {
            eprintln!("janela nova não adotada: outro objetivo já trabalha em {:?}", novas.iter().map(|w| w.name.as_str()).collect::<Vec<_>>());
        }
        if let Some(nova) = adotada {
            lancado = None;
            janelas_do_app.extend(janela_app.take());
            janela_app = Some(nova);
            abriu = 0;
            (esqueleto, regiao) = (None, None);
            base = Scope { window: janela_app.clone(), ..Scope::default() };
            lido_como = base.clone();
            let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
            obs = controle.medir("observacao", Progresso::Observacao, async { observar(s, &lido_como).await.map_err(erro_sessao) }).await?;
        }
        janelas_antes = obs.windows.iter().map(|w| w.id.clone()).collect();
        abriu = abriu.saturating_sub(1);
        // Another window or a dialog: the partial read no longer fits, read it whole.
        let completa = lido_como == base
            // Keyed by the window itself, not its title: Explorer and browsers retitle it on every navigation.
            || Some(&obs.foreground) != esqueleto.as_ref() || crate::estado::dialogo_aberto(&obs).is_some();
        if completa {
            if lido_como != base {
                (esqueleto, regiao) = (None, None);
                let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
                obs = controle.medir("observacao", Progresso::Observacao,
                    async { observar(s, &base).await.map_err(erro_sessao) }).await?;
            }
            if obs.truncated && crate::estado::dialogo_aberto(&obs).is_none() { esqueleto = Some(obs.foreground.clone()); }
        }
        let lido_como = if completa { base.clone() } else { lido_como };
        let atual = assinatura_tela(&obs);
        sem_tela_nova = if vistas.insert(atual.clone()) { 0 } else { sem_tela_nova + 1 };
        iniciais.get_or_insert_with(|| obs.windows.iter().map(|w| w.id.clone()).collect());
        let conferir = &*conferir.get_or_insert_with(|| sem_localizadores(&op.texto, &obs));
        if let Some(ultima) = recentes.last_mut().filter(|r| r.screen_changed.is_none()) {
            ultima.screen_changed = anterior.as_ref().map_or(Some(true), |a| mudou(a, &atual));
        }
        if let Some(u) = r.ultima.as_mut().filter(|u| u.entrega.is_some() && u.tela_mudou.is_none()) {
            u.tela_mudou = recentes.last().and_then(|x| x.screen_changed);
        }
        anterior = Some(atual.clone());
        anotar_diario(op, r, secretos, gravados, recentes, &mut diario_ate);
        let divergencia = digitado.take().and_then(|(i, papel, nome, valor)| {
            let lido = campo_diferente(&obs, &papel, &nome, &valor)?;
            let texto = secretos.texto(&format!("o campo {papel} {} ficou com {}, não com o valor pedido",
                py_repr(&nome), py_repr(&lido.chars().take(80).collect::<String>())));
            // Not progress: the same typing is not offered again, and the next pick goes through the vision model.
            if let Some(reg) = recentes.get_mut(i) { reg.result = format!("NOT confirmed: {texto}"); reg.screen_changed = Some(false); }
            Some(texto)
        });
        fechados.extend(apps_fechados(&std::mem::take(&mut em_foco_nas_acoes), &obs));
        let escopo_agora = (obs.foreground.clone(), crate::estado::dialogo_aberto(&obs).map(|(d, _)| d.name.clone()));
        let novo_escopo = escopo.replace(escopo_agora.clone()) != Some(escopo_agora);
        if novo_escopo && (ciclo > 0 || sem_roteiro) && divergencia.is_none() && !pecas.is_empty()
            && repetir_trecho(op, conector, sessao, controle, r, secretos, jev, &pasta, &pecas, &mut feitas, obs.clone(), recentes,
                gravados, &mut anterior, &mut foco_escolhido, &valores_meta, &Scope { window: lido_como.window.clone(), ..Scope::default() }, &aprendido).await? {
            continue;
        }
        let mut gravada = serde_json::to_value(&obs).map_err(|e| format!("ValueError: {e}"))?;
        secretos.valor(&mut gravada);
        controle.rodar(async { tokio::fs::write(pasta.join("observacao.json"), py_dumps(&gravada)).await.map_err(erro_io) }).await?;
        // Every cycle's screen stays, so a run can be read back step by step.
        controle.rodar(async { tokio::fs::write(pasta.join(format!("observacao-{ciclo}.json")), py_dumps(&gravada)).await.map_err(erro_io) }).await?;
        // Jev decides every step seeing the screen; without a usable print it decides from the tree alone.
        let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
        let (print, imagem, sem_imagem) = match capturar(s, controle, &pasta.join(format!("tela-{ciclo}.png")), lido_como.window.as_deref()).await? {
            Err(e) => (None, None, Some(Envio { imagem: "sem_print", motivo: erro_sessao(e) })),
            Ok(png) => match Imagem::de_png(png.clone()) {
                Ok(i) => (Some(png), Some(i), None),
                Err(e) => (Some(png), None, Some(Envio { imagem: "fora_do_limite", motivo: e })),
            },
        };
        if barreiras.aviso_repetido(&obs).is_some() {
            let texto: String = secretos.texto(&aviso(&obs).unwrap_or_default()).chars().take(400).collect();
            r.motivo = format!("o aplicativo respondeu duas vezes com o mesmo aviso: {texto}"); return Ok(());
        }
        if so_leitura(&op.texto) {
            // Nothing to do on screen: the tree text and one print reading are the answer; no Jev, no action.
            let estado = secretos.estado(op, &obs, recentes, None);
            let (ajuda, leu, _) = ver(op, sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?, controle, r, ciclo, &obs, &estado, secretos,
                print.as_deref(), jev, None, lido_como.window.as_deref()).await?;
            let texto = texto_janela(&obs);
            if !leu && texto.is_empty() { r.motivo = format!("leitura sem resultado: {}", ajuda.interpretacao); return Ok(()); }
            r.ok = true;
            let janela = estado["window"].as_str().map_or_else(|| "None".into(), py_repr);
            r.motivo = format!("leitura da tela, sem ações, na janela {janela}");
            if leu { r.motivo.push_str(&format!(". Leitura: {}", ajuda.interpretacao)); }
            if !texto.is_empty() { r.motivo.push_str(&format!(". Texto na janela: {texto}")); }
            return Ok(());
        }
        let mut valores = op.dados.clone();
        for (i, l) in literais(&op.texto).into_iter().enumerate() { valores.insert(format!("objetivo_{i}"), Value::String(l)); }
        let cands = sem_reabrir(apps_do_objetivo(sem_app_aberto(candidatos(&obs, &valores, &secretos.valores), &obs, &op.texto), &op.texto), &fechados).into_iter().map(|mut c| {
            c.descricao = secretos.descricao(&c.acao, &obs, true); c
        }).collect();
        if sem_tela_nova >= SEM_PROGRESSO {
            let ultimas: Vec<&str> = recentes.iter().rev().take(3).map(|x| x.action.as_str()).collect();
            r.motivo = format!("sem progresso: {sem_tela_nova} ciclos seguidos sem tela nova; últimas ações: {}", ultimas.join("; "));
            return Ok(());
        }
        // Nothing changed three times: not a stop, the vision model is asked for another route.
        let divergencia = match (divergencia, Barreiras::tres_sem_efeito(recentes).map(|m| format!("{m}; take another route, never these actions again"))) {
            (Some(a), Some(b)) => Some(format!("{a}\n{b}")),
            (a, b) => a.or(b),
        };
        let antes = segurar_segredos(Barreiras::barrar(cands, recentes), &op.texto, recentes, &secretos.valores);
        let (acoes, confirmacao) = Barreiras::sem_confirmar(antes.clone(), &obs, &op.texto);
        let fora: Vec<&str> = antes.iter().filter(|c| !acoes.iter().any(|a| a.acao == c.acao)).map(|c| c.descricao.as_str()).collect();
        if !fora.is_empty() { *barrado = Some(fora.join("; ")); }
        let mut acoes: Vec<Candidato> = acoes.into_iter().filter(|c| !texto_sem_alvo_barrado(&c.acao, foco_escolhido)).collect();
        if let Some(mut clique) = Barreiras::reclique(recentes, &obs) {
            clique.descricao = secretos.texto(&clique.descricao);
            acoes.push(clique);
        }
        let parecido = nome_parecido(&op.texto, &obs);
        let nome = parecido.as_ref().map(|(dita, tela)| format!(
            "'{dita}' in the goal is likely a speech transcription of '{tela}', the only similar name on screen; treat it as '{tela}'."));
        let alerta = [divergencia.clone(), confirmacao].into_iter().flatten().collect::<Vec<_>>().join("\n");
        let (mut dica, mut ajudou, mut direta) = (com_dica(&nome, alerta), false, None);
        let mut leitura_valida = false;
        let mut leitura = String::new();
        let mut fila: Vec<Candidato> = Vec::new();
        let mut falta_dica = divergencia.clone();
        let mut primeira_ajuda: Option<Candidato> = None;
        if sem_controles(&obs) {
            let estado = secretos.estado(op, &obs, recentes, None);
            let (ajuda, leu, escolhida) = ver(op, sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?, controle, r, ciclo, &obs, &estado, secretos,
                print.as_deref(), jev, imagem.as_ref(), lido_como.window.as_deref()).await?;
            leitura_valida = leu;
            leitura = ajuda.interpretacao.clone();
            if !ajuda.impedimento.is_empty() { r.motivo = ajuda.impedimento; return Ok(()); }
            dica = com_dica(&nome, ajuda.interpretacao); ajudou = true;
            let escolhida = escolhida.and_then(|(a, p)| match &no_campo(alvo_observado(a, &obs), &obs, recentes)[..] {
                [a] => Some((a.clone(), p)), _ => None });
            let grupos: Vec<Vec<Action>> = ajuda.acoes.into_iter()
                .filter(|a| matches!(a.kind, ActionType::Mouse | ActionType::Keys | ActionType::Text))
                .map(|a| no_campo(alvo_observado(a, &obs), &obs, recentes)).collect();
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
            let propostas: Vec<Candidato> = Barreiras::sem_confirmar(segurar_segredos(Barreiras::barrar(propostas, recentes), &op.texto, recentes, &secretos.valores), &obs, &op.texto).0;
            let pode = |p: &Candidato| dentro(&p.acao, &obs) && !texto_sem_alvo_barrado(&p.acao, foco_escolhido);
            // Jev, seeing the print, picks among the proposals; unsure, the first one decides, as before.
            let pela_jev = escolhida.filter(|(_, p)| *p >= FRACO_COM_PRINT)
                .and_then(|(a, p)| propostas.iter().find(|c| c.acao == a && pode(c)).map(|c| (c.clone(), p)));
            // A proposal dropped by the guards breaks the run: the rest waits for the next cycle.
            let seguidas = propostas.iter().zip(&teclado).take_while(|(p, a)| p.acao == **a && dentro(&p.acao, &obs)).count();
            // The queue follows the first proposal: another pick goes alone.
            if seguidas >= 2 && pela_jev.as_ref().is_none_or(|(c, _)| propostas.first() == Some(c)) { fila = propostas[1..seguidas].to_vec(); }
            for proposta in propostas.iter().filter(|p| pode(p)) {
                if !acoes.iter().any(|a| a.acao == proposta.acao) { acoes.push(proposta.clone()); }
            }
            let proposta = pela_jev.or_else(|| propostas.first().filter(|p| !ambigua && pode(p)).map(|p| (p.clone(), 1.0)));
            if let Some((proposta, p)) = proposta {
                let limite = controle.restante()?.min(Duration::from_secs(30));
                let (risco, envio) = controle.medir("jev", Progresso::Jev, jev.arriscado(&estado, &proposta.descricao, imagem.as_ref(), limite)).await?;
                let (envio, motivo) = envio_registro(sem_imagem.as_ref().unwrap_or(&envio), secretos);
                controle.rodar(registrar(&pasta, json!({"ciclo":ciclo, "direta":proposta.descricao, "risky":risco, "dica":dica,
                    "imagem":envio, "motivo":motivo}), secretos)).await?;
                let i = acoes.iter().position(|a| a.acao == proposta.acao).ok_or("IndexError: list index out of range")?;
                direta = Some(Decisao { escolha: Escolha::Indice(i), p, risco, top: vec![], com_imagem: false,
                    envio: Envio::default(), blocked: None });
            }
        }
        // Reading choices, not actions: never sent to the app, never recorded in a script.
        let mut drill = HashMap::new();
        for (descricao, spec) in regioes(&obs, &lido_como) {
            drill.insert(acoes.len(), spec);
            acoes.push(Candidato { acao: Action { kind: ActionType::Focus, ..Default::default() }, intencao: intencao(&Action::default(), &obs),
                descricao: secretos.texto(&descricao) });
        }
        let mut decisao = loop {
            if let Some(d) = direta.take() { break d; }
            let estado = secretos.estado(op, &obs, recentes, Some(&dica));
            let limite = controle.restante()?.min(Duration::from_secs(30));
            let d = controle.medir("jev", Progresso::Jev, jev.decidir(&estado, &acoes, imagem.as_ref(), limite)).await?;
            let opcao = escolha(d.escolha);
            let (envio, motivo) = envio_registro(sem_imagem.as_ref().unwrap_or(&d.envio), secretos);
            // Python conta caracteres do JSON ASCII, apesar do nome bytes_estado.
            let ascii = py_dumps(&estado).chars().map(|c| if c.is_ascii() { 1 } else if u32::from(c) > 0xffff { 12 } else { 6 }).sum::<usize>();
            let mut log = json!({"ciclo":ciclo, "n_acoes":acoes.len(), "bytes_estado":ascii, "dica":dica,
                "opcoes":acoes.iter().map(|a| &a.descricao).collect::<Vec<_>>(),
                "choice":opcao, "probability":d.p, "risky":d.risco, "imagem":envio, "motivo":motivo,
                "blocked":d.blocked});
            if !d.top.is_empty() { log["top"] = json!(d.top); }
            controle.rodar(registrar(&pasta, log, secretos)).await?;
            controle.restante()?;
            let (fraco_min, done_min, truncada_min) = limites(&d);
            if d.escolha == Escolha::Done && d.p >= done_min {
                let mut confirmado = (!ajudou).then_some(d.p);
                let mut leitura_forte = false;
                if ajudou {
                    let sem_dica = secretos.estado(op, &obs, recentes, None);
                    let limite = controle.restante()?.min(Duration::from_secs(30));
                    let d2 = controle.medir("jev", Progresso::Jev, jev.decidir(&sem_dica, &acoes, imagem.as_ref(), limite)).await?;
                    let (envio2, motivo2) = envio_registro(sem_imagem.as_ref().unwrap_or(&d2.envio), secretos);
                    controle.rodar(registrar(&pasta, json!({"ciclo":ciclo, "sem_dica":true,
                        "choice":escolha(d2.escolha), "probability":d2.p, "risky":d2.risco, "imagem":envio2, "motivo":motivo2,
                        "blocked":d2.blocked}), secretos)).await?;
                    controle.restante()?;
                    // Without the hint Jev sees no reading at all: WAIT there is "cannot tell", not an objection.
                    // Below 95% the reading counts only where nothing else could: an empty tree, or a goal whose
                    // deliverable is the reading itself.
                    let so_a_leitura = d.p >= 0.95 || obs.elements.is_empty() || pede_leitura(&op.texto);
                    if leitura_valida && so_a_leitura && matches!(d2.escolha, Escolha::Done | Escolha::Wait) {
                        confirmado = Some(d.p);
                        leitura_forte = true;
                    } else if d2.escolha == Escolha::Done && d2.p >= limites(&d2).1 { confirmado = Some(d2.p); }
                    // The print reading plus every explicit item of the goal on screen is evidence enough.
                    else if ((itens_pendentes(conferir, &obs) > 0 && literais_atendidos(conferir, &obs, recentes)) || nomes_no_titulo(&op.texto, &obs)
                        || iniciais.as_ref().is_some_and(|i| nomes_em_janela_nova(&op.texto, &obs, i)))
                        && pendencia(conferir, &obs, recentes, &secretos.valores).is_none() {
                        confirmado = Some(d.p);
                    }
                }
                match confirmado {
                    Some(p) => {
                        // Nothing done yet while the control the goal names is still there: asked once more with that
                        // said; if Jev insists, it is reported as already done before any action.
                        if !recusou_sem_acao && let Some(falta) = sem_acao_pedida(&op.texto, &obs, recentes) {
                            recusou_sem_acao = true;
                            recentes.push(Registro { action: format!("DONE recusado: {falta}"), result: "não confirmado".into(),
                                screen_changed: Some(false), target_rect: None, clique: None, executada: false });
                            dica = if dica.is_empty() { falta } else { format!("{falta}\n{dica}") };
                            continue;
                        }
                        // Insisted with nothing done: the caller decides, a success here would be the false "concluído".
                        if let Some(falta) = sem_acao_pedida(&op.texto, &obs, recentes) {
                            r.motivo = format!("não confirmado: {falta}; o Jev diz que já estava feito ({})", percentual(p));
                            return Ok(());
                        }
                        if let Some((indice, falta)) = pendencia(conferir, &obs, recentes, &secretos.valores) {
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
                        if let Some(arquivo) = arquivo.as_deref().filter(|_| !gravados.is_empty()) {
                            let (passos, formas): (Vec<roteiro::Passo>, Vec<Vec<String>>) = gravados.drain(..).map(|(mut p, i, _, f)| { if let Some(i) = i { p.mudou_tela = recentes.get(i).and_then(|r| r.screen_changed); } (p, f) }).unzip();
                            let pecas = gravar_trechos(op, secretos, &passos, &formas, Some(&obs));
                            // `refazer` asked to forget the recorded ways, not to keep them beside the new one.
                            let anteriores = if op.refazer { Ok(None) } else { roteiro::anteriores(arquivo) };
                            let gravado = anteriores.map(|a| roteiro::juntar(a, &op.texto, passos, false)).and_then(|novo| {
                                // Any field may echo a secret (goal text, a title, a vision label): such a script stays off disk.
                                if serde_json::to_value(&novo).map_or(true, |v| secretos.tem_segredo(&v)) {
                                    Err("contém um valor secreto".to_owned())
                                } else { roteiro::salvar(arquivo, &novo).map_err(|e| e.to_string()) }
                            }).map_err(|e| format!("não gravado: {e}"));
                            let texto = gravado.map_or_else(|e| e, |()| format!("gravado em {}", arquivo.display()));
                            let texto = if pecas.is_empty() { texto } else { format!("{texto}; {pecas}") };
                            r.roteiro = Some(r.roteiro.take().map_or_else(|| texto.clone(), |antes| format!("{antes}; {texto}")));
                        }
                        let janela = estado["window"].as_str().map_or_else(|| "None".into(), py_repr);
                        r.motivo = if leitura_forte {
                            format!("objetivo confirmado pela leitura da tela ({}) na janela {janela}. Leitura: {leitura}", percentual(p))
                        } else { format!("objetivo confirmado ({}) na janela {janela}", percentual(p)) };
                        if let Some((dita, tela)) = &parecido {
                            r.motivo.push_str(&format!(". Nome '{dita}' do pedido tratado como '{tela}', o único parecido na tela"));
                        }
                        if pede_leitura(&op.texto) {
                            if !leitura_forte && !leitura.is_empty() { r.motivo.push_str(&format!(". Leitura: {leitura}")); }
                            let texto = texto_janela(&obs);
                            if !texto.is_empty() { r.motivo.push_str(&format!(". Texto na janela: {texto}")); }
                        }
                    }
                    None => r.motivo = format!("não confirmado: só a leitura do print indica que terminou. {dica}"),
                }
                return Ok(());
            }
            let esperando = recentes.iter().rev().filter(|r| !r.action.starts_with("DONE recusado: "))
                .take(3).filter(|r| r.action == "WAIT").count() >= 3;
            // A blocked screen calls the helper, which writes why or disagrees; once it has looked, its word stands.
            let bloqueado = d.blocked.is_some_and(|b| b >= LIMITE_BLOQUEIO);
            let fraco = d.escolha == Escolha::Blocked || d.p < if d.escolha == Escolha::Done { done_min } else { fraco_min }
                || (d.escolha == Escolha::Wait && esperando) || (bloqueado && !ajudou);
            // A truncated tree may hide the control the goal needs, but a sure pick was found in what came.
            // A skeleton is partial on purpose: Jev reaches the rest by DRILL, not by a stricter pick.
            if !fraco && (!obs.elements.is_empty() && (!obs.truncated || lido_como.skeleton || d.p >= truncada_min) && divergencia.is_none() || ajudou) { break d; }
            if ajudou {
                // Jev still unsure: the help's first action decides, after the risk question.
                if let Some(proposta) = primeira_ajuda.take() {
                    let estado = secretos.estado(op, &obs, recentes, Some(&dica));
                    let limite = controle.restante()?.min(Duration::from_secs(30));
                    let (risco, envio) = controle.medir("jev", Progresso::Jev, jev.arriscado(&estado, &proposta.descricao, imagem.as_ref(), limite)).await?;
                    let (envio, motivo) = envio_registro(sem_imagem.as_ref().unwrap_or(&envio), secretos);
                    controle.rodar(registrar(&pasta, json!({"ciclo":ciclo, "direta":proposta.descricao, "risky":risco, "dica":dica,
                        "imagem":envio, "motivo":motivo}), secretos)).await?;
                    let i = acoes.iter().position(|a| a.acao == proposta.acao).ok_or("IndexError: list index out of range")?;
                    // Jev's own confidence stays on record: the step came from the help, not from a sure vote.
                    break Decisao { escolha: Escolha::Indice(i), risco, top: vec![], ..d };
                }
                r.motivo = format!("sem ação segura: Jev escolheu {opcao} com {}. {dica}", percentual(d.p)).trim().into();
                return Ok(());
            }
            ajudou = true;
            let (ajuda, leu, escolhida) = ver(op, sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?, controle, r, ciclo, &obs, &estado, secretos,
                print.as_deref(), jev, imagem.as_ref(), lido_como.window.as_deref()).await?;
            // Jev's pick on the print, when sure enough, is the help's action if Jev stays unsure; else the first one.
            let pela_jev = escolhida.filter(|(_, p)| *p >= FRACO_COM_PRINT)
                .and_then(|(a, _)| match &no_campo(a, &obs, recentes)[..] { [a] => Some(a.clone()), _ => None });
            leitura_valida = leu;
            leitura = ajuda.interpretacao.clone();
            dica = com_dica(&nome, falta_dica.as_ref().map_or_else(|| ajuda.interpretacao.clone(), |f| format!("{f}\n{}", ajuda.interpretacao)));
            if !ajuda.impedimento.is_empty() { r.motivo = ajuda.impedimento; return Ok(()); }
            // Without a reading the helper neither confirmed nor denied the block; the loop goes on as it would without it.
            if bloqueado {
                let quem = if leu { "auxiliar_discorda" } else { "auxiliar_indisponivel" };
                controle.rodar(registrar(&pasta, json!({"ciclo":ciclo, quem:true}), secretos)).await?;
            }
            let propostas = ajuda.acoes.into_iter().filter(|a| a.kind != ActionType::Focus && a.target.as_ref().is_none_or(|id|
                obs.windows.iter().any(|w| &w.id == id) || obs.elements.iter().any(|e| &e.id == id)))
                .flat_map(|a| no_campo(a, &obs, recentes)).map(|a| secretos.candidato(a, &obs)).collect();
            for proposta in Barreiras::sem_confirmar(segurar_segredos(Barreiras::barrar(sem_reabrir(apps_do_objetivo(propostas, &op.texto), &fechados), recentes), &op.texto, recentes, &secretos.valores), &obs, &op.texto).0
                .into_iter().filter(|p| !texto_sem_alvo_barrado(&p.acao, foco_escolhido)) {
                if (primeira_ajuda.is_none() || pela_jev.as_ref() == Some(&proposta.acao)) && dentro(&proposta.acao, &obs) {
                    primeira_ajuda = Some(proposta.clone());
                }
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
        if let Some(spec) = drill.remove(&i) {
            regiao = spec;
            r.passos.push(format!("{}. {} [{}]", r.passos.len() + 1, acoes[i].descricao, percentual(decisao.p)));
            continue;
        }
        let mut candidato = acoes.get(i).ok_or("IndexError: list index out of range")?.clone();
        candidato.acao.input = modo_entrada(&candidato.acao, &obs, &aprendido, op.ver);
        candidato.acao.cursor = op.ver;
        // What the one-step tool reports: this pick, unless it is a look past the last step allowed.
        if ciclo < op.max_passos {
            let chave = i.to_string();
            r.ultima = Some(crate::resultado::UmPasso { acao: candidato.descricao.clone(), confianca: decisao.p,
                segundo: decisao.top.iter().find(|(k, ..)| *k != chave).map(|(_, p, d)| (d.clone(), *p)), risco: decisao.risco,
                ..Default::default() });
        }
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
            if op.so_decidir { r.motivo = "decidido, sem agir".into(); return Ok(()); }
            let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
            let rotulo = secretos.descricao(acao, &vista, false);
            let marcado = print_da_acao(&pasta, ciclo, acao, &vista, lido_como.window.as_deref(), print.as_deref());
            let mut retorno = controle.agir(s, &vista, acao, Progresso::Acao(rotulo.clone(), marcado), lido_como.window.as_deref()).await?;
            // Refused before it touched the app, on a fresh look of the same screen: with real input when only that
            // reaches it, after the user stops (up to a minute), or once more for any other refusal.
            let (mut reenviou, mut subiu, desde) = (false, false, Instant::now());
            while let Err(SessionError::NotDelivered(motivo)) = &retorno {
                let ativo = [recusa::USUARIO_ATIVO, recusa::ENTRADA_OCUPADA, JANELA_OCUPADA].iter().any(|r| motivo.contains(r));
                if [recusa::ENTRADA_REAL, recusa::JANELA_ATRAS, recusa::VALOR_NAO_CONFIRMADO].iter().any(|r| motivo.contains(r)) && !subiu {
                    subiu = true;
                    candidato.acao.input = Some(InputMode::Polite);
                } else if ativo && desde.elapsed() < USUARIO_ATIVO {
                    // A busy turn was already waited for by the agent.
                    if !motivo.contains(recusa::ENTRADA_OCUPADA) { controle.esperar(Duration::from_secs(1)).await?; }
                } else if !ativo && !reenviou {
                    reenviou = true;
                } else {
                    break;
                }
                let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
                let nova = match controle.medir("observacao", Progresso::Observacao, async { Ok(observar(s, &lido_como).await) }).await? {
                    Ok(n) => n,
                    Err(e) if fechou(&e) => break,
                    Err(e) => return Err(erro_sessao(e)),
                };
                let Some(de_novo) = mesmo_alvo(&candidato.acao, &vista, &nova) else { break };
                candidato.acao = de_novo;
                vista = nova;
                atual = assinatura_tela(&vista);
                anterior = Some(atual.clone());
                let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
                retorno = controle.agir(s, &vista, &candidato.acao, Progresso::Acao(rotulo.clone(), None), lido_como.window.as_deref()).await?;
            }
            let acao = &candidato.acao;
            if let Some(u) = r.ultima.as_mut().filter(|u| u.entrega.is_none()) {
                u.entrega = Some(match &retorno {
                    Ok(ret) => ret.delivery,
                    Err(SessionError::NotDelivered(_)) => Delivery::NotDelivered,
                    Err(_) => Delivery::Unknown,
                });
            }
            let registrados = recentes.len();
            let falhou = match retorno {
                Ok(ret) => {
                    if ret.ok {
                        // A control acted on names the app's window; a launch or activation brings another to the front.
                        match acao.kind {
                            ActionType::Activate if !op.ver => janela_app = acao.target.clone(),
                            // The launched app's window shows up in a later look: the first new window becomes the app's.
                            ActionType::Launch => {
                                janela_app = None; janelas_do_app.clear(); abriu = if op.ver { 0 } else { 3 };
                                lancado = acao.application.as_deref().map(programa);
                            }
                            _ if acao.target.as_ref().is_some_and(|t| vista.elements.iter().any(|e| &e.id == t)) && !op.ver => janela_app = Some(vista.foreground.clone()),
                            _ => {}
                        }
                        foco_escolhido |= escolhe_foco(acao) || invoca_editavel(acao, &vista);
                        em_foco_nas_acoes.extend(vista.windows.iter().find(|w| w.id == vista.foreground).cloned());
                    }
                    let alvo = acao.target.as_ref().and_then(|id| vista.elements.iter().rev().find(|e| &e.id == id));
                    let clique = match (acao.kind, acao.mode, acao.x, acao.y) {
                        (ActionType::Mouse, Some(m), Some(x), Some(y)) => Some((m, x, y)), _ => None,
                    };
                    let result = match (ret.ok, ret.delivery) {
                        (false, _) => "{'ok': False}",
                        (true, Delivery::DeliveredVerified) => "ok (confirmado)",
                        (true, _) => "ok",
                    };
                    recentes.push(Registro { action: nome.clone(), result: result.into(),
                        screen_changed: None, target_rect: alvo.and_then(|e| e.rect), clique, executada: true });
                    !ret.ok
                }
                // It may have run: the next look measures whether the screen changed, and nothing resends it.
                Err(e @ SessionError::Timeout(_)) => {
                    recentes.push(Registro { action: nome.clone(), result: secretos.texto(&format!("delivery unknown, check the screen: {e}")),
                        screen_changed: None, target_rect: None, clique: None, executada: false });
                    if !controle.cancel.is_cancelled() { reconectar(conector, sessao, controle).await?; }
                    true
                }
                Err(e) => {
                    recentes.push(Registro { action: nome.clone(), result: secretos.texto(&format!("NOT executed: {e}")),
                        screen_changed: Some(false), target_rect: None, clique: None, executada: false });
                    if reconectavel(&e) && !controle.cancel.is_cancelled() { reconectar(conector, sessao, controle).await?; }
                    true
                }
            };
            if let Some(u) = r.ultima.as_mut().filter(|u| u.resultado.is_none()) {
                u.resultado = recentes.get(registrados).map(|x| x.result.clone());
            }
            r.passos.push(format!("{}. {nome} [{}]", r.passos.len() + 1, percentual(decisao.p)));
            if let Some((papel, nome, valor)) = digitacao(acao, &vista, &secretos.valores).filter(|_| !falhou) {
                digitado = Some((recentes.len() - 1, papel, nome, valor));
            }
            if !falhou && op.roteiros.is_some() {
                gravados.push((roteiro::passo(acao, &vista, &op.dados), Some(recentes.len() - 1), hash_tela(&atual), trecho::forma(&vista, &valores_meta)));
            }
            let mudou_tela = esperar_efeito(conector, sessao, controle, acao, &vista, &atual, secretos, &lido_como).await?;
            // Background accepted but nothing changed: the app may ignore accessibility for this control. Once with real
            // input; when that works, the app is remembered. Never for invoke (a save or send with no visible effect
            // would run twice), nor for a value the agent read back.
            if !mudou_tela && !falhou && acao.input == Some(InputMode::Background) && acao.kind != ActionType::Invoke
                && recentes.last().is_some_and(|u| u.result == "ok") {
                let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
                let nova = match controle.medir("observacao", Progresso::Observacao, async { Ok(observar(s, &lido_como).await) }).await? {
                    Ok(n) => Some(n),
                    Err(e) if fechou(&e) => None,
                    Err(e) => return Err(erro_sessao(e)),
                };
                if let Some((nova, real)) = nova.and_then(|n| mesmo_alvo(acao, &vista, &n).map(|a| (n, Action { input: Some(InputMode::Polite), ..a }))) {
                    vista = nova;
                    atual = assinatura_tela(&vista);
                    anterior = Some(atual.clone());
                    let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
                    let feito = controle.agir(s, &vista, &real, Progresso::Acao(secretos.descricao(&real, &vista, false), None), lido_como.window.as_deref()).await?;
                    if feito.as_ref().is_ok_and(|r| r.ok) {
                        if let Some(u) = recentes.last_mut() { u.result = "ok (refeito com entrada real)".into(); }
                        if esperar_efeito(conector, sessao, controle, &real, &vista, &atual, secretos, &lido_como).await?
                            && aprendido.aprender(&vista, &real) && let Some(d) = op.roteiros.as_deref()
                            && let Err(e) = aprendido.gravar(d) {
                            eprintln!("não foi possível gravar {}: {e}", crate::modo::ARQUIVO);
                        }
                    }
                    candidato.acao = real;
                }
            }
            let Some(proxima) = fila.next().filter(|_| !falhou) else { break };
            // The agent consumes each observation id: the next key/text needs a fresh one.
            let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
            vista = match controle.medir("observacao", Progresso::Observacao, async { Ok(observar(s, &lido_como).await) }).await? {
                Ok(v) => v,
                Err(e) if fechou(&e) => break,
                Err(e) => return Err(erro_sessao(e)),
            };
            let nova = assinatura_tela(&vista);
            if let Some(ultima) = recentes.last_mut() { ultima.screen_changed = mudou(&atual, &nova); }
            atual = nova;
            anterior = Some(atual.clone());
            let mut gravada = serde_json::to_value(&vista).map_err(|e| format!("ValueError: {e}"))?;
            secretos.valor(&mut gravada);
            controle.rodar(async { tokio::fs::write(pasta.join("observacao.json"), py_dumps(&gravada)).await.map_err(erro_io) }).await?;
            // A new window or a message box takes the queued keys: the next cycle looks at it first.
            if vista.foreground != primeiro_plano || aviso(&vista).is_some() { break; }
            // Same action again while the last one's effect is unknown (empty tree): look first, never resend.
            let repete_cego = recentes.last().is_some_and(|u| u.action == proxima.descricao && u.screen_changed != Some(true));
            if repete_cego || texto_sem_alvo_barrado(&proxima.acao, foco_escolhido)
                || Barreiras::tres_sem_efeito(recentes).is_some() || Barreiras::barrar(vec![proxima.clone()], recentes).is_empty() { break; }
            let estado = secretos.estado(op, &vista, recentes, None);
            let limite = controle.restante()?.min(Duration::from_secs(30));
            (decisao.risco, _) = controle.medir("jev", Progresso::Jev, jev.arriscado(&estado, &proxima.descricao, None, limite)).await?;
            controle.rodar(registrar(&pasta, json!({"ciclo":ciclo, "direta":proxima.descricao, "risky":decisao.risco, "dica":dica}), secretos)).await?;
            candidato = proxima;
            candidato.acao.input = modo_entrada(&candidato.acao, &vista, &aprendido, op.ver);
            candidato.acao.cursor = op.ver;
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
    let prefixo = std::env::temp_dir().join(format!("hangar-computer-control-{digest}")).to_string_lossy().into_owned();
    let trava = format!("{prefixo}.lock");
    let mut controle = Controle { prazo: inicio.checked_add(op.limite).unwrap_or(inicio), cancel: &cancel, progresso, tempos: vec![],
        trava: prefixo, janelas: HashMap::new() };
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
            // Goals in the background share the desktop (each takes its windows); watching takes all of it.
            match if op.ver { file.try_lock() } else { file.try_lock_shared() } {
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
    r.tempos = std::mem::take(&mut controle.tempos);
    r.tempos.push(format!("total={:.2}s", inicio.elapsed().as_secs_f64()));
    if let (Some(pasta), Some(secretos)) = (&r.registro, &secretos) {
        let mut fim = json!({"resultado": {"ok": r.ok, "motivo": r.motivo, "tempos": r.tempos}, "t": agora()});
        if let Some(jev) = &op.jev { fim["jev"] = jev.uso(); }
        secretos.valor(&mut fim);
        let linha = format!("{}\n", py_dumps(&fim));
        // Best effort: the result already reached the caller; a full disk must not turn it into a failure.
        if let Err(e) = std::fs::OpenOptions::new().append(true).create(true).open(pasta.join("jev.jsonl"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, linha.as_bytes())) {
            eprintln!("registro final não gravado: {e}");
        }
    }
    r
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn early_change_needs_a_second_equal_look() {
        let cedo = Duration::from_millis(300);
        assert!(!efeito_visto(true, false, cedo), "menu montando: a primeira olhada cedo não basta");
        assert!(efeito_visto(true, true, cedo), "a mesma tela duas vezes seguidas basta");
        assert!(efeito_visto(true, false, Duration::from_millis(900)), "depois de 800 ms conta direto");
        assert!(!efeito_visto(false, false, Duration::from_secs(2)));
    }

    #[tokio::test]
    async fn log_write_error_is_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/dev/full", dir.path().join("jev.jsonl")).unwrap();
        let secretos = Segredos::new(&Map::new()).unwrap();
        let r = registrar(dir.path(), json!({"choice":"DONE"}), &secretos).await;
        assert!(r.is_err(), "erro da gravação precisa chegar ao loop");
    }
}
