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

use crate::barreiras::{Barreiras, aviso, escolhe_foco, literais_atendidos, sem_acao_pedida, sem_localizadores, fecha_janela, itens_pendentes, objetivo_pede_fechar, pendencia, segurar_segredos, texto_sem_alvo_barrado};
use crate::candidatos::{apps_do_objetivo, candidatos, descrever_acao, py_str, segredos, sem_app_aberto, texto_no_editavel};
use crate::descrever::{intencao, py_repr};
use crate::estado::{AssinaturaTela, assinatura_tela, estado_compacto, padrao_segredos, sem_controles};
use crate::geometria::{dentro, para_tela};
use crate::http::py_dumps;
use crate::jev::Jev;
use crate::llm::Llm;
use crate::objetivo::{literais, nome_parecido, nomes_em_janela_nova, nomes_no_titulo, pede_leitura, so_leitura, texto_janela, valores_do_objetivo};
use crate::trecho;
use crate::resultado::Resultado;
use crate::roteiro;
use crate::tipos::{Ajuda, Candidato, Decisao, Escolha, Progresso, Registro};

const CANCELADO: &str = "RuntimeError: objetivo cancelado; nenhuma nova ação será enviada";
const SEM_PROGRESSO: u32 = 6;
const LIMITE: &str = "TimeoutError: limite de duração atingido; objetivo não confirmado";

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

/// Invoke on an editable field puts the caret there, like a click.
fn invoca_editavel(acao: &Action, obs: &Observation) -> bool {
    acao.kind == ActionType::Invoke && acao.target.as_ref().is_some_and(|id| obs.elements.iter()
        .any(|e| &e.id == id && (e.role == "Edit" || e.actions.contains(&hcc_protocolo::ElementAction::SetValue))))
}

fn com_dica(nome: &Option<String>, texto: String) -> String {
    match nome { Some(n) if texto.is_empty() => n.clone(), Some(n) => format!("{n}\n{texto}"), None => texto }
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
    if let Some(pasta) = r.registro.clone() {
        let bruto = json!({"llm": ajuda.acoes, "interpretacao": ajuda.interpretacao, "impedimento": ajuda.impedimento});
        controle.rodar(registrar(&pasta, bruto, secretos)).await?;
    }
    let propostas = ajuda.acoes.len();
    ajuda.acoes = ajuda.acoes.iter().map(|a| para_tela(a, &obs.screen, &op.texto)).filter_map(|a| secretos.desmascarar(a)).collect();
    if ajuda.acoes.len() < propostas {
        ajuda.interpretacao.push_str(&format!(" [{} ação(ões) com <senha> descartada(s): não há um único segredo nos dados]", propostas - ajuda.acoes.len()));
    }
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

async fn esperar_efeito<C: Connector>(conector: &C, sessao: &mut Option<C::S>, controle: &mut Controle<'_>,
    acao: &Action, vista: &Observation, atual: &AssinaturaTela, secretos: &Segredos) -> Result<(), String> {
    // Blind (empty tree): only the title could change; no polling, the next observation looks.
    let prazo = Instant::now() + if acao.kind == ActionType::Launch { Duration::from_secs(45) }
        else if vista.elements.is_empty() { Duration::from_millis(800) } else { Duration::from_secs(3) };
    controle.esperar(Duration::from_millis(800)).await?;
    while Instant::now() < prazo {
        let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
        match controle.rodar(async { Ok(observar(s).await) }).await? {
            Ok(nova) if assinatura_tela(&nova) != *atual => break,
            Ok(_) => {}
            Err(e) if reconectavel(&e) => reconectar(conector, sessao, controle).await?,
            Err(e) if e.tipo() == "RuntimeError" => eprintln!("observação durante espera ignorada: {}", secretos.texto(&e.to_string())),
            Err(e) => return Err(erro_sessao(e)),
        }
        controle.esperar(Duration::from_secs(1)).await?;
    }
    Ok(())
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
    let res = ciclo_passos(op, conector, sessao, controle, r, secretos, &mut gravados, &mut recentes).await;
    // A field left with another value is the likely cause of a stop: the caller must see it.
    if !r.ok && let Some(falha) = recentes.iter().rev().find(|x| x.action.starts_with("set_value ") || x.action.starts_with("text "))
        .and_then(|x| x.result.strip_prefix("NOT confirmed: ")).filter(|f| !r.motivo.contains(*f)) {
        r.motivo = format!("{}; {falha}", r.motivo);
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
fn anotar_diario(op: &Opcoes, r: &Resultado, secretos: &Segredos, gravados: &[Gravado],
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
    if let Err(e) = escrito { eprintln!("diário de passos não gravado em {}: {e}", arquivo.display()); }
}

#[allow(clippy::too_many_arguments)]
async fn ciclo_passos<C: Connector>(op: &Opcoes, conector: &C, sessao: &mut Option<C::S>, controle: &mut Controle<'_>,
    r: &mut Resultado, secretos: &Segredos, gravados: &mut Vec<Gravado>, recentes: &mut Vec<Registro>) -> Result<(), String> {
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
            match controle.medir("jev", Progresso::Jev, jev.escolher(&estado, PARECIDO, &opcoes, limite)).await {
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
                    async { observar(s).await.map_err(erro_sessao) }).await?;
                let atual = assinatura_tela(&vista);
                iniciais.get_or_insert_with(|| vista.windows.iter().map(|w| w.id.clone()).collect());
                if !pulou {
                    if let (Some(ultima), Some(a)) = (recentes.last_mut(), anterior.as_ref()) { ultima.screen_changed = mudou(a, &atual); }
                }
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
                let nome = secretos.descricao(&acao, &vista, true);
                let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
                let retorno = controle.medir(&format!("acao:{}", acao.kind), Progresso::Acao(secretos.descricao(&acao, &vista, false)),
                    async { Ok(s.act(&vista.observation_id, &acao).await) }).await?;
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
                esperar_efeito(conector, sessao, controle, &acao, &vista, &atual, secretos).await?;
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
    // Without a script for this goal, pieces recorded by any goal: Jev picks the one that fits the screen and the
    // goal, its slots filled with the goal's values; each step is checked, and the first divergence leaves it to Jev.
    // A reading goal never acts, not even through a piece.
    let pecas = match op.roteiros.as_deref() {
        Some(d) if sem_roteiro && !op.refazer && !so_leitura(&op.texto) => trecho::todos(&d.join("trechos")), _ => vec![] };
    if !pecas.is_empty() {
        let valores = valores_do_objetivo(&op.texto, &op.dados, &secretos.valores);
        let mut repetidos = Vec::new();
        let mut feitas: Vec<(usize, std::collections::BTreeMap<String, String>)> = Vec::new();
        let mut parou = None;
        'pecas: for _ in 0..5 {
            let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
            let vista = controle.medir("observacao", Progresso::Observacao, async { observar(s).await.map_err(erro_sessao) }).await?;
            let atual = assinatura_tela(&vista);
            iniciais.get_or_insert_with(|| vista.windows.iter().map(|w| w.id.clone()).collect());
            if let (Some(ultima), Some(a)) = (recentes.last_mut(), anterior.as_ref()) && ultima.screen_changed.is_none() {
                ultima.screen_changed = mudou(a, &atual);
            }
            anterior = Some(atual);
            // A piece already replayed with the same values would only repeat itself.
            let opcoes: Vec<trecho::Opcao> = trecho::opcoes(&pecas, &vista, &op.dados, &valores).into_iter()
                .filter(|o| !feitas.contains(&(o.trecho, o.params.clone()))).collect();
            if opcoes.is_empty() { break; }
            let textos: Vec<String> = opcoes.iter().map(|o| secretos.texto(&o.texto)).collect();
            let estado = secretos.estado(op, &vista, recentes, None);
            let limite = controle.restante()?.min(Duration::from_secs(30));
            let (i, p) = match controle.medir("jev", Progresso::Jev, jev.escolher(&estado, TRECHO, &textos, limite)).await {
                Ok(v) => v,
                Err(e) => { parou = Some(format!("escolha de trecho falhou: {}", secretos.texto(&e))); break; }
            };
            controle.rodar(registrar(&pasta, json!({"trecho": i.map(|i| &textos[i]), "probability": p}), secretos)).await?;
            let Some(i) = i.filter(|_| p >= 0.8) else { break; };
            let opcao = &opcoes[i];
            let passos = &pecas[opcao.trecho].1.variantes[opcao.variante].passos;
            let mut vista = vista;
            for (n, passo) in passos.iter().enumerate() {
                if n > 0 {
                    let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
                    vista = controle.medir("observacao", Progresso::Observacao, async { observar(s).await.map_err(erro_sessao) }).await?;
                    let agora = assinatura_tela(&vista);
                    if let (Some(ultima), Some(a)) = (recentes.last_mut(), anterior.as_ref()) { ultima.screen_changed = mudou(a, &agora); }
                    anterior = Some(agora);
                    if passos[n - 1].mudou_tela == Some(true) && recentes.last().is_some_and(|u| u.screen_changed == Some(false)) {
                        parou = Some(format!("trecho {} parou no passo {}: a tela não mudou", textos[i], n + 1)); break 'pecas;
                    }
                }
                let atual = assinatura_tela(&vista);
                let Some(acao) = roteiro::resolver_com(passo, &vista, &op.dados, &opcao.params) else {
                    parou = Some(format!("trecho {} parou no passo {}: controle diferente do gravado", textos[i], n + 1)); break 'pecas;
                };
                let nome = secretos.descricao(&acao, &vista, true);
                let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
                let retorno = controle.medir(&format!("acao:{}", acao.kind), Progresso::Acao(secretos.descricao(&acao, &vista, false)),
                    async { Ok(s.act(&vista.observation_id, &acao).await) }).await?;
                let ok = matches!(&retorno, Ok(ret) if ret.ok);
                let resultado = match &retorno { Ok(ret) if ret.ok => "ok".to_owned(), Ok(_) => "{'ok': False}".to_owned(),
                    Err(e) => secretos.texto(&format!("NOT executed: {e}")) };
                recentes.push(Registro { action: nome.clone(), result: resultado.clone(), screen_changed: None, target_rect: None,
                    clique: None, executada: retorno.is_ok() });
                r.passos.push(format!("{}. {nome} [trecho]", r.passos.len() + 1));
                if let Err(e) = &retorno && reconectavel(e) && !controle.cancel.is_cancelled() { reconectar(conector, sessao, controle).await?; }
                if !ok { parou = Some(format!("trecho {} parou no passo {}: {resultado}", textos[i], n + 1)); break 'pecas; }
                foco_escolhido |= escolhe_foco(&acao) || invoca_editavel(&acao, &vista);
                gravados.push((roteiro::passo(&acao, &vista, &op.dados), Some(recentes.len() - 1), hash_tela(&atual), trecho::forma(&vista, &valores_meta)));
                esperar_efeito(conector, sessao, controle, &acao, &vista, &atual, secretos).await?;
            }
            feitas.push((opcao.trecho, opcao.params.clone()));
            repetidos.push(textos[i].clone());
        }
        if !repetidos.is_empty() || parou.is_some() {
            let mut texto = format!("trechos repetidos: {}", if repetidos.is_empty() { "nenhum".into() } else { repetidos.join(" | ") });
            if let Some(p) = parou { texto.push_str(&format!("; {p}")); }
            r.roteiro = Some(r.roteiro.take().map_or_else(|| texto.clone(), |antes| format!("{antes}; {texto}")));
        }
    }
    for ciclo in 0..=op.max_passos {
        let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
        let obs = controle.medir("observacao", Progresso::Observacao,
            async { observar(s).await.map_err(erro_sessao) }).await?;
        let atual = assinatura_tela(&obs);
        sem_tela_nova = if vistas.insert(atual.clone()) { 0 } else { sem_tela_nova + 1 };
        iniciais.get_or_insert_with(|| obs.windows.iter().map(|w| w.id.clone()).collect());
        let conferir = &*conferir.get_or_insert_with(|| sem_localizadores(&op.texto, &obs));
        if let Some(ultima) = recentes.last_mut().filter(|r| r.screen_changed.is_none()) {
            ultima.screen_changed = anterior.as_ref().map_or(Some(true), |a| mudou(a, &atual));
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
        let mut gravada = serde_json::to_value(&obs).map_err(|e| format!("ValueError: {e}"))?;
        secretos.valor(&mut gravada);
        controle.rodar(async { tokio::fs::write(pasta.join("observacao.json"), py_dumps(&gravada)).await.map_err(erro_io) }).await?;
        // Every cycle's screen stays, so a run can be read back step by step.
        controle.rodar(async { tokio::fs::write(pasta.join(format!("observacao-{ciclo}.json")), py_dumps(&gravada)).await.map_err(erro_io) }).await?;
        if barreiras.aviso_repetido(&obs).is_some() {
            let texto: String = secretos.texto(&aviso(&obs).unwrap_or_default()).chars().take(400).collect();
            r.motivo = format!("o aplicativo respondeu duas vezes com o mesmo aviso: {texto}"); return Ok(());
        }
        if so_leitura(&op.texto) {
            // Nothing to do on screen: the tree text and one print reading are the answer; no Jev, no action.
            let estado = secretos.estado(op, &obs, recentes, None);
            let (ajuda, leu) = ver(op, sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?, controle, r, &obs, &estado, secretos).await?;
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
        let (acoes, confirmacao) = Barreiras::sem_confirmar(segurar_segredos(Barreiras::barrar(cands, recentes), &op.texto, recentes, &secretos.valores), &obs, &op.texto);
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
            let (ajuda, leu) = ver(op, sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?, controle, r, &obs, &estado, secretos).await?;
            leitura_valida = leu;
            leitura = ajuda.interpretacao.clone();
            if !ajuda.impedimento.is_empty() { r.motivo = ajuda.impedimento; return Ok(()); }
            dica = com_dica(&nome, ajuda.interpretacao); ajudou = true;
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
            let estado = secretos.estado(op, &obs, recentes, Some(&dica));
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
                    let sem_dica = secretos.estado(op, &obs, recentes, None);
                    let limite = controle.restante()?.min(Duration::from_secs(30));
                    let d2 = controle.medir("jev", Progresso::Jev, jev.decidir(&sem_dica, &acoes, limite)).await?;
                    controle.rodar(registrar(&pasta, json!({"ciclo":ciclo, "sem_dica":true,
                        "choice":escolha(d2.escolha), "probability":d2.p, "risky":d2.risco}), secretos)).await?;
                    controle.restante()?;
                    // Without the hint Jev sees no reading at all: WAIT there is "cannot tell", not an objection.
                    // Below 95% the reading counts only where nothing else could: an empty tree, or a goal whose
                    // deliverable is the reading itself.
                    let so_a_leitura = d.p >= 0.95 || obs.elements.is_empty() || pede_leitura(&op.texto);
                    if leitura_valida && so_a_leitura && matches!(d2.escolha, Escolha::Done | Escolha::Wait) {
                        confirmado = Some(d.p);
                        leitura_forte = true;
                    } else if d2.escolha == Escolha::Done && d2.p >= 0.75 { confirmado = Some(d2.p); }
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
            let fraco = d.escolha == Escolha::Blocked || d.p < if d.escolha == Escolha::Done { 0.75 } else { 0.25 }
                || (d.escolha == Escolha::Wait && esperando);
            // A truncated tree may hide the control the goal needs, but a sure pick was found in what came.
            if !fraco && (!obs.elements.is_empty() && (!obs.truncated || d.p >= 0.6) && divergencia.is_none() || ajudou) { break d; }
            if ajudou {
                // Jev still unsure: the help's first action decides, after the risk question.
                if let Some(proposta) = primeira_ajuda.take() {
                    let estado = secretos.estado(op, &obs, recentes, Some(&dica));
                    let limite = controle.restante()?.min(Duration::from_secs(30));
                    let risco = controle.medir("jev", Progresso::Jev, jev.arriscado(&estado, &proposta.descricao, limite)).await?;
                    controle.rodar(registrar(&pasta, json!({"ciclo":ciclo, "direta":proposta.descricao, "risky":risco, "dica":dica}), secretos)).await?;
                    let i = acoes.iter().position(|a| a.acao == proposta.acao).ok_or("IndexError: list index out of range")?;
                    // Jev's own confidence stays on record: the step came from the help, not from a sure vote.
                    break Decisao { escolha: Escolha::Indice(i), p: d.p, risco, top: vec![] };
                }
                r.motivo = format!("sem ação segura: Jev escolheu {opcao} com {}. {dica}", percentual(d.p)).trim().into();
                return Ok(());
            }
            ajudou = true;
            let (ajuda, leu) = ver(op, sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?, controle, r, &obs, &estado, secretos).await?;
            leitura_valida = leu;
            leitura = ajuda.interpretacao.clone();
            dica = com_dica(&nome, falta_dica.as_ref().map_or_else(|| ajuda.interpretacao.clone(), |f| format!("{f}\n{}", ajuda.interpretacao)));
            if !ajuda.impedimento.is_empty() { r.motivo = ajuda.impedimento; return Ok(()); }
            let propostas = ajuda.acoes.into_iter().filter(|a| a.kind != ActionType::Focus && a.target.as_ref().is_none_or(|id|
                obs.windows.iter().any(|w| &w.id == id) || obs.elements.iter().any(|e| &e.id == id)))
                .flat_map(|a| no_campo(a, &obs, recentes)).map(|a| secretos.candidato(a, &obs)).collect();
            for proposta in Barreiras::sem_confirmar(segurar_segredos(Barreiras::barrar(sem_reabrir(apps_do_objetivo(propostas, &op.texto), &fechados), recentes), &op.texto, recentes, &secretos.valores), &obs, &op.texto).0
                .into_iter().filter(|p| !texto_sem_alvo_barrado(&p.acao, foco_escolhido)) {
                if primeira_ajuda.is_none() && dentro(&proposta.acao, &obs) { primeira_ajuda = Some(proposta.clone()); }
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
                        foco_escolhido |= escolhe_foco(acao) || invoca_editavel(acao, &vista);
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
            if let Some((papel, nome, valor)) = digitacao(acao, &vista, &secretos.valores).filter(|_| !falhou) {
                digitado = Some((recentes.len() - 1, papel, nome, valor));
            }
            if !falhou && op.roteiros.is_some() {
                gravados.push((roteiro::passo(acao, &vista, &op.dados), Some(recentes.len() - 1), hash_tela(&atual), trecho::forma(&vista, &valores_meta)));
            }
            esperar_efeito(conector, sessao, controle, acao, &vista, &atual, secretos).await?;
            let Some(proxima) = fila.next().filter(|_| !falhou) else { break };
            // The agent consumes each observation id: the next key/text needs a fresh one.
            let s = sessao.as_mut().ok_or("RuntimeError: sessão indisponível")?;
            vista = controle.medir("observacao", Progresso::Observacao,
                async { observar(s).await.map_err(erro_sessao) }).await?;
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
    r.tempos = std::mem::take(&mut controle.tempos);
    r.tempos.push(format!("total={:.2}s", inicio.elapsed().as_secs_f64()));
    if let (Some(pasta), Some(secretos)) = (&r.registro, &secretos) {
        let mut fim = json!({"resultado": {"ok": r.ok, "motivo": r.motivo, "tempos": r.tempos}, "t": agora()});
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

    #[tokio::test]
    async fn log_write_error_is_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/dev/full", dir.path().join("jev.jsonl")).unwrap();
        let secretos = Segredos::new(&Map::new()).unwrap();
        let r = registrar(dir.path(), json!({"choice":"DONE"}), &secretos).await;
        assert!(r.is_err(), "erro da gravação precisa chegar ao loop");
    }
}
