use atspi::proxy::accessible::ObjectRefExt;
use atspi::proxy::{action::ActionProxy, component::ComponentProxy, text::TextProxy};
use atspi::{AccessibilityConnection, AtspiError, CoordType, Interface, ObjectRefOwned};
use hcc_protocolo::{Element, ElementAction, Rect};
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

#[derive(Clone, Debug)]
pub struct NoInfo {
    pub role: String,
    pub name: Vec<u8>,
    pub states: Vec<String>,
    pub actions: Vec<String>,
    pub extents: Option<(i32, i32, i32, i32)>,
    pub text: Option<String>,
}

impl NoInfo {
    pub(crate) fn has(&self, state: &str) -> bool { self.states.iter().any(|s| s == state) }
    pub(crate) fn name(&self) -> String { String::from_utf8_lossy(&self.name).into_owned() }
}

#[derive(Debug)]
pub enum TreeError { Vanished(String), Unsupported(String), Other(String) }

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self { Self::Vanished(s) | Self::Unsupported(s) | Self::Other(s) => f.write_str(s) }
    }
}

impl From<zbus::fdo::Error> for TreeError {
    fn from(error: zbus::fdo::Error) -> Self {
        match error {
            zbus::fdo::Error::UnknownObject(_) | zbus::fdo::Error::NameHasNoOwner(_) | zbus::fdo::Error::ServiceUnknown(_) => Self::Vanished(error.to_string()),
            zbus::fdo::Error::UnknownInterface(_) | zbus::fdo::Error::UnknownMethod(_) => Self::Unsupported(error.to_string()),
            zbus::fdo::Error::ZBus(error) => error.into(),
            error => Self::Other(error.to_string()),
        }
    }
}

impl From<zbus::Error> for TreeError {
    fn from(error: zbus::Error) -> Self {
        match error {
            zbus::Error::FDO(error) => (*error).into(),
            zbus::Error::MethodError(ref name, ..) => match name.as_str() {
                "org.freedesktop.DBus.Error.UnknownObject" | "org.freedesktop.DBus.Error.NameHasNoOwner" | "org.freedesktop.DBus.Error.ServiceUnknown" => Self::Vanished(error.to_string()),
                "org.freedesktop.DBus.Error.UnknownInterface" | "org.freedesktop.DBus.Error.UnknownMethod" => Self::Unsupported(error.to_string()),
                _ => Self::Other(error.to_string()),
            },
            _ => Self::Other(error.to_string()),
        }
    }
}

impl From<AtspiError> for TreeError {
    fn from(error: AtspiError) -> Self {
        match error {
            AtspiError::NullRef(_) => Self::Vanished(error.to_string()),
            AtspiError::InterfaceNotAvailable(_) => Self::Unsupported(error.to_string()),
            error => Self::Other(error.to_string()),
        }
    }
}

pub trait Arvore {
    type Node: Clone;
    fn apps(&self) -> Result<Vec<Self::Node>, TreeError>;
    fn pid(&self, node: &Self::Node) -> Result<Option<u32>, TreeError>;
    fn read(&self, node: &Self::Node) -> Result<NoInfo, TreeError>;
    fn children(&self, node: &Self::Node) -> Result<Vec<Self::Node>, TreeError>;
    fn focus(&self, node: &Self::Node) -> Result<bool, TreeError>;
    fn invoke(&self, node: &Self::Node, index: usize) -> Result<bool, TreeError>;
    fn text(&self, node: &Self::Node) -> Result<String, TreeError>;
}

pub struct AtspiTree { runtime: Runtime, connection: AccessibilityConnection }

fn classify_error(error: impl Into<TreeError>) -> TreeError { error.into() }

impl AtspiTree {
    pub fn new() -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        let connection = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(4), AccessibilityConnection::new()).await
                .map_err(|e| e.to_string())?.map_err(|e| e.to_string())
        }).map_err(|e| format!("acessibilidade (AT-SPI) indisponível: {e}"))?;
        Ok(Self { runtime, connection })
    }

    // ProxyExt transforma o erro D-Bus em texto e perde a causa.
    async fn action_proxy<'a>(&'a self, node: &'a ObjectRefOwned) -> Result<ActionProxy<'a>, TreeError> {
        ActionProxy::builder(self.connection.connection())
            .destination(node.name().ok_or_else(|| TreeError::Vanished("aplicativo encerrado".into()))?.clone()).map_err(classify_error)?
            .path(node.path()).map_err(classify_error)?.cache_properties(zbus::proxy::CacheProperties::No)
            .build().await.map_err(classify_error)
    }

    async fn component_proxy<'a>(&'a self, node: &'a ObjectRefOwned) -> Result<ComponentProxy<'a>, TreeError> {
        ComponentProxy::builder(self.connection.connection())
            .destination(node.name().ok_or_else(|| TreeError::Vanished("aplicativo encerrado".into()))?.clone()).map_err(classify_error)?
            .path(node.path()).map_err(classify_error)?.cache_properties(zbus::proxy::CacheProperties::No)
            .build().await.map_err(classify_error)
    }

    async fn text_proxy<'a>(&'a self, node: &'a ObjectRefOwned) -> Result<TextProxy<'a>, TreeError> {
        TextProxy::builder(self.connection.connection())
            .destination(node.name().ok_or_else(|| TreeError::Vanished("aplicativo encerrado".into()))?.clone()).map_err(classify_error)?
            .path(node.path()).map_err(classify_error)?.cache_properties(zbus::proxy::CacheProperties::No)
            .build().await.map_err(classify_error)
    }

    fn call<R>(&self, future: impl Future<Output = Result<R, TreeError>>) -> Result<R, TreeError> {
        self.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(4), future).await
                .map_err(|_| TreeError::Other("AT-SPI excedeu 4 segundos".into()))?
        })
    }
}

impl Arvore for AtspiTree {
    type Node = ObjectRefOwned;

    fn apps(&self) -> Result<Vec<Self::Node>, TreeError> {
        self.call(async {
            let root = self.connection.root_accessible_on_registry().await.map_err(classify_error)?;
            Ok(root.get_children().await.map_err(classify_error)?.into_iter().filter(|n| !n.is_null()).collect())
        })
    }

    fn pid(&self, node: &Self::Node) -> Result<Option<u32>, TreeError> {
        self.call(async {
            let Some(name) = node.name() else { return Ok(None); };
            let bus = zbus::fdo::DBusProxy::new(self.connection.connection()).await.map_err(classify_error)?;
            Ok(Some(bus.get_connection_unix_process_id(name.clone().into()).await.map_err(classify_error)?))
        })
    }

    fn read(&self, node: &Self::Node) -> Result<NoInfo, TreeError> {
        self.call(async {
            let accessible = node.as_accessible_proxy(self.connection.connection()).await.map_err(classify_error)?;
            let role = accessible.get_role().await.map_err(classify_error)?;
            let estendido = if role == atspi::Role::Extended { Some(accessible.get_role_name().await.map_err(classify_error)?) } else { None };
            let role = nome_papel(role, estendido);
            let name = accessible.name().await.map_err(classify_error)?.into_bytes();
            let states = accessible.get_state().await.map_err(classify_error)?.iter().map(|s| s.to_string()).collect();
            let interfaces = accessible.get_interfaces().await.map_err(classify_error)?;
            let mut actions = vec![];
            if interfaces.contains(Interface::Action) {
                let action = self.action_proxy(node).await?;
                for i in 0..action.n_actions().await.map_err(classify_error)? {
                    actions.push(action.get_name(i).await.map_err(classify_error)?);
                }
            }
            let extents = if interfaces.contains(Interface::Component) {
                Some(self.component_proxy(node).await?.get_extents(CoordType::Window).await.map_err(classify_error)?)
            } else { None };
            let text = if role != "password text" && interfaces.contains(Interface::Text) {
                Some(self.text_proxy(node).await?.get_text(0, 4000).await.map_err(classify_error)?)
            } else { None };
            Ok(NoInfo { role, name, states, actions, extents, text })
        })
    }

    fn children(&self, node: &Self::Node) -> Result<Vec<Self::Node>, TreeError> {
        self.call(async {
            let accessible = node.as_accessible_proxy(self.connection.connection()).await.map_err(classify_error)?;
            Ok(accessible.get_children().await.map_err(classify_error)?.into_iter().filter(|n| !n.is_null()).collect())
        })
    }

    fn focus(&self, node: &Self::Node) -> Result<bool, TreeError> {
        self.call(async { self.component_proxy(node).await?.grab_focus().await.map_err(classify_error) })
    }

    fn invoke(&self, node: &Self::Node, index: usize) -> Result<bool, TreeError> {
        self.call(async {
            let index = i32::try_from(index).map_err(|e| TreeError::Other(e.to_string()))?;
            self.action_proxy(node).await?.do_action(index).await.map_err(classify_error)
        })
    }

    fn text(&self, node: &Self::Node) -> Result<String, TreeError> {
        self.call(async { self.text_proxy(node).await?.get_text(0, -1).await.map_err(classify_error) })
    }
}

pub fn nome_papel(role: atspi::Role, estendido: Option<String>) -> String {
    if role == atspi::Role::Extended { estendido.unwrap_or_else(|| role.name().into()) }
    else { role.name().into() }
}

pub const ITEMS: &[&str] = &["ListItem", "TreeItem", "TabItem", "DataItem"];
pub const INVOKE: &[&str] = &["click", "press", "activate", "dodefault", "jump", "open", "showmenu"];

pub fn role_name(role: &str, states: &[String]) -> String {
    if role == "text" { return if states.iter().any(|s| s == "editable") { "Edit" } else { "Text" }.into(); }
    let mapped = match role {
        "push button" | "button" | "toggle button" | "push button menu" => "Button",
        "check box" => "CheckBox", "radio button" => "RadioButton", "entry" | "password text" => "Edit",
        "spin button" => "Spinner", "combo box" => "ComboBox",
        "document web" | "document frame" | "document text" => "Document",
        "label" | "static" | "heading" | "paragraph" => "Text",
        "menu item" | "check menu item" | "radio menu item" | "menu" => "MenuItem",
        "list item" => "ListItem", "tree item" => "TreeItem", "table cell" => "DataItem",
        "link" => "Hyperlink", "page tab" => "TabItem", "image" | "icon" => "Image",
        "menu bar" => "MenuBar", "tool bar" => "ToolBar", "scroll bar" => "ScrollBar", "slider" => "Slider",
        "list" | "list box" => "List", "tree" | "tree table" => "Tree", "table" => "Table",
        "page tab list" => "Tab", "frame" | "dialog" | "window" => "Window", "panel" | "filler" => "Pane",
        "section" => "Group", "status bar" => "StatusBar", "progress bar" => "ProgressBar",
        _ => return role.split_whitespace().map(|word| {
            let mut chars = word.chars();
            chars.next().map(|first| first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()).unwrap_or_default()
        }).collect(),
    };
    mapped.into()
}

pub fn element_rect((x, y, w, h): (i32, i32, i32, i32), window: Rect) -> Option<Rect> {
    if w <= 0 || h <= 0 { return None; }
    let (x, y) = (window.0.checked_add(x)?, window.1.checked_add(y)?);
    Some(Rect(x, y, x.checked_add(w)?, y.checked_add(h)?))
}

pub fn element_from(info: &NoInfo, window: Rect) -> Option<Element> {
    let role = role_name(&info.role, &info.states);
    let names: Vec<String> = info.actions.iter().map(|a| a.to_lowercase()).collect();
    let mut actions = vec![];
    if names.iter().any(|n| n == "toggle") { actions.push(if ITEMS.contains(&role.as_str()) { ElementAction::Select } else { ElementAction::Toggle }); }
    else if names.iter().any(|n| INVOKE.contains(&n.as_str())) { actions.push(ElementAction::Invoke); }
    if info.has("expandable") { actions.push(if info.has("expanded") { ElementAction::Collapse } else { ElementAction::Expand }); }
    if info.has("editable") { actions.push(ElementAction::SetValue); }
    if info.has("focusable") { actions.push(ElementAction::Focus); }
    let password = info.role == "password text";
    let name = info.name();
    let value = if role == "CheckBox" || role == "RadioButton" || info.has("checkable") || info.role == "toggle button" {
        Some(if info.has("checked") { "on" } else { "off" }.into())
    } else if info.has("selectable") && ITEMS.contains(&role.as_str()) {
        Some(if info.has("selected") { "selected" } else { "not selected" }.into())
    } else if !password && (role == "Edit" || role == "Document" || (actions.is_empty() && name.is_empty())) {
        info.text.as_ref().map(|t| t.chars().take(4000).collect())
    } else { None };
    if actions.is_empty() && name.is_empty() && value.is_none() { return None; }
    Some(Element { id: String::new(), name, role, value, enabled: info.has("enabled") || info.has("sensitive"),
        rect: info.extents.and_then(|e| element_rect(e, window)), focused: info.has("focused"), actions, password })
}

fn intersects(a: Rect, b: Rect) -> bool { a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3 }

pub fn choose_frame(frames: &[(String, Vec<String>)], title: &str) -> Option<usize> {
    if let Some(index) = frames.iter().position(|(name, _)| name == title) { return Some(index); }
    let active: Vec<usize> = frames.iter().enumerate().filter(|(_, (_, states))| states.iter().any(|s| s == "active")).map(|(i, _)| i).collect();
    if active.len() == 1 { Some(active[0]) } else if frames.len() == 1 { Some(0) } else { None }
}

pub type WalkResult<N> = (Vec<Element>, Vec<N>, bool);

pub fn walk<T: Arvore>(tree: &T, roots: Vec<T::Node>, window: Rect, budget: Duration, limit: usize) -> Result<WalkResult<T::Node>, String> {
    let mut pending: VecDeque<_> = roots.into();
    let (mut controls, mut nodes, mut visited) = (vec![], vec![], 0);
    let start = Instant::now();
    while !pending.is_empty() && visited < limit && start.elapsed() < budget {
        let Some(node) = pending.pop_front() else { break; };
        visited += 1;
        let info = match tree.read(&node) { Ok(info) => info, Err(error) => { crate::ignorado("walk.read", &error); continue; } };
        if !info.has("showing") { continue; }
        match tree.children(&node) { Ok(children) => pending.extend(children), Err(error) => crate::ignorado("walk.children", &error) }
        if let Some(mut element) = element_from(&info, window) {
            if element.rect.is_some_and(|rect| !intersects(rect, window)) { continue; }
            element.id = format!("e{}", controls.len());
            controls.push(element); nodes.push(node);
        }
    }
    Ok((controls, nodes, !pending.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_and_transport_errors_are_not_vanished_nodes() {
        let permission = classify_error(zbus::Error::from(zbus::fdo::Error::AccessDenied("negado".into())));
        assert!(matches!(permission, TreeError::Other(_)));
        let disconnected = classify_error(zbus::Error::from(zbus::fdo::Error::Disconnected("conexão encerrada".into())));
        assert!(matches!(disconnected, TreeError::Other(_)));
        let destroyed = classify_error(zbus::Error::from(zbus::fdo::Error::UnknownObject("objeto destruído".into())));
        assert!(matches!(destroyed, TreeError::Vanished(_)));
    }
}
