//! MCP surface driven over stdio against the binary built with `motor-fake`.
#![cfg(feature = "motor-fake")]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_hangar-computer-control");

struct Ambiente {
    casa: tempfile::TempDir,
    alvos: tempfile::TempDir,
}

impl Ambiente {
    fn novo(arquivos: &[(&str, &str)]) -> Self {
        let amb = Self { casa: tempfile::tempdir().unwrap(), alvos: tempfile::tempdir().unwrap() };
        for (nome, conteudo) in arquivos {
            std::fs::write(amb.alvos.path().join(nome), conteudo).unwrap();
        }
        amb
    }

    fn comando(&self) -> Command {
        let mut c = Command::new(BIN);
        // Nothing from the developer's real environment (HCC_AGENT_CONFIG, Hyprland) leaks in.
        c.env_clear()
            .env("HOME", self.casa.path())
            .env("HCC_AGENTS_DIR", self.alvos.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    fn alvo(&self, nome: &str) -> PathBuf {
        self.alvos.path().join(nome)
    }
}

const VALIDO: &str = r#"{"transport": "local"}"#;

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
            for linha in BufReader::new(out).lines() {
                let Ok(linha) = linha else { break };
                let valor: Value = serde_json::from_str(&linha)
                    .unwrap_or_else(|e| panic!("stdout is not JSON-RPC ({e}): {linha}"));
                if tx.send(valor).is_err() {
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
        Self { filho, entrada, saida, erros, notificacoes: Vec::new() }
    }

    fn enviar(&mut self, msg: Value) {
        let e = self.entrada.as_mut().unwrap();
        writeln!(e, "{msg}").unwrap();
        e.flush().unwrap();
    }

    fn resposta(&mut self, id: u64) -> Value {
        let limite = Instant::now() + Duration::from_secs(10);
        loop {
            let resto = limite.saturating_duration_since(Instant::now());
            let msg = self
                .saida
                .recv_timeout(resto)
                .unwrap_or_else(|_| panic!("no response to id {id}; stderr: {}", self.stderr()));
            if msg["id"] == json!(id) {
                return msg;
            }
            self.notificacoes.push(msg);
        }
    }

    fn inicializar(&mut self) -> Value {
        self.enviar(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "teste", "version": "0"}}}));
        let r = self.resposta(1);
        self.enviar(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        r
    }

    fn chamar(&mut self, id: u64, nome: &str, args: Value, meta: Option<Value>) -> Value {
        let mut params = json!({"name": nome, "arguments": args});
        if let Some(m) = meta {
            params["_meta"] = m;
        }
        self.enviar(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params}));
        self.resposta(id)["result"].clone()
    }

    fn stderr(&self) -> String {
        self.erros.lock().unwrap().clone()
    }

    fn esperar_stderr(&self, trecho: &str, ate: Duration) -> bool {
        let limite = Instant::now() + ate;
        while Instant::now() < limite {
            if self.stderr().contains(trecho) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn esperar_saida(&mut self, ate: Duration) -> Option<ExitStatus> {
        let limite = Instant::now() + ate;
        while Instant::now() < limite {
            if let Some(s) = self.filho.try_wait().unwrap() {
                return Some(s);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

impl Drop for Servidor {
    fn drop(&mut self) {
        let _ = self.filho.kill();
        let _ = self.filho.wait();
    }
}

fn texto(resultado: &Value) -> String {
    resultado["content"][0]["text"].as_str().unwrap_or_default().to_owned()
}

fn ferramentas(s: &mut Servidor) -> Value {
    s.enviar(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    s.resposta(2)["result"].clone()
}

fn descricao_estado(s: &mut Servidor) -> String {
    let lista = ferramentas(s);
    lista["tools"][2]["description"].as_str().unwrap().to_owned()
}

#[test]
fn tools_list_matches_python_golden() {
    let golden: Value = serde_json::from_str(include_str!("golden/tools_list.json")).unwrap();
    let amb = Ambiente::novo(&[("vm-a-agent.json", VALIDO), ("vm-b-agent.json", VALIDO)]);
    let mut c = amb.comando();
    c.env("HCC_AGENT_CONFIG", amb.alvo("vm-a-agent.json"));
    let mut s = Servidor::iniciar(c);
    let init = s.inicializar()["result"].clone();
    assert_eq!(init["protocolVersion"], golden["initialize"]["protocolVersion"]);
    assert_eq!(init["serverInfo"]["name"], golden["initialize"]["serverInfo"]["name"]);
    assert_eq!(init["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(init["capabilities"]["tools"].is_object());
    assert_eq!(ferramentas(&mut s), golden["tools_list"]);
}

#[test]
fn unknown_target_error_carries_message() {
    let amb = Ambiente::novo(&[("vm-a-agent.json", VALIDO), ("vm-b-agent.json", VALIDO)]);
    let mut s = Servidor::iniciar(amb.comando());
    s.inicializar();
    for (id, ferramenta, args) in [
        (3, "estado", json!({"alvo": "x"})),
        (4, "ver_tela", json!({"alvo": "x"})),
        (5, "objetivo", json!({"texto": "abrir", "alvo": "x"})),
    ] {
        let r = s.chamar(id, ferramenta, args, None);
        assert_eq!(r["isError"], json!(true), "{ferramenta}: {r}");
        assert_eq!(texto(&r), "alvo desconhecido: x; disponíveis: vm-a, vm-b", "{ferramenta}");
    }
    let vazio = Ambiente::novo(&[]);
    let mut s = Servidor::iniciar(vazio.comando());
    s.inicializar();
    let r = s.chamar(3, "estado", json!({"alvo": "x"}), None);
    assert_eq!(texto(&r), "alvo desconhecido: x; disponíveis: nenhum");
}

#[test]
fn broken_agent_json_reported_and_server_keeps_serving() {
    let amb = Ambiente::novo(&[("vm-a-agent.json", VALIDO), ("vm-c-agent.json", "{quebrado")]);
    let mut s = Servidor::iniciar(amb.comando());
    s.inicializar();
    assert!(descricao_estado(&mut s).contains("Disponíveis: vm-a, vm-c;"));
    let r = s.chamar(3, "estado", json!({"alvo": "vm-c"}), None);
    let t = texto(&r);
    assert!(t.starts_with("indisponível: "), "{t}");
    assert!(t.contains("vm-c-agent.json"), "{t}");
    let r = s.chamar(4, "estado", json!({"alvo": "vm-a"}), None);
    assert_eq!(texto(&r), "disponível via acessibilidade, 1280x800");
    assert_eq!(r["structuredContent"], json!({"result": "disponível via acessibilidade, 1280x800"}));
}

#[test]
fn progress_written_for_valid_tool_use_id_only() {
    let amb = Ambiente::novo(&[("vm-a-agent.json", VALIDO)]);
    let mut s = Servidor::iniciar(amb.comando());
    s.inicializar();
    let r = s.chamar(
        3,
        "objetivo",
        json!({"texto": "abrir ação", "alvo": "vm-a"}),
        Some(json!({"claudecode/toolUseId": "toolu_abc", "progressToken": "p1"})),
    );
    assert!(texto(&r).starts_with("concluído: "), "{r}");
    let pasta = amb.casa.path().join(".hangar/tool-progress");
    let linhas = std::fs::read_to_string(pasta.join("toolu_abc.jsonl")).unwrap();
    let linha: Value = serde_json::from_str(linhas.lines().next().unwrap()).unwrap();
    assert_eq!(linha["message"], "lendo a tela");
    assert!(linha["t"].as_f64().unwrap() > 1.7e9);
    let progresso: Vec<_> =
        s.notificacoes.iter().filter(|n| n["method"] == "notifications/progress").collect();
    assert_eq!(progresso.len(), 1, "{:?}", s.notificacoes);
    assert_eq!(progresso[0]["params"]["progressToken"], "p1");
    assert_eq!(progresso[0]["params"]["message"], "lendo a tela");
    assert_eq!(progresso[0]["params"]["progress"], json!(1.0));

    for (id, invalido) in [(4, "../x"), (5, "toolu_"), (6, "toolu_a/b")] {
        s.chamar(
            id,
            "objetivo",
            json!({"texto": "abrir", "alvo": "vm-a"}),
            Some(json!({"claudecode/toolUseId": invalido})),
        );
    }
    let nomes: Vec<_> = std::fs::read_dir(&pasta)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(nomes, ["toolu_abc.jsonl"]);
    assert!(!amb.casa.path().join(".hangar/x.jsonl").exists());
}

#[test]
fn objective_inputs_and_defaults_reach_motor() {
    let amb = Ambiente::novo(&[("vm-a-agent.json", VALIDO)]);
    let mut c = amb.comando();
    c.env("HCC_AGENT_CONFIG", amb.alvo("vm-a-agent.json"));
    let mut s = Servidor::iniciar(c);
    s.inicializar();
    let r = s.chamar(3, "objetivo", json!({"texto": "digitar olá"}), None);
    let esperado = format!(
        "concluído: fake\ntexto=digitar olá; max_passos=12; dados={{}}; limite=240s; config={}",
        amb.alvo("vm-a-agent.json").display()
    );
    assert_eq!(texto(&r), esperado);
    assert_eq!(r["isError"], json!(false));
    assert_eq!(r["structuredContent"]["result"], json!(esperado));
    let r = s.chamar(
        4,
        "objetivo",
        json!({"texto": "t", "max_passos": 3, "dados": {"nome": "Zé"}, "limite_segundos": 1.5}),
        None,
    );
    assert!(texto(&r).contains("max_passos=3; dados={\"nome\":\"Zé\"}; limite=1.5s;"), "{r}");
    let r = s.chamar(5, "ver_tela", json!({}), None);
    assert_eq!(texto(&r), format!("{}; 0.00s", amb.alvo("vm-a-agent.json").display()));
}

#[test]
fn cancel_reaches_motor() {
    let amb = Ambiente::novo(&[("vm-a-agent.json", VALIDO)]);
    let mut s = Servidor::iniciar(amb.comando());
    s.inicializar();
    s.enviar(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "objetivo", "_meta": {"progressToken": "p"},
        "arguments": {"texto": "esperar cancelamento", "alvo": "vm-a"}}}));
    // The first progress notification proves the motor is running before the cancel.
    let inicio = s.saida.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(inicio["method"], "notifications/progress");
    s.enviar(json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": 3, "reason": "teste"}}));
    assert!(s.esperar_stderr("fake: cancelado", Duration::from_secs(5)), "{}", s.stderr());
    // The server keeps serving after the cancel.
    let r = s.chamar(4, "estado", json!({"alvo": "vm-a"}), None);
    assert_eq!(texto(&r), "disponível via acessibilidade, 1280x800");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_target_automatic_only_under_hyprland() {
    let amb = Ambiente::novo(&[("vm-a-agent.json", VALIDO)]);
    let auto = amb.casa.path().join(".cache/hangar-computer-control/linux-agent.v2.json");

    let mut c = amb.comando();
    c.env("HYPRLAND_INSTANCE_SIGNATURE", "x");
    let mut s = Servidor::iniciar(c);
    s.inicializar();
    let d = descricao_estado(&mut s);
    assert!(d.ends_with("Disponíveis: vm-a, linux; sem `alvo`, usa o padrão (linux)."), "{d}");
    let conteudo: Value = serde_json::from_str(&std::fs::read_to_string(&auto).unwrap()).unwrap();
    assert_eq!(
        conteudo,
        json!({"transport": "local", "request_timeout": 15, "command": [BIN, "agent"]})
    );
    let esperado = format!("{}; 0.00s", auto.display());
    assert_eq!(texto(&s.chamar(3, "ver_tela", json!({"alvo": "linux"}), None)), esperado);
    assert_eq!(texto(&s.chamar(4, "ver_tela", json!({}), None)), esperado);
    assert!(!amb.casa.path().join(".cache/hangar-computer-control/linux-agent.json").exists());
    drop(s);

    // A real linux-agent.json in the targets dir wins.
    std::fs::write(amb.alvo("linux-agent.json"), VALIDO).unwrap();
    let mut c = amb.comando();
    c.env("HYPRLAND_INSTANCE_SIGNATURE", "x");
    let mut s = Servidor::iniciar(c);
    s.inicializar();
    assert!(descricao_estado(&mut s).contains("Disponíveis: linux, vm-a;"));
    let r = s.chamar(3, "ver_tela", json!({"alvo": "linux"}), None);
    assert_eq!(texto(&r), format!("{}; 0.00s", amb.alvo("linux-agent.json").display()));
    drop(s);
    std::fs::remove_file(amb.alvo("linux-agent.json")).unwrap();

    // No Hyprland → no automatic target.
    let mut s = Servidor::iniciar(amb.comando());
    s.inicializar();
    let d = descricao_estado(&mut s);
    assert!(d.ends_with("Disponíveis: vm-a; sem `alvo`, usa o padrão (nenhum)."), "{d}");
    let r = s.chamar(3, "estado", json!({"alvo": "linux"}), None);
    assert_eq!(texto(&r), "alvo desconhecido: linux; disponíveis: vm-a");
    drop(s);

    // Write failure → one stderr line, the server still starts with the other targets.
    let ruim = Ambiente::novo(&[("vm-a-agent.json", VALIDO)]);
    std::fs::write(ruim.casa.path().join(".cache"), "não é pasta").unwrap();
    let mut c = ruim.comando();
    c.env("HYPRLAND_INSTANCE_SIGNATURE", "x");
    let mut s = Servidor::iniciar(c);
    s.inicializar();
    let d = descricao_estado(&mut s);
    assert!(d.ends_with("Disponíveis: vm-a; sem `alvo`, usa o padrão (nenhum)."), "{d}");
    let linha = "alvo linux indisponível: não gravei ";
    assert!(s.esperar_stderr(linha, Duration::from_secs(2)), "{}", s.stderr());
}

#[test]
fn stdin_eof_exits_0() {
    let amb = Ambiente::novo(&[("vm-a-agent.json", VALIDO)]);
    let mut s = Servidor::iniciar(amb.comando());
    s.inicializar();
    s.entrada = None;
    let status = s.esperar_saida(Duration::from_secs(3)).expect("server did not exit on EOF");
    assert_eq!(status.code(), Some(0), "{}", s.stderr());
}

#[test]
fn stdin_eof_mid_objective_cancels_it_and_exits_0() {
    let amb = Ambiente::novo(&[("vm-a-agent.json", VALIDO)]);
    let mut s = Servidor::iniciar(amb.comando());
    s.inicializar();
    s.enviar(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "objetivo", "_meta": {"progressToken": "p"},
        "arguments": {"texto": "esperar cancelamento", "alvo": "vm-a"}}}));
    s.saida.recv_timeout(Duration::from_secs(10)).unwrap();
    s.entrada = None;
    let status = s.esperar_saida(Duration::from_secs(6)).expect("server did not exit on EOF");
    assert_eq!(status.code(), Some(0), "{}", s.stderr());
    assert!(s.esperar_stderr("fake: cancelado", Duration::from_secs(2)), "{}", s.stderr());
}

#[test]
fn stdin_eof_still_delivers_the_cancelled_objective_response() {
    let amb = Ambiente::novo(&[("vm-a-agent.json", VALIDO)]);
    let mut s = Servidor::iniciar(amb.comando());
    s.inicializar();
    s.enviar(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "objetivo", "_meta": {"progressToken": "p"},
        "arguments": {"texto": "esperar cancelamento", "alvo": "vm-a"}}}));
    s.saida.recv_timeout(Duration::from_secs(10)).unwrap();
    s.entrada = None;
    assert_eq!(texto(&s.resposta(3)["result"]), "parou: cancelado");
}

#[test]
fn stdin_eof_with_motor_ignoring_cancel_exits_within_5s() {
    let amb = Ambiente::novo(&[("vm-a-agent.json", VALIDO)]);
    let mut s = Servidor::iniciar(amb.comando());
    s.inicializar();
    s.enviar(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "objetivo", "_meta": {"progressToken": "p"},
        "arguments": {"texto": "ignorar cancelamento", "alvo": "vm-a", "limite_segundos": 30}}}));
    s.saida.recv_timeout(Duration::from_secs(10)).unwrap();
    let inicio = Instant::now();
    s.entrada = None;
    let status = s.esperar_saida(Duration::from_secs(20)).expect("server did not exit on EOF");
    let gasto = inicio.elapsed();
    assert_eq!(status.code(), Some(0), "{}", s.stderr());
    assert!(gasto < Duration::from_millis(6500), "EOF -> exit took {gasto:?}");
}

#[cfg(unix)]
#[test]
fn sigterm_mid_objective_cancels_it_and_exits_0() {
    let amb = Ambiente::novo(&[("vm-a-agent.json", VALIDO)]);
    let mut s = Servidor::iniciar(amb.comando());
    s.inicializar();
    s.enviar(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "objetivo", "_meta": {"progressToken": "p"},
        "arguments": {"texto": "esperar cancelamento", "alvo": "vm-a"}}}));
    s.saida.recv_timeout(Duration::from_secs(10)).unwrap();
    let kill = Command::new("kill").args(["-TERM", &s.filho.id().to_string()]).status().unwrap();
    assert!(kill.success());
    let status = s.esperar_saida(Duration::from_secs(6)).expect("server did not exit on SIGTERM");
    assert_eq!(status.code(), Some(0), "{}", s.stderr());
    assert!(s.esperar_stderr("fake: cancelado", Duration::from_secs(2)), "{}", s.stderr());
}

#[test]
fn startup_under_500ms() {
    let amb = Ambiente::novo(&[("vm-a-agent.json", VALIDO)]);
    let inicio = Instant::now();
    let mut s = Servidor::iniciar(amb.comando());
    s.inicializar();
    let gasto = inicio.elapsed();
    assert!(gasto < Duration::from_millis(500), "initialize took {gasto:?}");
}
