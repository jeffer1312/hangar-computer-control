#![cfg(target_os = "linux")]

pub mod arvore;
pub mod comandos;
pub mod entrada;
pub mod hypr;

use arvore::{Arvore, AtspiTree, INVOKE, ITEMS, NoInfo, TreeError, choose_frame, element_rect, role_name, walk};
use comandos::{Comandos, ComandosSistema};
use hcc_protocolo::{Action, ActionType, Button, Desktop, DesktopError, ErrorKind, MouseMode, Observation, Rect, Screen, Window, dividir_comando};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
struct ObservedElement<N> { node: N, name: String, role: String }

pub struct LinuxDesktop<C: Comandos = ComandosSistema, T: Arvore = AtspiTree> {
    commands: C,
    tree: T,
    session_id: u32,
    _lock: Option<File>,
    elements: HashMap<String, ObservedElement<T::Node>>,
    windows: HashMap<String, Value>,
    observed_at: Option<Instant>,
    foreground: String,
    area: Rect,
    apps: Vec<String>,
}

impl LinuxDesktop {
    pub fn new() -> Result<Self, String> {
        let tree = AtspiTree::new()?;
        let commands = ComandosSistema::new()?;
        let missing: Vec<_> = ["hyprctl", "grim", "ydotool", "wtype", "pgrep"].into_iter().filter(|p| !commands.exists(p)).collect();
        if !missing.is_empty() { return Err(format!("faltam programas no sistema: {}", missing.join(", "))); }
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none_or(|s| s.is_empty()) || std::env::var_os("WAYLAND_DISPLAY").is_none_or(|s| s.is_empty()) {
            return Err("o agente deve rodar dentro da sessão Hyprland do usuário".into());
        }
        let status = std::fs::read_to_string("/proc/self/status").map_err(|e| e.to_string())?;
        let uid = status.lines().find_map(|line| line.strip_prefix("Uid:").and_then(|s| s.split_whitespace().next()).and_then(|s| s.parse::<u32>().ok()))
            .ok_or("não foi possível determinar o usuário da sessão")?;
        if uid == 0 { return Err("o agente não deve rodar como root".into()); }
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
        let lock = OpenOptions::new().create(true).append(true).open(runtime.join("hangar-computer-control-agent.lock")).map_err(|e| e.to_string())?;
        lock.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => "outro agente já controla esta sessão".into(),
            std::fs::TryLockError::Error(e) => e.to_string(),
        })?;
        let mut desktop = Self::with_backends(commands, tree, uid);
        desktop._lock = Some(lock);
        Ok(desktop)
    }
}

fn runtime(message: impl Into<String>) -> DesktopError { DesktopError { kind: ErrorKind::RuntimeError, msg: message.into() } }

pub(crate) fn ignorado(onde: &str, error: &impl std::fmt::Display) {
    let detail = error.to_string().replace('\r', "\\r").replace('\n', "\\n");
    eprintln!("hcc-agente-linux: ignorado {onde}: {detail}");
}

fn invalid(message: impl Into<String>) -> DesktopError { DesktopError { kind: ErrorKind::ValueError, msg: message.into() } }
fn tree_error(error: TreeError) -> DesktopError { runtime(error.to_string()) }

fn foreground_id(active: &Value) -> String {
    active["address"].as_str().filter(|a| !a.is_empty()).map(|a| format!("w{a}")).unwrap_or_else(|| "desktop".into())
}

fn python_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut result = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => result.push_str("\\\\"), '\n' => result.push_str("\\n"), '\r' => result.push_str("\\r"), '\t' => result.push_str("\\t"),
            c if c == quote => { result.push('\\'); result.push(c); }
            c if c.is_control() => result.push_str(&format!("\\x{:02x}", u32::from(c))),
            c => result.push(c),
        }
    }
    result.push(quote);
    result
}

fn desktop_value(value: &str) -> Option<String> {
    let mut chars = value.chars();
    let mut text = String::new();
    while let Some(c) = chars.next() {
        text.push(if c == '\\' {
            match chars.next()? { 's' => ' ', 'n' => '\n', 't' => '\t', 'r' => '\r', '\\' => '\\', _ => return None }
        } else { c });
    }
    Some(text)
}

fn exec_tokens(exec: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    for token in dividir_comando(exec)? {
        let mut chars = token.chars();
        let mut clean = String::new();
        while let Some(c) = chars.next() {
            if c == '%' { if chars.next()? == '%' { clean.push('%'); } }
            else { clean.push(c); }
        }
        if !clean.is_empty() { tokens.push(clean); }
    }
    if tokens.first().is_none_or(|t| t.contains('=')) { return None; }
    Some(tokens)
}

fn desktop_app(text: &str) -> Option<String> {
    let (mut name, mut exec, mut entry, mut hidden) = (None, None, false, false);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') { entry = line == "[Desktop Entry]"; continue; }
        if !entry || line.starts_with('#') { continue; }
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "Name" => name = Some(value.trim()),
                "Exec" => exec = Some(value.trim()),
                "NoDisplay" | "Hidden" if value.trim() == "true" => hidden = true,
                _ => {},
            }
        }
    }
    if hidden { return None; }
    let name = desktop_value(name?)?.replace(" => ", " -> ");
    if name.is_empty() { return None; }
    let command = exec_tokens(&desktop_value(exec?)?)?.into_iter().map(|token| {
        if token == "=>" || token.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '`' | '$' | '\\')) {
            let mut escaped = String::new();
            for c in token.chars() {
                if matches!(c, '"' | '`' | '$' | '\\') { escaped.push('\\'); }
                escaped.push(c);
            }
            format!("\"{escaped}\"")
        } else { token }
    }).collect::<Vec<_>>().join(" ");
    Some(format!("{name} => {command}"))
}

fn installed_apps() -> Vec<String> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("XDG_DATA_HOME").filter(|p| !p.is_empty()).map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share"))) {
        dirs.push(home);
    }
    let system = std::env::var_os("XDG_DATA_DIRS").filter(|p| !p.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(std::env::split_paths(&system));
    let mut apps = BTreeSet::new();
    let mut seen = HashSet::new();
    for dir in dirs {
        let entries = match std::fs::read_dir(dir.join("applications")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => { ignorado("installed_apps.directory", &error); continue; }
        };
        for entry in entries {
            let entry = match entry { Ok(entry) => entry, Err(error) => { ignorado("installed_apps.entry", &error); continue; } };
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "desktop") || !seen.insert(entry.file_name()) { continue; }
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text, Err(error) => { ignorado("installed_apps.file", &error); continue; }
            };
            if let Some(app) = desktop_app(&text) { apps.insert(app); }
        }
    }
    apps.into_iter().take(150).collect()
}

impl<C: Comandos, T: Arvore> LinuxDesktop<C, T> {
    pub fn with_backends(commands: C, tree: T, session_id: u32) -> Self {
        Self { commands, tree, session_id, _lock: None, elements: HashMap::new(), windows: HashMap::new(),
            observed_at: None, foreground: String::new(), area: Rect(0, 0, 0, 0), apps: installed_apps() }
    }

    fn current(&self) -> Result<(Value, Rect, Value), DesktopError> {
        let active = hypr::query(&self.commands, "activewindow").map_err(runtime)?;
        let monitors = hypr::query(&self.commands, "monitors").map_err(runtime)?;
        let monitor = hypr::reference_monitor(&monitors, &active).map_err(runtime)?;
        Ok((active, hypr::monitor_logico(monitor).map_err(runtime)?, monitor.clone()))
    }

    fn roots(&self, client: &Value) -> Result<Vec<T::Node>, DesktopError> {
        let pid = client["pid"].as_u64().and_then(|p| u32::try_from(p).ok()).ok_or_else(|| runtime("hyprctl retornou pid inválido"))?;
        let mut tops = vec![];
        for app in self.tree.apps().map_err(tree_error)? {
            match self.tree.pid(&app) {
                Ok(Some(p)) if p == pid => tops.extend(self.tree.children(&app).map_err(tree_error)?),
                Ok(_) => {}, Err(error) => ignorado("roots.pid", &error),
            }
        }
        let mut frames = vec![];
        let mut popups = vec![];
        for top in tops {
            let info = match self.tree.read(&top) {
                Ok(info) => info, Err(error @ TreeError::Vanished(_)) => { ignorado("roots.read", &error); continue; }, Err(error) => return Err(tree_error(error)),
            };
            if info.role == "frame" || info.role == "dialog" { frames.push((top.clone(), info.clone())); }
            if ["window", "popup menu", "menu", "tool tip"].contains(&info.role.as_str()) && info.has("showing") { popups.push(top); }
        }
        let descriptions: Vec<_> = frames.iter().map(|(_, info)| (info.name(), info.states.clone())).collect();
        let Some(chosen) = choose_frame(&descriptions, hypr::string(client, "title").map_err(runtime)?) else { return Ok(vec![]); };
        let mut roots = vec![];
        for top in std::iter::once(&frames[chosen].0).chain(popups.iter()) { roots.extend(self.tree.children(top).map_err(tree_error)?); }
        Ok(roots)
    }

    fn validate_observation(&mut self, observed: &Observation) -> Result<(), DesktopError> {
        self.available()?;
        if self.observed_at.is_none_or(|at| at.elapsed() > Duration::from_secs(120)) || observed.foreground != self.foreground {
            return Err(runtime("observação expirada; observe novamente"));
        }
        if self.foreground()? != self.foreground { return Err(runtime("o foco mudou; observe novamente antes de agir")); }
        Ok(())
    }

    fn point(&self, x: Option<i32>, y: Option<i32>) -> Result<(), DesktopError> {
        let (x, y) = match (x, y) { (Some(x), Some(y)) => entrada::para_global(self.area, x, y).map_err(invalid)?, _ => return Err(invalid("coordenada fora da tela")) };
        hypr::dispatch(&self.commands, &format!("hl.dsp.cursor.move({{ x = {x}, y = {y} }})")).map_err(runtime)
    }

    fn mouse(&self, action: &Action) -> Result<(), DesktopError> {
        let code = match action.button.unwrap_or(Button::Left) { Button::Left => 0, Button::Right => 1, Button::Middle => 2 };
        let mode = action.mode.unwrap_or(MouseMode::Click);
        let delta = action.delta.unwrap_or(0);
        if mode == MouseMode::Scroll && !(-50..=50).contains(&delta) { return Err(invalid("delta inválido")); }
        self.point(action.x, action.y)?;
        match mode {
            MouseMode::Click => { self.commands.run(&["ydotool", "click", &format!("{:#x}", 0xc0 | code)], None).map_err(runtime)?; }
            MouseMode::Double => { self.commands.run(&["ydotool", "click", "--repeat", "2", "--next-delay", "60", &format!("{:#x}", 0xc0 | code)], None).map_err(runtime)?; }
            MouseMode::Move => {},
            MouseMode::Scroll => { self.commands.run(&["ydotool", "mousemove", "--wheel", "-x", "0", "-y", &(-delta).to_string()], None).map_err(runtime)?; }
            MouseMode::Drag => {
                let moved = self.commands.run(&["ydotool", "click", &format!("{:#x}", 0x40 | code)], None).map_err(runtime).and_then(|_| {
                    std::thread::sleep(Duration::from_millis(100));
                    self.point(action.x2, action.y2)
                });
                if moved.is_ok() { std::thread::sleep(Duration::from_millis(100)); }
                let released = self.commands.run(&["ydotool", "click", &format!("{:#x}", 0x80 | code)], None).map_err(runtime);
                match (moved, released) {
                    (Err(error), Err(release)) => { ignorado("mouse.release", &release); return Err(error); },
                    (Err(error), Ok(_)) | (Ok(_), Err(error)) => return Err(error), (Ok(_), Ok(_)) => {},
                }
            }
        }
        Ok(())
    }

    fn type_text(&self, value: Option<&str>, message: &str) -> Result<(), DesktopError> {
        let value = value.filter(|v| v.chars().count() <= 20000).ok_or_else(|| invalid(message))?;
        self.commands.run(&["wtype", "-"], Some(value.as_bytes())).map_err(runtime)?;
        Ok(())
    }

    fn focus_field(&self, element: &ObservedElement<T::Node>) -> Result<(), DesktopError> {
        match self.tree.focus(&element.node) {
            Ok(true) => return Ok(()), Ok(false) => {}, Err(error) => ignorado("focus_field.focus", &error),
        }
        let info = self.tree.read(&element.node).map_err(tree_error)?;
        if info.name() != element.name || role_name(&info.role, &info.states) != element.role || (!info.has("enabled") && !info.has("sensitive")) {
            return Err(runtime("controle mudou; observe novamente"));
        }
        let active = hypr::query(&self.commands, "activewindow").map_err(runtime)?;
        let window = hypr::client_rect(&active, (self.area.0, self.area.1)).map_err(runtime)?;
        let rect = info.extents.and_then(|extents| element_rect(extents, window)).ok_or_else(|| runtime("aplicativo recusou o foco e o campo não tem posição na tela"))?;
        let (x, y) = center(rect);
        self.point(Some(x), Some(y))?;
        self.commands.run(&["ydotool", "click", "0xc0"], None).map_err(runtime)?;
        Ok(())
    }

    fn do_action(&self, node: &T::Node, info: &NoInfo, wanted: &[&str]) -> Result<(), DesktopError> {
        let names: Vec<String> = info.actions.iter().map(|n| n.to_lowercase()).collect();
        let index = names.iter().position(|n| wanted.contains(&n.as_str())).ok_or_else(|| runtime(format!("controle sem ação {}: [{}]", wanted[0], names.iter().map(|s| python_repr(s)).collect::<Vec<_>>().join(", "))))?;
        if !self.tree.invoke(node, index).map_err(tree_error)? { return Err(runtime(format!("aplicativo recusou a ação {}", names[index]))); }
        Ok(())
    }

    fn set_value(&self, element: &ObservedElement<T::Node>, info: &NoInfo, value: Option<&str>) -> Result<(), DesktopError> {
        let value = value.filter(|v| v.chars().count() <= 20000).ok_or_else(|| invalid("valor inválido"))?;
        self.focus_field(element)?;
        std::thread::sleep(Duration::from_millis(150));
        if info.text.is_none() && !self.tree.read(&element.node).map_err(tree_error)?.has("focused") {
            return Err(runtime("campo não recebeu o foco; nada foi digitado"));
        }
        entrada::keys(&self.commands, &["ctrl".into(), "a".into()]).map_err(runtime)?;
        entrada::keys(&self.commands, &["delete".into()]).map_err(runtime)?;
        self.type_text(Some(value), "texto inválido")?;
        std::thread::sleep(Duration::from_millis(150));
        if info.role != "password text" && info.text.is_some() {
            let readback = self.tree.text(&element.node).map_err(tree_error)?;
            if readback.replace("\r\n", "\n").trim() != value.replace("\r\n", "\n").trim() {
                return Err(runtime(format!("valor digitado não confirmado; campo ficou com {}", python_repr(&readback.chars().take(80).collect::<String>()))));
            }
        }
        Ok(())
    }

    fn element_action(&self, action: &Action) -> Result<(), DesktopError> {
        let element = action.target.as_ref().and_then(|t| self.elements.get(t)).ok_or_else(|| invalid("controle não pertence à observação"))?;
        let info = self.tree.read(&element.node).map_err(tree_error)?;
        if info.name() != element.name || role_name(&info.role, &info.states) != element.role || (!info.has("enabled") && !info.has("sensitive")) {
            return Err(runtime("controle mudou; observe novamente"));
        }
        let active = hypr::query(&self.commands, "activewindow").map_err(runtime)?;
        let window = hypr::client_rect(&active, (self.area.0, self.area.1)).map_err(runtime)?;
        let rect = info.extents.and_then(|extents| element_rect(extents, window));
        let on_screen = rect.map(center).filter(|(x, y)| window.0 <= *x && *x < window.2 && window.1 <= *y && *y < window.3);
        if let Some((x, y)) = on_screen.filter(|_| (action.kind == ActionType::Invoke && !ITEMS.contains(&element.role.as_str())) || matches!(action.kind, ActionType::Select | ActionType::Toggle)) {
            return self.mouse(&Action { kind: ActionType::Mouse, x: Some(x), y: Some(y), ..Default::default() });
        }
        match action.kind {
            ActionType::Invoke => self.do_action(&element.node, &info, INVOKE),
            ActionType::Select | ActionType::Toggle => self.do_action(&element.node, &info, &[&["toggle"], INVOKE].concat()),
            ActionType::Expand | ActionType::Collapse => self.do_action(&element.node, &info, &[&["expand or contract", action.kind.as_str(), "expand or collapse"], INVOKE].concat()),
            ActionType::Focus => {
                if self.tree.focus(&element.node).map_err(tree_error)? { Ok(()) } else { Err(runtime("aplicativo recusou o foco")) }
            }
            ActionType::SetValue => self.set_value(element, &info, action.value.as_deref()),
            ActionType::Scroll => Err(invalid("tipo de ação desconhecido: scroll")),
            ActionType::Activate | ActionType::Launch | ActionType::Keys | ActionType::Text | ActionType::Mouse => Err(invalid(format!("tipo de ação desconhecido: {}", action.kind))),
        }
    }
}

fn center(rect: Rect) -> (i32, i32) {
    (((i64::from(rect.0) + i64::from(rect.2)).div_euclid(2)) as i32, ((i64::from(rect.1) + i64::from(rect.3)).div_euclid(2)) as i32)
}

impl<C: Comandos, T: Arvore> Desktop for LinuxDesktop<C, T> {
    fn session_id(&self) -> u32 { self.session_id }

    fn available(&mut self) -> Result<(), DesktopError> {
        match self.commands.run(&["pgrep", "-x", "hyprlock"], None) {
            Ok(_) => Err(runtime("tela bloqueada")),
            Err(error) if error.starts_with("pgrep falhou (1):") => Ok(()),
            Err(error) => {
                if let Some(rc) = error.strip_prefix("pgrep falhou (").and_then(|s| s.split_once(')')).map(|(rc, _)| rc) {
                    Err(runtime(format!("pgrep falhou ({rc})")))
                } else { Err(runtime(error)) }
            }
        }
    }

    fn foreground(&mut self) -> Result<String, DesktopError> { Ok(foreground_id(&hypr::query(&self.commands, "activewindow").map_err(runtime)?)) }

    fn observe(&mut self) -> Result<Observation, DesktopError> {
        self.available()?;
        self.observed_at = None; self.elements.clear(); self.windows.clear();
        let (active, area, _) = self.current()?;
        let clients = hypr::query(&self.commands, "clients").map_err(runtime)?;
        let mut windows = hypr::windows_from(&clients, (area.0, area.1)).map_err(runtime)?;
        for client in clients.as_array().ok_or_else(|| runtime("hyprctl retornou clients inválido"))? {
            if client["mapped"].as_bool().unwrap_or(false) && !client["hidden"].as_bool().unwrap_or(false) {
                self.windows.insert(format!("w{}", hypr::string(client, "address").map_err(runtime)?), client.clone());
            }
        }
        self.area = area;
        self.foreground = foreground_id(&active);
        let screen = Screen { width: (area.2 - area.0) as u32, height: (area.3 - area.1) as u32 };
        let (elements, truncated) = if self.foreground != "desktop" {
            let window = windows.iter().find(|w| w.id == self.foreground).ok_or_else(|| runtime("a janela ativa mudou durante a observação"))?;
            let (elements, nodes, cut) = walk(&self.tree, self.roots(&active)?, window.rect, Duration::from_secs(4), 800).map_err(runtime)?;
            for (element, node) in elements.iter().zip(nodes) {
                self.elements.insert(element.id.clone(), ObservedElement { node, name: element.name.clone(), role: element.role.clone() });
            }
            (elements, cut)
        } else {
            windows.push(Window { id: "desktop".into(), name: "Área de trabalho".into(), process_id: 0, class_name: "desktop".into(), rect: Rect(0, 0, screen.width as i32, screen.height as i32) });
            (vec![], false)
        };
        if self.foreground()? != self.foreground { return Err(runtime("a janela ativa mudou durante a observação; tente novamente")); }
        self.observed_at = Some(Instant::now());
        Ok(Observation { observation_id: String::new(), connected: true, session_id: self.session_id,
            foreground: self.foreground.clone(), windows, elements, apps: self.apps.clone(), truncated,
            timestamp: SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| runtime(e.to_string()))?.as_secs_f64(), screen })
    }

    fn act(&mut self, observed: &Observation, action: &Action) -> Result<(), DesktopError> {
        self.validate_observation(observed)?;
        self.observed_at = None;
        match action.kind {
            ActionType::Activate => {
                let window = action.target.as_ref().and_then(|t| self.windows.get(t)).ok_or_else(|| invalid("janela não pertence à observação"))?;
                hypr::dispatch(&self.commands, &format!("hl.dsp.focus({{ window = {} }})", entrada::lua_string(&format!("address:{}", hypr::string(window, "address").map_err(runtime)?)))).map_err(runtime)
            }
            ActionType::Launch => {
                let app = action.application.as_deref().filter(|a| !a.is_empty()).ok_or_else(|| invalid("application e args inválidos"))?;
                if !self.commands.exists(app) { return Err(invalid(format!("programa não encontrado: {app}"))); }
                let parts: Vec<&str> = std::iter::once(app).chain(action.args.iter().flatten().map(String::as_str)).collect();
                hypr::dispatch(&self.commands, &format!("hl.dsp.exec_cmd({})", entrada::lua_string(&entrada::shell_join(&parts)))).map_err(runtime)
            }
            ActionType::Keys => {
                let names = action.keys.as_deref().ok_or_else(|| invalid("keys precisa conter de 1 a 8 teclas"))?;
                entrada::evdev(names).map_err(invalid)?;
                entrada::keys(&self.commands, names).map_err(runtime)
            }
            ActionType::Text => {
                if let Some(target) = &action.target {
                    let element = self.elements.get(target).ok_or_else(|| invalid("controle não pertence à observação"))?;
                    self.focus_field(element)?; std::thread::sleep(Duration::from_millis(150));
                }
                self.type_text(action.value.as_deref(), "texto inválido")
            }
            ActionType::Mouse => self.mouse(action),
            ActionType::Invoke | ActionType::SetValue | ActionType::Select | ActionType::Toggle | ActionType::Expand | ActionType::Collapse | ActionType::Focus | ActionType::Scroll => self.element_action(action),
        }
    }

    fn screenshot_png(&mut self) -> Result<Vec<u8>, DesktopError> {
        self.available()?;
        let (_, area, monitor) = self.current()?;
        let width = area.2 - area.0;
        let scale = f64::from(width.min(1280)) / f64::from(width);
        let scale = if scale.fract() == 0.0 { format!("{scale:.1}") } else { scale.to_string() };
        self.commands.run(&["grim", "-o", hypr::string(&monitor, "name").map_err(runtime)?, "-s", &scale, "-"], None).map_err(runtime)
    }
}
