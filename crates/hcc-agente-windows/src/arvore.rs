//! Accessibility tree walk (windows_uia.py:77-191).

use crate::regras::{self, runtime_err};
use crate::{entrada, sessao_win};
use hcc_protocolo::{DesktopError, Element, ElementAction, ErrorKind, Rect, RootSpec, Scope, Window};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use uiautomation::patterns::{UIExpandCollapsePattern, UIInvokePattern, UIPatternType, UIScrollPattern, UISelectionItemPattern, UITextPattern, UITogglePattern, UIValuePattern};
use uiautomation::types::{ExpandCollapseState, Handle, ToggleState, TreeScope, UIProperty};
use uiautomation::core::UICacheRequest;
use uiautomation::{UIAutomation, UIElement};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GW_OWNER, GetClassNameW, GetWindow, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SW_RESTORE,
    SetForegroundWindow, ShowWindow,
};
use windows::core::{BOOL, w};

const MAX_NOS: usize = 800;
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
        _ => DesktopError::new(ErrorKind::ComError, e.to_string()),
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

/// Everything `elemento` reads, fetched with the children in one call per parent instead of one per property.
fn pedido_cache(ua: &UIAutomation) -> Result<UICacheRequest, DesktopError> {
    let pedido = ua.create_cache_request().map_err(com)?;
    for p in [
        UIProperty::Name,
        UIProperty::ControlType,
        UIProperty::BoundingRectangle,
        UIProperty::IsOffscreen,
        UIProperty::IsPassword,
        UIProperty::IsEnabled,
        UIProperty::HasKeyboardFocus,
        UIProperty::IsKeyboardFocusable,
        UIProperty::ToggleToggleState,
        UIProperty::SelectionItemIsSelected,
        UIProperty::ExpandCollapseExpandCollapseState,
        UIProperty::ValueValue,
        UIProperty::ValueIsReadOnly,
    ] {
        pedido.add_property(p).map_err(com)?;
    }
    for p in [
        UIPatternType::Invoke,
        UIPatternType::SelectionItem,
        UIPatternType::Toggle,
        UIPatternType::Scroll,
        UIPatternType::ExpandCollapse,
        UIPatternType::Value,
    ] {
        pedido.add_pattern(p).map_err(com)?;
    }
    Ok(pedido)
}

fn filhos_cache(ua: &UIAutomation, el: &UIElement, pedido: &UICacheRequest) -> Result<Vec<UIElement>, DesktopError> {
    let todos = ua.create_true_condition().map_err(com)?;
    el.find_all_build_cache(TreeScope::Children, &todos, pedido).map_err(com)
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

/// The user's window back in front, with the same Alt tap when the foreground lock refuses.
pub(crate) fn focar_hwnd(h: isize) {
    if h == 0 || sessao_win::primeiro_plano() == h || !sessao_win::janela_existe(&id_janela(h)) {
        return;
    }
    primeiro_plano(h);
    if sessao_win::primeiro_plano() != h {
        entrada::tocar_alt();
        primeiro_plano(h);
    }
}

/// A menu, dropdown or dialog of the window's process is open over it, or the window itself is one (a modal
/// dialog in front has an owner).
pub(crate) fn menu_aberto(h: isize) -> bool {
    let hwnd = HWND(h as *mut _);
    // SAFETY: a stale handle only makes the query fail.
    let tem_dono = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|d| !d.is_invalid());
    tem_dono || !popups(hwnd, pid(hwnd)).is_empty()
}

pub(crate) fn mesmo_processo(a: isize, b: isize) -> bool {
    a != 0 && b != 0 && pid(HWND(a as *mut _)) == pid(HWND(b as *mut _))
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

pub(crate) fn ler(ua: &UIAutomation, scope: &Scope) -> Result<Lido, DesktopError> {
    // A window read by id stays where it is: in front or behind others.
    let fg = match &scope.window {
        Some(id) => sessao_win::hwnd_de(id).filter(|_| sessao_win::janela_existe(id)).ok_or_else(|| runtime_err("a janela fechou; observe novamente"))?,
        None => sessao_win::primeiro_plano(),
    };
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
        let process_id = el.get_process_id().map_err(com)?;
        windows.push(Window {
            id: wid.clone(),
            name: el.get_name().map_err(com)?,
            process_id,
            class_name: el.get_classname().map_err(com)?,
            process: crate::sessao_win::nome_processo(process_id),
            rect: rect(&el)?,
        });
        janelas.insert(wid, Janela { hwnd, el });
    }
    let foreground = id_janela(fg);
    let ativa = janelas.get(&foreground).ok_or_else(|| runtime_err("a janela ativa mudou durante a observação"))?;
    // Read limit, not an application limit: `truncated` says when it cut.
    // Each pending node with the index of its nearest ancestor that became an element, and its depth.
    let pedido = pedido_cache(ua)?;
    let mut pendentes: VecDeque<(UIElement, Option<usize>, usize)> = filhos_cache(ua, &ativa.el, &pedido)?.into_iter().map(|e| (e, None, 0)).collect();
    let pid_ativa = ativa.el.get_process_id().map_err(com)?;
    for h in popups(HWND(fg as *mut _), pid_ativa) {
        if let Ok(f) = ua.element_from_handle(Handle::from(h)).map_err(com).and_then(|p| filhos_cache(ua, &p, &pedido)) {
            pendentes.extend(f.into_iter().map(|e| (e, None, 0)));
        }
    }
    // Taskbar buttons always: a minimised VCL app (hidden window) only comes back through them.
    // SAFETY: class-name lookup with a static wide string.
    if let Ok(barra) = unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }
        && !barra.is_invalid()
        && barra.0 as isize != fg
        && let Ok(f) = ua.element_from_handle(Handle::from(barra.0 as isize)).map_err(com).and_then(|p| filhos_cache(ua, &p, &pedido))
    {
        pendentes.extend(f.into_iter().map(|e| (e, None, 0)));
    }
    if let Some(spec) = &scope.root {
        let regiao = achar_regiao(ua, pendentes.drain(..).map(|(e, _, _)| e).collect(), &pedido, spec)?
            .ok_or_else(|| regras::valor_err("região sumiu; observe a janela inteira"))?;
        pendentes.push_back((regiao, None, 0));
    }
    let niveis = scope.skeleton.then_some(hcc_protocolo::ESQUELETO_NIVEIS);
    let mut elements = Vec::new();
    let mut controles = HashMap::new();
    let mut visitados = 0;
    let mut cortou_filhos = false;
    // Time budget, not just nodes: Chrome gives 800 nodes in 2 s, the Delphi IDE ~150 in 4 s.
    let prazo = Instant::now() + PRAZO;
    while visitados < MAX_NOS && Instant::now() < prazo {
        let Some((el, ancestral, nivel)) = pendentes.pop_front() else { break };
        visitados += 1;
        let senha = el.is_cached_password().map_err(com)?;
        // An "invisible" container (popup host, pane without a rect) still has visible children.
        let mut seus = filhos_cache(ua, &el, &pedido)?;
        let (lidos, cortados) = regras::cortar_filhos(seus.len());
        seus.truncate(lidos);
        let mut ancestral_dos_filhos = ancestral;
        if !el.is_cached_offscreen().map_err(com)?
            && let Some(e) = elemento(&el, senha, elements.len())?
        {
            ancestral_dos_filhos = Some(elements.len());
            controles.insert(e.id.clone(), Controle { el, name: e.name.clone(), role: e.role.clone() });
            elements.push(e);
        }
        // A skeleton stops here: the container only says how many children it has.
        let alem = if niveis.is_some_and(|n| nivel >= n) { u32::try_from(seus.len()).unwrap_or(u32::MAX) } else { 0 };
        cortou_filhos |= cortados > 0 || alem > 0;
        regras::somar_nao_lidos(&mut elements, ancestral_dos_filhos, cortados + alem);
        if alem == 0 {
            pendentes.extend(seus.into_iter().map(|f| (f, ancestral_dos_filhos, nivel + 1)));
        }
    }
    if scope.window.is_none() && sessao_win::primeiro_plano() != fg {
        return Err(runtime_err("a janela ativa mudou durante a observação; tente novamente"));
    }
    let truncated = cortou_filhos || !pendentes.is_empty();
    for (_, ancestral, _) in pendentes {
        regras::somar_nao_lidos(&mut elements, ancestral, 1);
    }
    Ok(Lido { foreground, windows, elements, truncated, janelas, controles })
}

/// The `spec.index`-th node with that role and name, in the order `ler` lists elements (breadth first).
fn achar_regiao(ua: &UIAutomation, raizes: Vec<UIElement>, pedido: &UICacheRequest, spec: &RootSpec) -> Result<Option<UIElement>, DesktopError> {
    let mut fila: VecDeque<UIElement> = raizes.into();
    let mut iguais = 0;
    let prazo = Instant::now() + PRAZO;
    while let Some(el) = fila.pop_front() {
        if Instant::now() >= prazo {
            return Ok(None);
        }
        if !el.is_cached_offscreen().map_err(com)?
            && el.get_cached_name().map_err(com)? == spec.name
            && format!("{:?}", el.get_cached_control_type().map_err(com)?) == spec.role
        {
            if iguais == spec.index {
                return Ok(Some(el));
            }
            iguais += 1;
        }
        let mut seus = filhos_cache(ua, &el, pedido)?;
        seus.truncate(regras::cortar_filhos(seus.len()).0);
        fila.extend(seus);
    }
    Ok(None)
}

fn elemento(el: &UIElement, senha: bool, indice: usize) -> Result<Option<Element>, DesktopError> {
    let role = format!("{:?}", el.get_cached_control_type().map_err(com)?);
    let name = el.get_cached_name().map_err(com)?;
    let mut actions = Vec::new();
    let toggle = padrao(el.get_cached_pattern::<UITogglePattern>())?;
    let selection = padrao(el.get_cached_pattern::<UISelectionItemPattern>())?;
    if padrao(el.get_cached_pattern::<UIInvokePattern>())?.is_some() {
        actions.push(ElementAction::Invoke);
    }
    if selection.is_some() {
        actions.push(ElementAction::Select);
    }
    if toggle.is_some() {
        actions.push(ElementAction::Toggle);
    }
    if padrao(el.get_cached_pattern::<UIScrollPattern>())?.is_some() {
        actions.push(ElementAction::Scroll);
    }
    if let Some(e) = padrao(el.get_cached_pattern::<UIExpandCollapsePattern>())? {
        let recolhido = e.get_cached_state().map_err(com)? == ExpandCollapseState::Collapsed;
        actions.push(if recolhido { ElementAction::Expand } else { ElementAction::Collapse });
    }
    let mut value = if let Some(t) = &toggle {
        Some(
            match t.get_cached_toggle_state().map_err(com)? {
                ToggleState::Off => "off",
                ToggleState::On => "on",
                _ => "indeterminate",
            }
            .to_string(),
        )
    } else if let Some(s) = &selection
        && matches!(role.as_str(), "RadioButton" | "ListItem" | "TabItem" | "TreeItem" | "DataItem")
    {
        Some(if s.is_cached_selected().map_err(com)? { "selected" } else { "not selected" }.to_string())
    } else {
        None
    };
    if let Some(edit) = padrao(el.get_cached_pattern::<UIValuePattern>())? {
        // A password field enters the tree to be typed into, but its content never leaves Windows.
        value = if senha { None } else { Some(edit.get_cached_value().map_err(com)?) };
        if !edit.cached_is_readonly().map_err(com)? {
            actions.push(ElementAction::SetValue);
        }
    } else if !senha
        && (matches!(role.as_str(), "Document" | "Edit") || (actions.is_empty() && name.is_empty()))
        && let Some(t) = padrao(el.get_pattern::<UITextPattern>())?
    {
        // Consoles and unnamed viewers expose their text only through TextPattern.
        value = Some(t.get_document_range().map_err(com)?.get_text(4000).map_err(com)?);
    }
    if el.is_cached_keyboard_focusable().map_err(com)? {
        actions.push(ElementAction::Focus);
    }
    if actions.is_empty() && name.is_empty() && value.is_none() {
        return Ok(None);
    }
    Ok(Some(Element {
        id: format!("e{indice}"),
        value: value.map(|v| regras::corta(&v, 4000)),
        enabled: el.is_cached_enabled().map_err(com)?,
        rect: Some({
            let r = el.get_cached_bounding_rectangle().map_err(com)?;
            Rect(r.get_left(), r.get_top(), r.get_right(), r.get_bottom())
        }),
        focused: el.has_cached_keyboard_focus().map_err(com)?,
        actions,
        password: senha,
        unread: None,
        name,
        role,
    }))
}
