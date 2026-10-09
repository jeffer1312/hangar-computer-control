//! Accessibility tree walk (windows_uia.py:77-191).

use crate::regras::{self, runtime_err};
use crate::{entrada, sessao_win};
use hcc_protocolo::{DesktopError, Element, ElementAction, ErrorKind, Rect, Window};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use uiautomation::patterns::{UIExpandCollapsePattern, UIInvokePattern, UIScrollPattern, UISelectionItemPattern, UITextPattern, UITogglePattern, UIValuePattern};
use uiautomation::types::{ExpandCollapseState, Handle, ToggleState, TreeScope};
use uiautomation::{UIAutomation, UIElement};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GW_OWNER, GetClassNameW, GetWindow, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SW_RESTORE,
    SetForegroundWindow, ShowWindow,
};
use windows::core::{BOOL, w};

const MAX_NOS: usize = 800;
const MAX_FILHOS: usize = 40;
const PRAZO: Duration = Duration::from_secs(4);

pub(crate) struct Janela {
    pub hwnd: isize,
    pub el: UIElement,
}

pub(crate) struct Controle {
    pub el: UIElement,
    pub name: String,
    pub role: String,
}

pub(crate) struct Lido {
    pub foreground: String,
    pub windows: Vec<Window>,
    pub elements: Vec<Element>,
    pub truncated: bool,
    pub janelas: HashMap<String, Janela>,
    pub controles: HashMap<String, Controle>,
}

pub(crate) fn com(e: uiautomation::Error) -> DesktopError {
    // A null pattern pointer (code 0) is pywinauto's NoPatternInterfaceError, not a COM failure:
    // the loop retries observe on COMError and must not for it.
    // Positive codes are the crate's own errors (bad argument, …): not COM, not worth a retry.
    match e.code() {
        0 => runtime_err("NoPatternInterfaceError"),
        c if c > 0 => runtime_err(e.to_string()),
        _ => DesktopError { kind: ErrorKind::ComError, msg: e.to_string() },
    }
}

pub(crate) fn id_janela(hwnd: isize) -> String {
    format!("w{hwnd}")
}

pub(crate) fn papel(el: &UIElement) -> Result<String, DesktopError> {
    Ok(format!("{:?}", el.get_control_type().map_err(com)?))
}

pub(crate) fn retangulo(el: &UIElement) -> Result<(i32, i32, i32, i32), DesktopError> {
    let r = el.get_bounding_rectangle().map_err(com)?;
    Ok((r.get_left(), r.get_top(), r.get_right(), r.get_bottom()))
}

fn rect(el: &UIElement) -> Result<Rect, DesktopError> {
    let (l, t, r, b) = retangulo(el)?;
    Ok(Rect(l, t, r, b))
}

fn handle(el: &UIElement) -> Result<isize, DesktopError> {
    Ok(el.get_native_window_handle().map_err(com)?.into())
}

/// `None` when the element does not support the pattern; other failures propagate.
fn padrao<T>(r: uiautomation::Result<T>) -> Result<Option<T>, DesktopError> {
    match r {
        Ok(p) => Ok(Some(p)),
        Err(e) if e.code() == 0 => Ok(None),
        Err(e) => Err(com(e)),
    }
}

fn filhos(ua: &UIAutomation, el: &UIElement) -> Result<Vec<UIElement>, DesktopError> {
    let todos = ua.create_true_condition().map_err(com)?;
    el.find_all(TreeScope::Children, &todos).map_err(com)
}

/// pywinauto's set_focus: UIA SetFocus, falling back to the window handle; failures only warn there.
pub(crate) fn focar(el: &UIElement) {
    if el.set_focus().is_err()
        && let Ok(h) = handle(el)
        && h != 0
    {
        primeiro_plano(h);
    }
}

pub(crate) fn focar_janela(janela: &Janela) {
    let _ = janela.el.set_focus();
    if janela.hwnd != 0 && sessao_win::primeiro_plano() != janela.hwnd {
        primeiro_plano(janela.hwnd);
        if sessao_win::primeiro_plano() != janela.hwnd {
            // The foreground lock refuses a process that did not get the last input; an Alt tap
            // counts as input (pywinauto's HwndWrapper.set_focus does the same).
            entrada::tocar_alt();
            primeiro_plano(janela.hwnd);
        }
    }
}

fn primeiro_plano(h: isize) {
    let hwnd = HWND(h as *mut _);
    // SAFETY: plain Win32 calls on a window handle; a stale handle only makes them fail.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

fn classe(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: the buffer outlives the call and its length is passed.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: `pid` outlives the call.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

struct Busca {
    foreground: HWND,
    pid: u32,
    achados: Vec<isize>,
}

unsafe extern "system" fn coletar(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the `&mut Busca` handed to EnumWindows, alive for the whole call.
    let busca = unsafe { &mut *(lparam.0 as *mut Busca) };
    // SAFETY: Win32 queries on the handle EnumWindows gave us.
    let (visivel, habilitada, dono) = unsafe { (IsWindowVisible(hwnd).as_bool(), IsWindowEnabled(hwnd).as_bool(), GetWindow(hwnd, GW_OWNER).is_ok_and(|h| !h.is_invalid())) };
    if hwnd != busca.foreground && visivel && regras::e_popup(dono, &classe(hwnd), habilitada, pid(hwnd) == busca.pid) {
        busca.achados.push(hwnd.0 as isize);
    }
    true.into()
}

/// Menus, dropdowns and dialogs are their own windows owned by the foreground's process.
fn popups(foreground: HWND, pid: u32) -> Vec<isize> {
    let mut busca = Busca { foreground, pid, achados: Vec::new() };
    // SAFETY: the callback only touches `busca`, which outlives the call.
    let _ = unsafe { EnumWindows(Some(coletar), LPARAM(&mut busca as *mut Busca as isize)) };
    busca.achados
}

pub(crate) fn ler(ua: &UIAutomation) -> Result<Lido, DesktopError> {
    let fg = sessao_win::primeiro_plano();
    let raiz = ua.get_root_element().map_err(com)?;
    let mut enumeradas = filhos(ua, &raiz)?;
    // Dialogs owned by another window can be missing from the desktop enumeration.
    let mut presente = false;
    for j in &enumeradas {
        presente |= handle(j)? == fg;
    }
    if !presente {
        enumeradas.push(ua.element_from_handle(Handle::from(fg)).map_err(com)?);
    }
    let mut windows = Vec::new();
    let mut janelas = HashMap::new();
    for el in enumeradas {
        if el.is_offscreen().map_err(com)? {
            continue;
        }
        let hwnd = handle(&el)?;
        let wid = id_janela(hwnd);
        windows.push(Window {
            id: wid.clone(),
            name: el.get_name().map_err(com)?,
            process_id: el.get_process_id().map_err(com)?,
            class_name: el.get_classname().map_err(com)?,
            rect: rect(&el)?,
        });
        janelas.insert(wid, Janela { hwnd, el });
    }
    let foreground = id_janela(fg);
    let ativa = janelas.get(&foreground).ok_or_else(|| runtime_err("a janela ativa mudou durante a observação"))?;
    // Read limit, not an application limit: `truncated` says when it cut.
    let mut pendentes: VecDeque<UIElement> = filhos(ua, &ativa.el)?.into();
    let pid_ativa = ativa.el.get_process_id().map_err(com)?;
    for h in popups(HWND(fg as *mut _), pid_ativa) {
        if let Ok(f) = ua.element_from_handle(Handle::from(h)).and_then(|p| p.find_all(TreeScope::Children, &ua.create_true_condition()?)) {
            pendentes.extend(f);
        }
    }
    // Taskbar buttons always: a minimised VCL app (hidden window) only comes back through them.
    // SAFETY: class-name lookup with a static wide string.
    if let Ok(barra) = unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }
        && !barra.is_invalid()
        && barra.0 as isize != fg
        && let Ok(f) = ua.element_from_handle(Handle::from(barra.0 as isize)).and_then(|p| p.find_all(TreeScope::Children, &ua.create_true_condition()?))
    {
        pendentes.extend(f);
    }
    let mut elements = Vec::new();
    let mut controles = HashMap::new();
    let mut visitados = 0;
    // Time budget, not just nodes: Chrome gives 800 nodes in 2 s, the Delphi IDE ~150 in 4 s.
    let prazo = Instant::now() + PRAZO;
    while visitados < MAX_NOS && Instant::now() < prazo {
        let Some(el) = pendentes.pop_front() else { break };
        visitados += 1;
        let senha = el.is_password().map_err(com)?;
        // An "invisible" container (popup host, pane without a rect) still has visible children.
        pendentes.extend(filhos(ua, &el)?.into_iter().take(MAX_FILHOS));
        if el.is_offscreen().map_err(com)? {
            continue;
        }
        if let Some(e) = elemento(&el, senha, elements.len())? {
            controles.insert(e.id.clone(), Controle { el, name: e.name.clone(), role: e.role.clone() });
            elements.push(e);
        }
    }
    if sessao_win::primeiro_plano() != fg {
        return Err(runtime_err("a janela ativa mudou durante a observação; tente novamente"));
    }
    Ok(Lido { foreground, windows, elements, truncated: !pendentes.is_empty(), janelas, controles })
}

fn elemento(el: &UIElement, senha: bool, indice: usize) -> Result<Option<Element>, DesktopError> {
    let role = papel(el)?;
    let name = el.get_name().map_err(com)?;
    let mut actions = Vec::new();
    let toggle = padrao(el.get_pattern::<UITogglePattern>())?;
    let selection = padrao(el.get_pattern::<UISelectionItemPattern>())?;
    if padrao(el.get_pattern::<UIInvokePattern>())?.is_some() {
        actions.push(ElementAction::Invoke);
    }
    if selection.is_some() {
        actions.push(ElementAction::Select);
    }
    if toggle.is_some() {
        actions.push(ElementAction::Toggle);
    }
    if padrao(el.get_pattern::<UIScrollPattern>())?.is_some() {
        actions.push(ElementAction::Scroll);
    }
    if let Some(e) = padrao(el.get_pattern::<UIExpandCollapsePattern>())? {
        let recolhido = e.get_state().map_err(com)? == ExpandCollapseState::Collapsed;
        actions.push(if recolhido { ElementAction::Expand } else { ElementAction::Collapse });
    }
    let mut value = if let Some(t) = &toggle {
        Some(
            match t.get_toggle_state().map_err(com)? {
                ToggleState::Off => "off",
                ToggleState::On => "on",
                _ => "indeterminate",
            }
            .to_string(),
        )
    } else if let Some(s) = &selection
        && matches!(role.as_str(), "RadioButton" | "ListItem" | "TabItem" | "TreeItem" | "DataItem")
    {
        Some(if s.is_selected().map_err(com)? { "selected" } else { "not selected" }.to_string())
    } else {
        None
    };
    if let Some(edit) = padrao(el.get_pattern::<UIValuePattern>())? {
        // A password field enters the tree to be typed into, but its content never leaves Windows.
        value = if senha { None } else { Some(edit.get_value().map_err(com)?) };
        if !edit.is_readonly().map_err(com)? {
            actions.push(ElementAction::SetValue);
        }
    } else if !senha
        && (matches!(role.as_str(), "Document" | "Edit") || (actions.is_empty() && name.is_empty()))
        && let Some(t) = padrao(el.get_pattern::<UITextPattern>())?
    {
        // Consoles and unnamed viewers expose their text only through TextPattern.
        value = Some(t.get_document_range().map_err(com)?.get_text(4000).map_err(com)?);
    }
    if el.is_keyboard_focusable().map_err(com)? {
        actions.push(ElementAction::Focus);
    }
    if actions.is_empty() && name.is_empty() && value.is_none() {
        return Ok(None);
    }
    Ok(Some(Element {
        id: format!("e{indice}"),
        value: value.map(|v| regras::corta(&v, 4000)),
        enabled: el.is_enabled().map_err(com)?,
        rect: Some(rect(el)?),
        focused: el.has_keyboard_focus().map_err(com)?,
        actions,
        password: senha,
        name,
        role,
    }))
}
