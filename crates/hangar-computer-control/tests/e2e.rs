//! The real binary end to end: MCP over stdio, the real loop and controller, the agent as a child
//! process of the same binary driving the scripted `FakeDesktop`, Jev and LLM at wiremock.
// `motor-fake` swaps the real engine out, so these tests only make sense without it.
#![cfg(all(feature = "fake-desktop", not(feature = "motor-fake")))]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_hangar-computer-control");
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

#[test]
fn fake_desktop_build_refuses_real_desktop() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("agent.json");
    std::fs::write(&config, "{}").unwrap();
    let output = Command::new(BIN)
        .env_clear()
        .args(["agent", "--config"])
        .arg(&config)
        .output().unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("build fake-desktop: defina HCC_FAKE_DESKTOP; o desktop real não é usado"), "{stderr}");
}

#[derive(Clone)]
enum Resposta {
    /// Picks the option whose criterion contains the text.
    Escolher(&'static str, f64),
    Opcao(&'static str, f64),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Demorar(Duration),
}

/// Jev answers in order; the last one repeats.
#[derive(Clone)]
struct Jev(Arc<Mutex<VecDeque<Resposta>>>);

impl Respond for Jev {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let corpo: Value = serde_json::from_slice(&req.body).unwrap();
        let resposta = {
            let mut fila = self.0.lock().unwrap();
            if fila.len() > 1 { fila.pop_front().unwrap() } else { fila.front().cloned().unwrap() }
        };
        let escolha = |c: &str, p: f64| json!({"answers": {
            "action": {"choice": c, "probabilities": {c: p}}, "risky": {"noul": 0.05}}});
        match resposta {
            Resposta::Escolher(texto, p) => {
                let criterios = corpo["questions"]["action"]["criteria"].as_object().unwrap();
                let indice = criterios.iter()
                    .find(|(_, v)| v.as_str().is_some_and(|v| v.contains(texto)))
                    .unwrap_or_else(|| panic!("sem critério {texto}: {criterios:?}")).0;
                ResponseTemplate::new(200).set_body_json(escolha(indice, p))
            }
            Resposta::Opcao(c, p) => ResponseTemplate::new(200).set_body_json(escolha(c, p)),
            Resposta::Demorar(d) => ResponseTemplate::new(200).set_delay(d).set_body_json(escolha("WAIT", 0.9)),
        }
    }
}

fn observacao(valor: &str, conectado: bool) -> Value {
    json!({
        "observation_id": "", "connected": conectado, "session_id": 1, "foreground": "w1",
        "windows": [{"id": "w1", "name": "Cadastro", "process_id": 10, "class_name": "TMainForm",
            "rect": [0, 0, 800, 600]}],
        "elements": [
            {"id": "e0", "name": "Nome", "role": "Edit", "value": valor, "enabled": true,
             "rect": null, "focused": true, "actions": ["set_value"]},
            {"id": "e1", "name": "Salvar", "role": "Button", "value": null, "enabled": true,
             "rect": null, "focused": false, "actions": ["invoke"]},
        ],
        "truncated": false, "timestamp": 1.0, "screen": {"width": 800, "height": 600},
    })
}

struct Ambiente {
    casa: tempfile::TempDir,
    alvos: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
    modelos: MockServer,
}

impl Ambiente {
    fn novo(roteiro: Value, jev: Vec<Resposta>) -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let modelos = rt.block_on(async {
            let s = MockServer::start().await;
            Mock::given(method("POST")).and(path("/jev"))
                .respond_with(Jev(Arc::new(Mutex::new(jev.into())))).mount(&s).await;
            let ajuda = json!({"interpretacao": "tela vista", "acoes": []}).to_string();
            Mock::given(method("POST")).and(path("/llm"))
                .respond_with(ResponseTemplate::new(200)
                    .set_body_json(json!({"choices": [{"message": {"content": ajuda}}]})))
                .mount(&s).await;
            s
        });
        let amb = Self { casa: tempfile::tempdir().unwrap(), alvos: tempfile::tempdir().unwrap(), _rt: rt, modelos };
        std::fs::write(amb.alvos.path().join("roteiro.json"), roteiro.to_string()).unwrap();
        // Short RPC timeout: a dead agent is noticed in 3 s instead of the default 15 s.
        let config = json!({"transport": "local", "request_timeout": 3, "command": [BIN, "agent"]});
        std::fs::write(amb.config(), config.to_string()).unwrap();
        amb
    }

    fn config(&self) -> PathBuf {
        self.alvos.path().join("fake-agent.json")
    }

    fn comando(&self) -> Command {
        let mut c = Command::new(BIN);
        // Nothing from the developer's environment leaks in; no real desktop is ever touched.
        for v in ["HCC_AGENTS_DIR", "HYPRLAND_INSTANCE_SIGNATURE", "LLM_EFFORT", "LLM_MODEL"] {
            c.env_remove(v);
        }
        // Lock, registro and screenshot dirs land inside the test's own dir and go with it.
        c.env("HOME", self.casa.path())
            .env("USERPROFILE", self.casa.path())
            .env("TMPDIR", self.casa.path())
            .env("TEMP", self.casa.path())
            .env("TMP", self.casa.path())
            .env("HCC_AGENT_CONFIG", self.config())
            .env("HCC_FAKE_DESKTOP", self.alvos.path().join("roteiro.json"))
            .env("TYPESAFE_API_KEY", "chave-falsa")
            .env("HCC_JEV_URL", format!("{}/jev", self.modelos.uri()))
            .env("LLM_PROXY_URL", format!("{}/llm", self.modelos.uri()))
            .env("LLM_PROXY_KEY", "chave-falsa")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    fn progresso(&self, id: &str) -> Vec<String> {
        let arquivo = self.casa.path().join(".hangar").join("tool-progress").join(format!("{id}.jsonl"));
        std::fs::read_to_string(&arquivo).unwrap_or_else(|e| panic!("{}: {e}", arquivo.display()))
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap()["message"].as_str().unwrap().to_owned())
            .collect()
    }
}

struct Servidor {
    filho: Child,
    entrada: Option<ChildStdin>,
    saida: Receiver<Value>,
    erros: Arc<Mutex<String>>,
    notificacoes: Vec<Value>,
}

impl Servidor {
    fn iniciar(mut c: Command) -> Self {
        let mut filho = c.spawn().unwrap();
        let entrada = filho.stdin.take();
        let (tx, saida) = mpsc::channel();
        let out = filho.stdout.take().unwrap();
        std::thread::spawn(move || {
            for linha in BufReader::new(out).lines().map_while(Result::ok) {
                let v: Value = serde_json::from_str(&linha).unwrap_or_else(|e| panic!("stdout não é JSON-RPC ({e}): {linha}"));
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        let erros = Arc::new(Mutex::new(String::new()));
        let err = filho.stderr.take().unwrap();
        let acumulado = erros.clone();
        std::thread::spawn(move || {
            for linha in BufReader::new(err).lines().map_while(Result::ok) {
                let mut a = acumulado.lock().unwrap();
                a.push_str(&linha);
                a.push('\n');
            }
        });
        let mut s = Self { filho, entrada, saida, erros, notificacoes: Vec::new() };
        s.enviar(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "0"}}}));
        s.resposta(0, Duration::from_secs(10));
        s.enviar(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        s
    }

    fn enviar(&mut self, msg: Value) {
        let e = self.entrada.as_mut().unwrap();
        writeln!(e, "{msg}").unwrap();
        e.flush().unwrap();
    }

    fn resposta(&mut self, id: u64, espera: Duration) -> Value {
        let limite = Instant::now() + espera;
        loop {
            let resto = limite.saturating_duration_since(Instant::now());
            let msg = self.saida.recv_timeout(resto)
                .unwrap_or_else(|_| panic!("sem resposta ao id {id}; stderr:\n{}", self.erros.lock().unwrap()));
            if msg["id"] == json!(id) {
                return msg;
            }
            self.notificacoes.push(msg);
        }
    }

    fn pedir(&mut self, id: u64, nome: &str, args: Value, meta: Value) {
        self.enviar(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": nome, "arguments": args, "_meta": meta}}));
    }

    fn chamar(&mut self, id: u64, nome: &str, args: Value, espera: Duration) -> String {
        self.pedir(id, nome, args, json!({}));
        let r = self.resposta(id, espera);
        r["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{r}")).to_owned()
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn sair_em(&mut self, espera: Duration) -> bool {
        let limite = Instant::now() + espera;
        while Instant::now() < limite {
            if self.filho.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for Servidor {
    fn drop(&mut self) {
        let _ = self.filho.kill();
        let _ = self.filho.wait();
    }
}

/// Children of `pid` (any state, zombies included), from /proc.
#[cfg(target_os = "linux")]
fn filhos(pid: u32) -> Vec<u32> {
    std::fs::read_dir("/proc").unwrap().filter_map(|e| {
        let filho: u32 = e.ok()?.file_name().to_str()?.parse().ok()?;
        let stat = std::fs::read_to_string(format!("/proc/{filho}/stat")).ok()?;
        // `pid (comm) state ppid …`; comm may hold spaces, so split after the last ')'.
        let ppid: u32 = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()?;
        (ppid == pid).then_some(filho)
    }).collect()
}

/// A zombie waiting for init to reap it counts as dead.
#[cfg(target_os = "linux")]
fn vivo(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| Some(s.rsplit_once(')')?.1.split_whitespace().next()? != "Z"))
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn esperar_agente(s: &Servidor) -> u32 {
    let limite = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(&pid) = filhos(s.filho.id()).first() {
            return pid;
        }
        assert!(Instant::now() < limite, "agente não subiu; stderr:\n{}", s.erros.lock().unwrap());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn concluir() -> Vec<Resposta> {
    vec![Resposta::Escolher("set_value", 0.9), Resposta::Opcao("DONE", 0.9)]
}

fn roteiro_cadastro() -> Value {
    json!({"observations": [observacao("", true), observacao("Zé", true)]})
}

#[test]
fn objetivo_reaches_concluido() {
    let amb = Ambiente::novo(roteiro_cadastro(), concluir());
    let mut s = Servidor::iniciar(amb.comando());
    let texto = s.chamar(1, "objetivo",
        json!({"texto": "preencher Nome com Zé", "dados": {"nome": "Zé"}, "limite_segundos": 60}),
        Duration::from_secs(70));
    assert_eq!(texto.lines().next().unwrap(), "concluído: objetivo confirmado (90%) na janela 'Cadastro'", "{texto}");
    let acoes = std::fs::read_to_string(amb.alvos.path().join("roteiro.acts.jsonl")).unwrap();
    let acoes: Vec<Value> = acoes.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(acoes.len(), 1, "{acoes:?}");
    assert_eq!(acoes[0]["action"], json!({"type": "set_value", "target": "e0", "value": "Zé"}));
}

#[test]
fn agent_killed_mid_run_answers_parou_within_limit() {
    let mut roteiro = roteiro_cadastro();
    roteiro["fail_screenshot"] = json!(true);
    let amb = Ambiente::novo(roteiro, vec![Resposta::Opcao("BLOCKED", 0.9)]);
    let mut s = Servidor::iniciar(amb.comando());
    let limite = 10.0;
    let inicio = Instant::now();
    let texto = s.chamar(1, "objetivo", json!({"texto": "preencher Nome", "limite_segundos": limite}),
        Duration::from_secs(40));
    let gasto = inicio.elapsed().as_secs_f64();    // Python parity: a dead local agent is noticed when its RPC times out (request_timeout).
    assert_eq!(texto.lines().next().unwrap(), "parou: TimeoutError: agente não respondeu a screenshot; execução encerrada");
    assert!(gasto < limite, "{gasto:.1}s ≥ {limite}s: {texto}");
    #[cfg(target_os = "linux")]
    assert_eq!(filhos(s.filho.id()), Vec::<u32>::new(), "agente sobrou (ou zumbi) depois da resposta");
}

#[cfg(target_os = "linux")]
#[test]
fn sigterm_during_consultation_startup_reaps_child() {
    for nome in ["ver_tela", "estado"] {
        let amb = Ambiente::novo(roteiro_cadastro(), concluir());
        let config = json!({"transport": "local", "command": ["/usr/bin/python3", "-I", "-c", "import signal; signal.pause()"]});
        std::fs::write(amb.config(), config.to_string()).unwrap();
        let mut s = Servidor::iniciar(amb.comando());
        s.pedir(1, nome, json!({}), json!({}));
        let agente = esperar_agente(&s);
        assert!(Command::new("kill").args(["-TERM", &s.filho.id().to_string()]).status().unwrap().success());
        let saiu = s.sair_em(Duration::from_secs(6));
        let sobrou = vivo(agente);
        if sobrou {
            assert!(Command::new("kill").args(["-KILL", &agente.to_string()]).status().unwrap().success());
        }
        assert!(saiu, "{nome}: server did not exit on SIGTERM");
        assert!(!sobrou, "{nome}: child {agente} survived shutdown");
        assert!(!std::fs::read_dir(amb.casa.path()).unwrap().any(|e|
            e.unwrap().file_name().to_string_lossy().starts_with("hcc-agent-")), "{nome}: connection directory survived shutdown");
    }
}

#[cfg(target_os = "linux")]
fn objetivo_pendurado() -> (Ambiente, Servidor, u32) {
    let amb = Ambiente::novo(roteiro_cadastro(), vec![Resposta::Demorar(Duration::from_secs(120))]);
    let mut s = Servidor::iniciar(amb.comando());
    s.pedir(1, "objetivo", json!({"texto": "preencher Nome", "limite_segundos": 200}), json!({}));
    let agente = esperar_agente(&s);
    // The agent is up; give the loop time to reach the Jev call that hangs.
    let limite = Instant::now() + Duration::from_secs(10);
    while amb._rt.block_on(amb.modelos.received_requests()).unwrap_or_default().is_empty() {
        assert!(Instant::now() < limite, "o laço não chegou ao Jev");
        std::thread::sleep(Duration::from_millis(50));
    }
    (amb, s, agente)
}

#[cfg(target_os = "linux")]
#[test]
fn stdin_eof_kills_agent_child() {
    let (_amb, mut s, agente) = objetivo_pendurado();
    drop(s.entrada.take());
    assert!(s.sair_em(Duration::from_secs(6)), "servidor não saiu após EOF; stderr:\n{}", s.erros.lock().unwrap());
    assert!(!vivo(agente), "agente {agente} sobreviveu ao EOF");
}

#[cfg(target_os = "linux")]
#[test]
fn client_cancel_stops_agent_child_and_server_keeps_serving() {
    let (amb, mut s, agente) = objetivo_pendurado();
    s.enviar(json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": 1, "reason": "teste"}}));
    let limite = Instant::now() + Duration::from_secs(6);
    while vivo(agente) {
        assert!(Instant::now() < limite, "agente {agente} sobreviveu ao cancelamento");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!amb.alvos.path().join("roteiro.acts.jsonl").exists(), "ação enviada depois do cancelamento");
    assert_eq!(s.chamar(2, "estado", json!({}), Duration::from_secs(20)), "disponível via acessibilidade, 800x600");
}

#[cfg(target_os = "linux")]
#[test]
fn sigterm_mid_objective_stops_agent_child() {
    let (_amb, mut s, agente) = objetivo_pendurado();
    let ok = Command::new("kill").args(["-TERM", &s.filho.id().to_string()]).status().unwrap();
    assert!(ok.success());
    assert!(s.sair_em(Duration::from_secs(6)), "servidor não saiu após SIGTERM; stderr:\n{}", s.erros.lock().unwrap());
    assert!(!vivo(agente), "agente {agente} sobreviveu ao SIGTERM");
}

#[test]
fn estado_and_ver_tela_texts() {
    let amb = Ambiente::novo(roteiro_cadastro(), concluir());
    std::fs::write(amb.alvos.path().join("off-agent.json"), "{").unwrap();
    let mut s = Servidor::iniciar(amb.comando());
    let espera = Duration::from_secs(40);
    assert_eq!(s.chamar(1, "estado", json!({}), espera), "disponível via acessibilidade, 800x600");

    let texto = s.chamar(2, "ver_tela", json!({}), espera);
    let (caminho, tempo) = texto.split_once("; ").unwrap_or_else(|| panic!("{texto}"));
    let caminho = Path::new(caminho);
    assert_eq!(caminho.file_name().unwrap(), "tela.png");
    assert!(caminho.parent().unwrap().file_name().unwrap().to_str().unwrap().starts_with("hcu-tela-"), "{texto}");
    let segundos = tempo.strip_suffix('s').unwrap_or_else(|| panic!("{texto}"));
    assert_eq!(segundos.split_once('.').map(|(_, d)| d.len()), Some(2), "{texto}");
    assert!(caminho.starts_with(amb.casa.path()), "{texto}");
    assert!(std::fs::read(caminho).unwrap().starts_with(PNG));

    let quebrado = s.chamar(3, "estado", json!({"alvo": "off"}), espera);
    assert!(quebrado.starts_with("indisponível: ") && quebrado.contains("off-agent.json"), "{quebrado}");

    let amb = Ambiente::novo(json!({"observations": [observacao("", false)]}), concluir());
    let mut s = Servidor::iniciar(amb.comando());
    assert_eq!(s.chamar(1, "estado", json!({}), espera), "indisponível: agente desconectado");
}

#[test]
fn progress_file_lines_in_order() {
    let amb = Ambiente::novo(roteiro_cadastro(), concluir());
    let mut s = Servidor::iniciar(amb.comando());
    s.pedir(1, "objetivo", json!({"texto": "preencher Nome com Zé", "dados": {"nome": "Zé"}, "limite_segundos": 60}),
        json!({"progressToken": "p", "claudecode/toolUseId": "toolu_e2e"}));
    let r = s.resposta(1, Duration::from_secs(70));
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().starts_with("concluído: "), "{r}");
    let notificados: Vec<String> = s.notificacoes.iter()
        .filter(|n| n["method"] == "notifications/progress")
        .map(|n| n["params"]["message"].as_str().unwrap().to_owned())
        .collect();
    let arquivo = amb.progresso("toolu_e2e");    assert_eq!(arquivo, notificados);
    assert_eq!(&arquivo[..3], ["conectando ao Windows", "lendo a tela", "decidindo o próximo passo"], "{arquivo:?}");
    assert_eq!(arquivo.iter().filter(|m| *m == "decidindo o próximo passo").count(), 2, "{arquivo:?}");
    assert_eq!(arquivo.last().unwrap(), "decidindo o próximo passo", "{arquivo:?}");
}
