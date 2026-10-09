//! Windows UI Automation desktop agent (port of windows_uia.py).
//!
//! `regras` is pure and builds everywhere so its tests run on Linux; everything touching
//! COM/Win32 is `cfg(windows)`.

#[cfg(windows)]
mod arvore;
#[cfg(windows)]
mod captura;
#[cfg(windows)]
mod entrada;
#[cfg(windows)]
mod sessao_win;

#[cfg(windows)]
pub use desktop::WindowsDesktop;

pub mod regras {
    use hcc_protocolo::{Action, Amount, Button, DesktopError, Direction, ErrorKind, MouseMode};
    use image::ImageEncoder;

    pub(crate) fn valor_err(msg: impl Into<String>) -> DesktopError {
        DesktopError { kind: ErrorKind::ValueError, msg: msg.into() }
    }

    pub(crate) fn runtime_err(msg: impl Into<String>) -> DesktopError {
        DesktopError { kind: ErrorKind::RuntimeError, msg: msg.into() }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Tecla {
        /// pywinauto key code (`VK_CONTROL`, `ENTER`, `F5`, `s`): it names the key in errors.
        pub nome: String,
        pub vk: u16,
        /// Needs `KEYEVENTF_EXTENDEDKEY`, else arrows/Home/Delete arrive as numeric-keypad keys.
        pub estendida: bool,
    }

    const ALIASES: &[(&str, &str, u16)] = &[
        ("ctrl", "VK_CONTROL", 0x11),
        ("control", "VK_CONTROL", 0x11),
        ("alt", "VK_MENU", 0x12),
        ("shift", "VK_SHIFT", 0x10),
        ("win", "VK_LWIN", 0x5B),
        ("meta", "VK_LWIN", 0x5B),
        ("enter", "ENTER", 0x0D),
        ("escape", "ESC", 0x1B),
        ("tab", "TAB", 0x09),
        ("space", "SPACE", 0x20),
        ("backspace", "BACKSPACE", 0x08),
        ("delete", "DELETE", 0x2E),
        ("home", "HOME", 0x24),
        ("end", "END", 0x23),
        ("pageup", "PGUP", 0x21),
        ("pagedown", "PGDN", 0x22),
        ("arrowleft", "LEFT", 0x25),
        ("arrowright", "RIGHT", 0x27),
        ("arrowup", "UP", 0x26),
        ("arrowdown", "DOWN", 0x28),
        ("insert", "INSERT", 0x2D),
    ];

    fn estendida(vk: u16) -> bool {
        matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x5B)
    }

    fn tecla(nome: &str) -> Result<Tecla, DesktopError> {
        let lower = nome.to_lowercase();
        let (codigo, vk) = if let Some((_, codigo, vk)) = ALIASES.iter().find(|(a, _, _)| *a == lower) {
            (codigo.to_string(), *vk)
        } else if let Some(n) = lower.strip_prefix('f').filter(|n| !n.starts_with('0') && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).and_then(|n| n.parse::<u16>().ok()).filter(|n| (1..=24).contains(n)) {
            (format!("F{n}"), 0x6F + n)
        } else if let [c] = nome.as_bytes()
            && c.is_ascii_alphanumeric()
        {
            let c = c.to_ascii_lowercase();
            ((c as char).to_string(), c.to_ascii_uppercase() as u16)
        } else {
            return Err(valor_err(format!("tecla desconhecida: {nome}")));
        };
        Ok(Tecla { nome: codigo, vk, estendida: estendida(vk) })
    }

    /// Validates every name before anything is pressed (windows_uia.py:201-227).
    pub fn teclas(nomes: Option<&[String]>) -> Result<Vec<Tecla>, DesktopError> {
        match nomes {
            Some(n) if (1..=8).contains(&n.len()) => n.iter().map(|k| tecla(k)).collect(),
            _ => Err(valor_err("keys precisa conter de 1 a 8 teclas")),
        }
    }

    struct Soltar<'a, F: FnMut(&Tecla, bool) -> Result<(), DesktopError>> {
        apertadas: Vec<&'a Tecla>,
        enviar: F,
    }

    impl<F: FnMut(&Tecla, bool) -> Result<(), DesktopError>> Soltar<'_, F> {
        fn soltar(&mut self) -> Vec<String> {
            let mut erros = Vec::new();
            while let Some(t) = self.apertadas.pop() {
                if let Err(e) = (self.enviar)(t, false) {
                    erros.push(format!("{}: {}", t.nome, e.msg));
                }
            }
            erros
        }
    }

    // Releases even if `enviar` panics mid-chord, so no modifier stays down.
    impl<F: FnMut(&Tecla, bool) -> Result<(), DesktopError>> Drop for Soltar<'_, F> {
        fn drop(&mut self) {
            self.soltar();
        }
    }

    /// Press in order, release in reverse even on failure (windows_uia.py:228-241).
    pub fn acorde(teclas: &[Tecla], enviar: impl FnMut(&Tecla, bool) -> Result<(), DesktopError>) -> Result<(), DesktopError> {
        let mut guarda = Soltar { apertadas: Vec::new(), enviar };
        let mut resultado = Ok(());
        for t in teclas {
            guarda.apertadas.push(t);
            if let Err(e) = (guarda.enviar)(t, true) {
                resultado = Err(e);
                break;
            }
        }
        let erros = guarda.soltar();
        if !erros.is_empty() {
            return Err(runtime_err(format!("falha ao liberar teclas: {}", erros.join("; "))));
        }
        resultado
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Toque {
        /// One UTF-16 unit through `KEYEVENTF_UNICODE`.
        Unicode(u16),
        /// A virtual key (Enter for line breaks).
        Tecla(u16),
    }

    /// Text as keystrokes. Unicode packets are taken literally, so pywinauto's brace escaping of
    /// `+^%~(){}` has no counterpart here.
    pub fn texto(valor: &str) -> Vec<Toque> {
        let mut toques = Vec::new();
        let mut chars = valor.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\r' if chars.peek() == Some(&'\n') => {}
                '\r' | '\n' => toques.push(Toque::Tecla(0x0D)),
                // pywinauto drops tabs when with_tabs is off, as windows_uia.py calls it.
                '\t' => {}
                _ => toques.extend(c.encode_utf16(&mut [0; 2]).iter().map(|u| Toque::Unicode(*u))),
            }
        }
        toques
    }

    /// `invoke` clicks for real when on screen, except list/tree/data items where a click only
    /// selects and Invoke opens (windows_uia.py:311-317).
    pub fn deve_clicar(role: &str, na_tela: bool) -> bool {
        na_tela && !matches!(role, "ListItem" | "TreeItem" | "DataItem")
    }

    pub fn na_tela(rect: [i32; 4], offscreen: bool) -> bool {
        rect[2] - rect[0] > 0 && rect[3] - rect[1] > 0 && !offscreen
    }

    /// A visible window other than the foreground joins the tree when it is enabled (not behind
    /// a modal), of the same process, and owned or a Win32 menu (windows_uia.py:108-118).
    pub fn e_popup(tem_dono: bool, classe: &str, habilitada: bool, mesmo_pid: bool) -> bool {
        habilitada && mesmo_pid && (tem_dono || classe == "#32768")
    }

    /// UIA ScrollAmount `(horizontal, vertical)`: large/small decrement 0/1, none 2, large/small increment 3/4.
    pub fn quantidade_scroll(direcao: Direction, quanto: Amount) -> (i32, i32) {
        let valor = match (direcao, quanto) {
            (Direction::Up | Direction::Left, Amount::Large) => 0,
            (Direction::Up | Direction::Left, Amount::Small) => 1,
            (Direction::Down | Direction::Right, Amount::Large) => 3,
            (Direction::Down | Direction::Right, Amount::Small) => 4,
        };
        match direcao {
            Direction::Left | Direction::Right => (valor, 2),
            Direction::Up | Direction::Down => (2, valor),
        }
    }

    pub fn valor_confirmado(lido: &str, esperado: &str) -> bool {
        lido.replace("\r\n", "\n").trim() == esperado.replace("\r\n", "\n").trim()
    }

    pub fn valor_nao_confirmado(lido: &str) -> DesktopError {
        runtime_err(format!("valor digitado não confirmado; campo ficou com {}", repr_python(&corta(lido, 80))))
    }

    /// Python `repr()` of a str.
    fn repr_python(s: &str) -> String {
        let aspas = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
        let mut out = String::from(aspas);
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c == aspas => {
                    out.push('\\');
                    out.push(c);
                }
                c if !imprimivel(c) && (c as u32) < 0x100 => out.push_str(&format!("\\x{:02x}", c as u32)),
                c if !imprimivel(c) && (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c if !imprimivel(c) => out.push_str(&format!("\\U{:08x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push(aspas);
        out
    }

    // ponytail: control chars plus the common invisible separators/format chars, not the full
    // Unicode tables Python's str.isprintable uses; add them if a field shows other invisibles.
    fn imprimivel(c: char) -> bool {
        let u = c as u32;
        !(c.is_control()
            || matches!(u, 0xA0 | 0xAD | 0x1680 | 0x2000..=0x200F | 0x2028..=0x202F | 0x205F..=0x2064 | 0x3000 | 0xFEFF | 0xE000..=0xF8FF))
    }

    pub fn corta(s: &str, max: usize) -> String {
        s.chars().take(max).collect()
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Gesto {
        Clique { botao: Button, x: i32, y: i32, duplo: bool },
        Mover { x: i32, y: i32 },
        /// `roda` in WHEEL_DELTA units, already negated: positive `delta` scrolls down.
        Rolar { x: i32, y: i32, roda: i32 },
        Arrastar { botao: Button, x: i32, y: i32, x2: i32, y2: i32 },
    }

    fn dentro(x: Option<i32>, y: Option<i32>, w: i32, h: i32) -> Option<(i32, i32)> {
        let (x, y) = (x?, y?);
        ((0..w).contains(&x) && (0..h).contains(&y)).then_some((x, y))
    }

    /// Mouse action checked against the primary screen `w`×`h` (windows_uia.py:272-300).
    pub fn gesto(acao: &Action, w: i32, h: i32) -> Result<Gesto, DesktopError> {
        let (x, y) = dentro(acao.x, acao.y, w, h).ok_or_else(|| valor_err("coordenada fora da tela"))?;
        let botao = acao.button.unwrap_or(Button::Left);
        Ok(match acao.mode.unwrap_or(MouseMode::Click) {
            MouseMode::Click => Gesto::Clique { botao, x, y, duplo: false },
            MouseMode::Double => Gesto::Clique { botao, x, y, duplo: true },
            MouseMode::Move => Gesto::Mover { x, y },
            MouseMode::Scroll => {
                let delta = acao.delta.unwrap_or(0);
                if delta.abs() > 50 {
                    return Err(valor_err("delta inválido"));
                }
                Gesto::Rolar { x, y, roda: -delta * 120 }
            }
            MouseMode::Drag => {
                let (x2, y2) = dentro(acao.x2, acao.y2, w, h).ok_or_else(|| valor_err("destino fora da tela"))?;
                Gesto::Arrastar { botao, x, y, x2, y2 }
            }
        })
    }

    /// Pixel → `MOUSEEVENTF_ABSOLUTE` 0..=65535; rounds up so Windows' floor(n·w/65536) lands back on `x`.
    pub fn absoluto(x: i32, largura: i32) -> i32 {
        absoluto_em(x, 0, largura)
    }

    /// Same for a screen starting at `origem` (the virtual desktop, with `MOUSEEVENTF_VIRTUALDESK`).
    pub fn absoluto_em(x: i32, origem: i32, largura: i32) -> i32 {
        let w = largura.max(1) as i64;
        let n = ((x as i64 - origem as i64) * 65536 + w - 1).div_euclid(w);
        n.clamp(0, 65535) as i32
    }

    /// At most 1280 wide: the vision call drops from ~40 s to ~10 s (windows_uia.py:367-368).
    pub fn tamanho_reduzido(w: u32, h: u32) -> (u32, u32) {
        if w <= 1280 {
            return (w, h);
        }
        (1280, (h as f64 * 1280.0 / w as f64).round_ties_even() as u32)
    }

    /// RGBA top-down pixels → opaque PNG at most 1280 wide.
    pub fn png_reduzido(rgba: Vec<u8>, w: u32, h: u32) -> Result<Vec<u8>, DesktopError> {
        let rgba = image::RgbaImage::from_raw(w, h, rgba).ok_or_else(|| runtime_err("captura com tamanho inconsistente"))?;
        // BitBlt leaves alpha at 0; dropping the channel keeps the PNG opaque like PIL's RGB grab.
        let mut img = image::DynamicImage::ImageRgba8(rgba).into_rgb8();
        let (nw, nh) = tamanho_reduzido(w, h);
        if (nw, nh) != (w, h) {
            img = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::CatmullRom);
        }
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), nw, nh, image::ExtendedColorType::Rgb8)
            .map_err(|e| runtime_err(e.to_string()))?;
        Ok(png)
    }
}

#[cfg(windows)]
mod desktop {
    use crate::regras::{self, runtime_err, valor_err};
    use crate::{arvore, captura, entrada, sessao_win};
    use hcc_protocolo::{Action, ActionType, Amount, Desktop, DesktopError, Direction, Observation, Screen};
    use std::collections::HashMap;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use uiautomation::UIAutomation;
    use uiautomation::patterns::{UIExpandCollapsePattern, UIInvokePattern, UIScrollPattern, UISelectionItemPattern, UITogglePattern, UIValuePattern};
    use uiautomation::types::ScrollAmount;

    pub struct WindowsDesktop {
        sessao: sessao_win::Sessao,
        ua: UIAutomation,
        janelas: HashMap<String, arvore::Janela>,
        controles: HashMap<String, arvore::Controle>,
    }

    impl WindowsDesktop {
        pub fn new() -> Result<WindowsDesktop, String> {
            // Before any UIA/GDI call, else rects and the capture come in scaled coordinates.
            sessao_win::dpi_por_monitor();
            let sessao = sessao_win::Sessao::abrir().map_err(|e| e.to_string())?;
            let ua = UIAutomation::new().map_err(|e| arvore::com(e).to_string())?;
            Ok(WindowsDesktop { sessao, ua, janelas: HashMap::new(), controles: HashMap::new() })
        }

        fn controle(&self, alvo: Option<&str>) -> Result<&arvore::Controle, DesktopError> {
            alvo.and_then(|a| self.controles.get(a)).ok_or_else(|| valor_err("controle não pertence à observação"))
        }
    }

    fn pausa(segundos: f64) {
        std::thread::sleep(Duration::from_secs_f64(segundos));
    }

    fn padrao<T>(r: uiautomation::Result<T>) -> Result<T, DesktopError> {
        r.map_err(arvore::com)
    }

    fn quantidade(n: i32) -> ScrollAmount {
        match n {
            0 => ScrollAmount::LargeDecrement,
            1 => ScrollAmount::SmallDecrement,
            3 => ScrollAmount::LargeIncrement,
            4 => ScrollAmount::SmallIncrement,
            _ => ScrollAmount::NoAmount,
        }
    }

    impl Desktop for WindowsDesktop {
        fn session_id(&self) -> u32 {
            self.sessao.id
        }

        fn available(&mut self) -> Result<(), DesktopError> {
            self.sessao.disponivel()
        }

        fn foreground(&mut self) -> Result<String, DesktopError> {
            Ok(arvore::id_janela(sessao_win::primeiro_plano()))
        }

        fn observe(&mut self) -> Result<Observation, DesktopError> {
            self.janelas.clear();
            self.controles.clear();
            let lido = arvore::ler(&self.ua)?;
            self.janelas = lido.janelas;
            self.controles = lido.controles;
            let (w, h) = sessao_win::tela();
            Ok(Observation {
                observation_id: String::new(),
                connected: true,
                session_id: self.sessao.id,
                foreground: lido.foreground,
                windows: lido.windows,
                elements: lido.elements,
                truncated: lido.truncated,
                timestamp: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0),
                screen: Screen { width: w.max(0) as u32, height: h.max(0) as u32 },
            })
        }

        fn act(&mut self, _observed: &Observation, action: &Action) -> Result<(), DesktopError> {
            let target = action.target.as_deref();
            match action.kind {
                ActionType::Activate => {
                    let janela = target.and_then(|t| self.janelas.get(t)).ok_or_else(|| valor_err("janela não pertence à observação"))?;
                    arvore::focar_janela(janela);
                }
                ActionType::Launch => {
                    let app = action.application.as_deref().filter(|a| !a.is_empty()).ok_or_else(|| valor_err("application e args inválidos"))?;
                    // Not waited on: the app outlives the action, as with Popen.
                    std::process::Command::new(app)
                        .args(action.args.as_deref().unwrap_or_default())
                        // Popen(close_fds=True) hands the app none of the agent's handles.
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .map_err(|e| runtime_err(e.to_string()))?;
                }
                ActionType::Keys => {
                    let teclas = regras::teclas(action.keys.as_deref())?;
                    regras::acorde(&teclas, entrada::tecla)?;
                }
                ActionType::Text => {
                    let valor = action.value.as_deref().filter(|v| v.chars().count() <= 20000).ok_or_else(|| valor_err("texto inválido"))?;
                    // Typing "into field X" starts by focusing X; an unknown target is ignored.
                    if let Some(c) = target.and_then(|t| self.controles.get(t)) {
                        arvore::focar(&c.el);
                        pausa(0.15);
                    }
                    entrada::digitar(valor, Duration::ZERO)?;
                }
                ActionType::Mouse => {
                    let (w, h) = sessao_win::tela();
                    entrada::gesto(regras::gesto(action, w, h)?, w, h)?;
                }
                ActionType::Invoke
                | ActionType::SetValue
                | ActionType::Select
                | ActionType::Toggle
                | ActionType::Expand
                | ActionType::Collapse
                | ActionType::Focus
                | ActionType::Scroll => {
                    let c = self.controle(target)?;
                    let el = &c.el;
                    let nome = padrao(el.get_name())?;
                    let role = arvore::papel(el)?;
                    if nome != c.name || role != c.role || !padrao(el.is_enabled())? {
                        return Err(runtime_err("controle mudou; observe novamente"));
                    }
                    let senha = padrao(el.is_password())?;
                    let rect = arvore::retangulo(el)?;
                    let na_tela = regras::na_tela([rect.0, rect.1, rect.2, rect.3], padrao(el.is_offscreen())?);
                    match action.kind {
                        ActionType::Invoke if regras::deve_clicar(&role, na_tela) => entrada::clicar_centro(rect)?,
                        ActionType::Invoke => padrao(padrao(el.get_pattern::<UIInvokePattern>())?.invoke())?,
                        ActionType::SetValue => {
                            let valor = action.value.as_deref().filter(|v| v.chars().count() <= 20000).ok_or_else(|| valor_err("valor inválido"))?;
                            // SetValue swaps the text without notifying the field's owner (Save As kept the
                            // old name); typing like a person fires the notifications.
                            arvore::focar(el);
                            pausa(0.15);
                            let limpar = regras::teclas(Some(&["ctrl".into(), "a".into()]))?;
                            regras::acorde(&limpar, entrada::tecla)?;
                            pausa(0.05);
                            regras::acorde(&regras::teclas(Some(&["delete".into()]))?, entrada::tecla)?;
                            pausa(0.05);
                            entrada::digitar(valor, Duration::from_millis(10))?;
                            pausa(0.15);
                            if !senha {
                                let lido = padrao(padrao(el.get_pattern::<UIValuePattern>())?.get_value())?;
                                if !regras::valor_confirmado(&lido, valor) {
                                    return Err(regras::valor_nao_confirmado(&lido));
                                }
                            }
                        }
                        // UIA Select/Toggle marks without firing Delphi's OnClick; a real click fires it.
                        ActionType::Select | ActionType::Toggle if na_tela => entrada::clicar_centro(rect)?,
                        ActionType::Select => padrao(padrao(el.get_pattern::<UISelectionItemPattern>())?.select())?,
                        ActionType::Toggle => padrao(padrao(el.get_pattern::<UITogglePattern>())?.toggle())?,
                        ActionType::Expand => padrao(padrao(el.get_pattern::<UIExpandCollapsePattern>())?.expand())?,
                        ActionType::Collapse => padrao(padrao(el.get_pattern::<UIExpandCollapsePattern>())?.collapse())?,
                        ActionType::Focus => arvore::focar(el),
                        ActionType::Scroll => {
                            let (hz, vt) = regras::quantidade_scroll(action.direction.unwrap_or(Direction::Down), action.amount.unwrap_or(Amount::Small));
                            padrao(padrao(el.get_pattern::<UIScrollPattern>())?.scroll(quantidade(hz), quantidade(vt)))?;
                        }
                        _ => unreachable!("outer match lists the element actions"),
                    }
                }
            }
            Ok(())
        }

        fn screenshot_png(&mut self) -> Result<Vec<u8>, DesktopError> {
            let (w, h) = sessao_win::tela();
            captura::png_tela(w, h)
        }
    }
}
