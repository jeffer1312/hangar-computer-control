#![cfg(target_os = "linux")]

use hcc_agente_linux::LinuxDesktop;
use hcc_agente_linux::arvore::{Arvore, NoInfo, TreeError, choose_frame, element_from, element_rect, nome_papel, walk};
use hcc_agente_linux::comandos::Comandos;
use hcc_agente_linux::entrada::{evdev, keys, lua_string};
use hcc_agente_linux::hypr::{dispatch, monitor_logico, reference_monitor, windows_from};
use hcc_protocolo::{Action, ActionType, Desktop, ElementAction, ErrorKind, Rect};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

#[derive(Clone)]
struct FakeNo {
    info: NoInfo,
    children: Vec<usize>,
    vanish: bool,
    children_vanish: bool,
    focus: bool,
    pid: Option<u32>,
    read_error: Option<String>,
    children_error: Option<String>,
    focus_error: Option<String>,
    pid_error: Option<String>,
}

fn node(role: &str, name: &str) -> FakeNo {
    FakeNo {
        info: NoInfo {
            role: role.into(), name: name.as_bytes().to_vec(),
            states: vec!["showing".into(), "enabled".into()], actions: vec![],
            extents: Some((0, 0, 10, 10)), text: None,
        },
        children: vec![], vanish: false, children_vanish: false, focus: true, pid: None,
        read_error: None, children_error: None, focus_error: None, pid_error: None,
    }
}

#[derive(Clone, Default)]
struct FakeTree {
    nodes: Rc<RefCell<Vec<FakeNo>>>,
    events: Rc<RefCell<Vec<String>>>,
    readback: Rc<RefCell<Option<String>>>,
}

impl Arvore for FakeTree {
    type Node = usize;
    fn apps(&self) -> Result<Vec<usize>, TreeError> { Ok(vec![0]) }
    fn pid(&self, node: &usize) -> Result<Option<u32>, TreeError> {
        let nodes = self.nodes.borrow();
        if let Some(error) = &nodes[*node].pid_error { return Err(TreeError::Other(error.clone())); }
        Ok(nodes[*node].pid)
    }
    fn read(&self, node: &usize) -> Result<NoInfo, TreeError> {
        let nodes = self.nodes.borrow();
        if nodes[*node].vanish { return Err(TreeError::Vanished("sumiu".into())); }
        if let Some(error) = &nodes[*node].read_error { return Err(TreeError::Other(error.clone())); }
        let mut info = nodes[*node].info.clone();
        if info.role == "password text" { info.text = None; }
        Ok(info)
    }
    fn children(&self, node: &usize) -> Result<Vec<usize>, TreeError> {
        let nodes = self.nodes.borrow();
        if nodes[*node].children_vanish { return Err(TreeError::Vanished("filhos sumiram".into())); }
        if let Some(error) = &nodes[*node].children_error { return Err(TreeError::Other(error.clone())); }
        Ok(nodes[*node].children.clone())
    }
    fn focus(&self, node: &usize) -> Result<bool, TreeError> {
        self.events.borrow_mut().push(format!("focus:{node}"));
        let mut nodes = self.nodes.borrow_mut();
        if let Some(error) = &nodes[*node].focus_error { return Err(TreeError::Other(error.clone())); }
        if nodes[*node].focus { nodes[*node].info.states.push("focused".into()); }
        Ok(nodes[*node].focus)
    }
    fn invoke(&self, node: &usize, index: usize) -> Result<bool, TreeError> {
        self.events.borrow_mut().push(format!("invoke:{node}:{index}"));
        Ok(true)
    }
    fn text(&self, node: &usize) -> Result<String, TreeError> {
        self.events.borrow_mut().push(format!("text:{node}"));
        Ok(self.readback.borrow().clone().unwrap_or_else(|| self.nodes.borrow()[*node].info.text.clone().unwrap_or_default()))
    }
}

type RecordedCall = (Vec<String>, Option<Vec<u8>>);

#[derive(Clone)]
struct FakeCommands {
    calls: Rc<RefCell<Vec<RecordedCall>>>,
    active: Rc<RefCell<Value>>,
    monitors: Rc<RefCell<Value>>,
    clients: Rc<RefCell<Value>>,
    dispatch_output: Rc<RefCell<Vec<u8>>>,
    locked: Rc<RefCell<bool>>,
    fail_keys: Rc<RefCell<bool>>,
    fail_press: Rc<RefCell<bool>>,
    active_error: Rc<RefCell<Option<String>>>,
    fail_release: Rc<RefCell<bool>>,
}

impl Default for FakeCommands {
    fn default() -> Self {
        Self {
            calls: Rc::default(),
            active: Rc::new(RefCell::new(json!({"address":"0xa", "title":"Editor", "pid":7, "monitor":1, "at":[1542,51], "size":[800,600]}))),
            monitors: Rc::new(RefCell::new(json!([{"id":0,"name":"DP-1","focused":true,"x":0,"y":0,"width":1920,"height":1080,"scale":1.25}, {"id":1,"name":"DP-2","focused":false,"x":1536,"y":0,"width":1920,"height":1080,"scale":1}]))),
            clients: Rc::new(RefCell::new(json!([{"address":"0xa","title":"Editor","pid":7,"class":"editor","at":[1542,51],"size":[800,600],"mapped":true,"hidden":false}]))),
            dispatch_output: Rc::new(RefCell::new(b"ok\n".to_vec())),
            locked: Rc::default(), fail_keys: Rc::default(), fail_press: Rc::default(), active_error: Rc::default(), fail_release: Rc::default(),
        }
    }
}

impl Comandos for FakeCommands {
    fn run(&self, argv: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, String> {
        self.calls.borrow_mut().push((argv.iter().map(|s| s.to_string()).collect(), stdin.map(Vec::from)));
        match argv {
            ["pgrep", "-x", "hyprlock"] => if *self.locked.borrow() { Ok(vec![]) } else { Err("pgrep falhou (1): ".into()) },
            ["hyprctl", "-j", "activewindow"] => match &*self.active_error.borrow() { Some(error) => Err(error.clone()), None => Ok(serde_json::to_vec(&*self.active.borrow()).unwrap()) },
            ["hyprctl", "-j", "monitors"] => Ok(serde_json::to_vec(&*self.monitors.borrow()).unwrap()),
            ["hyprctl", "-j", "clients"] => Ok(serde_json::to_vec(&*self.clients.borrow()).unwrap()),
            ["hyprctl", "dispatch", _] => Ok(self.dispatch_output.borrow().clone()),
            ["ydotool", "key", events @ ..] if events.iter().all(|e| e.ends_with(":0")) && self.fail_release.replace(false) => Err("ydotool falhou (3): release falhou".into()),
            ["ydotool", "click", "0x80"] if self.fail_release.replace(false) => Err("ydotool falhou (3): release falhou".into()),
            ["ydotool", "key", ..] if self.fail_keys.replace(false) => Err("ydotool falhou (2): falhou".into()),
            ["ydotool", "click", "0x40"] if self.fail_press.replace(false) => Err("ydotool falhou (2): press falhou".into()),
            ["ydotool", ..] | ["wtype", "-"] => Ok(vec![]),
            ["grim", ..] => Ok(b"PNG".to_vec()),
            _ => panic!("comando inesperado: {argv:?}"),
        }
    }
    fn exists(&self, name: &str) -> bool { name == "editor" }
}

fn desktop(elements: Vec<FakeNo>) -> (LinuxDesktop<FakeCommands, FakeTree>, FakeCommands, FakeTree) {
    let commands = FakeCommands::default();
    let mut app = node("application", "Editor");
    app.pid = Some(7); app.children = vec![1];
    let mut frame = node("frame", "Editor");
    frame.children = (2..2 + elements.len()).collect();
    let tree = FakeTree { nodes: Rc::new(RefCell::new([vec![app, frame], elements].concat())), ..Default::default() };
    (LinuxDesktop::with_backends(commands.clone(), tree.clone(), 1000), commands, tree)
}

fn action(kind: ActionType, target: Option<&str>) -> Action {
    Action { kind, target: target.map(str::to_owned), ..Default::default() }
}

#[test]
fn monitor_com_escala_e_rotacao_em_coordenada_logica() {
    assert_eq!(monitor_logico(&json!({"x":0,"y":0,"width":1920,"height":1080,"scale":1.25,"transform":0})).unwrap(), Rect(0,0,1536,864));
    assert_eq!(monitor_logico(&json!({"x":1536,"y":0,"width":1920,"height":1080,"scale":1,"transform":1})).unwrap(), Rect(1536,0,2616,1920));
}

#[test]
fn monitor_transform_odd_swaps_and_scale_1_25() {
    assert_eq!(monitor_logico(&json!({"x":-864,"y":10,"width":1920,"height":1080,"scale":1.25,"transform":3})).unwrap(), Rect(-864,10,0,1546));
    assert_eq!(monitor_logico(&json!({"x":0,"y":0,"width":5,"height":5,"scale":2})).unwrap(), Rect(0,0,2,2));
}

#[test]
fn monitor_de_referencia_e_o_da_janela_ativa() {
    let monitors = json!([{"id":0,"focused":true},{"id":1,"focused":false}]);
    assert_eq!(reference_monitor(&monitors, &json!({"address":"0x1","monitor":1})).unwrap()["id"], 1);
    assert_eq!(reference_monitor(&monitors, &json!({})).unwrap()["id"], 0);
    assert!(reference_monitor(&json!([]), &json!({})).unwrap_err().contains("nenhum monitor"));
}

#[test]
fn janelas_relativas_ao_monitor_sem_ocultas() {
    let clients = json!([{"address":"0xa","title":"Editor","pid":7,"class":"gedit","at":[1542,51],"size":[800,600],"mapped":true,"hidden":false}, {"address":"0xb","mapped":true,"hidden":true}]);
    let windows = windows_from(&clients, (1536,0)).unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!((windows[0].id.as_str(), windows[0].name.as_str(), windows[0].process_id, windows[0].class_name.as_str(), windows[0].rect), ("w0xa","Editor",7,"gedit",Rect(6,51,806,651)));
}

#[test]
fn controle_soma_posicao_da_janela() {
    assert_eq!(element_rect((10,20,30,40), Rect(6,51,806,651)), Some(Rect(16,71,46,111)));
    assert_eq!(element_rect((-1,-1,-1,-1), Rect(0,0,9,9)), None);
}

#[test]
fn monta_controles_no_vocabulario_da_uia() {
    let mut campo = node("entry", "Nome");
    campo.info.states.extend(["editable".into(), "focusable".into()]); campo.info.text = Some("Ana".into());
    let mut senha = node("password text", "Senha"); senha.info.states.push("editable".into()); senha.info.text = Some("•••".into());
    let mut botao = node("push button", "Salvar"); botao.info.actions = vec!["click".into()];
    let mut caixa = node("check box", "Lembrar"); caixa.info.states.extend(["checkable".into(), "checked".into()]); caixa.info.actions = vec!["Toggle".into(), "Press".into()];
    let mut item = node("list item", "Downloads"); item.info.states.push("selectable".into()); item.info.actions.push("Toggle".into());
    let tree = FakeTree { nodes: Rc::new(RefCell::new(vec![campo,senha,botao,caixa,item])), ..Default::default() };
    let (controls, nodes, cut) = walk(&tree, vec![0,1,2,3,4], Rect(100,100,600,500), Duration::from_secs(4),800).unwrap();
    assert!(!cut); assert_eq!(nodes, vec![0,1,2,3,4]);
    assert_eq!(controls.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["e0","e1","e2","e3","e4"]);
    assert_eq!((controls[0].role.as_str(), controls[0].value.as_deref(), controls[0].rect), ("Edit",Some("Ana"),Some(Rect(100,100,110,110))));
    assert_eq!(controls[0].actions, vec![ElementAction::SetValue,ElementAction::Focus]);
    assert!(controls[1].password); assert_eq!(controls[1].value, None);
    assert_eq!(controls[2].actions, vec![ElementAction::Invoke]);
    assert_eq!((controls[3].actions.clone(),controls[3].value.as_deref()), (vec![ElementAction::Toggle],Some("on")));
    assert_eq!((controls[4].actions.clone(),controls[4].value.as_deref()), (vec![ElementAction::Select],Some("not selected")));
}

#[test]
fn subarvore_oculta_fora_da_janela_e_no_que_sumiu_ficam_de_fora() {
    let mut hidden = node("panel", ""); hidden.info.states = vec!["enabled".into()]; hidden.children = vec![4];
    let mut outside = node("push button", "Rolado"); outside.info.extents = Some((0,-200,50,20));
    let mut vanished = node("push button", "Fantasma"); vanished.vanish = true;
    let tree = FakeTree { nodes: Rc::new(RefCell::new(vec![hidden,outside,vanished,node("label","Pronto"),node("push button","Escondido")])), ..Default::default() };
    let (controls,_,_) = walk(&tree,vec![0,1,2,3],Rect(100,100,600,500),Duration::from_secs(4),800).unwrap();
    assert_eq!(controls.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),vec!["Pronto"]);
}

#[test]
fn no_cujos_filhos_somem_continua_valendo() {
    let mut list = node("list", "Lista"); list.children_vanish = true;
    let tree = FakeTree { nodes: Rc::new(RefCell::new(vec![list])), ..Default::default() };
    let (controls,_,_) = walk(&tree,vec![0],Rect(100,100,600,500),Duration::from_secs(4),800).unwrap();
    assert_eq!(controls[0].name, "Lista");
}

#[test]
fn corte_por_limite_e_informado() {
    let tree = FakeTree { nodes: Rc::new(RefCell::new((0..5).map(|i| node("label", &i.to_string())).collect())), ..Default::default() };
    let (controls,_,cut) = walk(&tree,vec![0,1,2,3,4],Rect(100,100,600,500),Duration::from_secs(4),3).unwrap();
    assert!(cut); assert_eq!(controls.len(),3);
    let (controls,_,cut) = walk(&tree,vec![0],Rect(100,100,600,500),Duration::ZERO,800).unwrap();
    assert!(cut); assert!(controls.is_empty());
}

#[test]
fn frame_pelo_titulo_senao_o_ativo() {
    assert_eq!(choose_frame(&[("Outra".into(),vec!["showing".into()]),("Editor".into(),vec!["showing".into()])],"Editor"),Some(1));
    assert_eq!(choose_frame(&[("a".into(),vec![]),("b".into(),vec!["active".into()])],"x"),Some(1));
    assert_eq!(choose_frame(&[("a".into(),vec![]),("b".into(),vec![])],"x"),None);
    assert_eq!(choose_frame(&[("a".into(),vec![])],"x"),Some(0));
}

#[test]
fn node_name_with_invalid_utf8_does_not_panic() {
    let mut n = node("entry", ""); n.info.name = vec![b'a',0xff,b'b'];
    let e = element_from(&n.info,Rect(0,0,100,100)).unwrap();
    assert_eq!(e.name,"a�b");
}

#[test]
fn lua_string_nao_deixa_escapar_do_literal() {
    assert_eq!(lua_string("ação\n\"'\\"), "\"a\\195\\167\\195\\163o\\010\\034\\039\\092\"");
    assert_eq!(lua_string("app/dir-a ._09"),"\"app/dir-a ._09\"");
}

#[test]
fn teclas_viram_codigos_evdev() {
    assert_eq!(evdev(&["Ctrl".into(),"a".into()]).unwrap(),vec![29,30]);
    assert_eq!(evdev(&["F13".into(),"Enter".into(),"0".into()]).unwrap(),vec![183,28,11]);
    assert!(evdev(&["ç".into()]).unwrap_err().contains("desconhecida"));
    assert!(evdev(&[]).is_err()); assert!(evdev(&vec!["a".into();9]).is_err());
    assert!(evdev(&["f01".into()]).is_err()); assert!(evdev(&["f+1".into()]).is_err());
}

#[test]
fn keys_pressiona_e_solta_em_ordem_inversa() {
    let commands = FakeCommands::default(); keys(&commands,&["ctrl".into(),"shift".into(),"t".into()]).unwrap();
    assert_eq!(commands.calls.borrow()[0].0,vec!["ydotool","key","29:1","42:1","20:1","20:0","42:0","29:0"]);
    commands.calls.borrow_mut().clear(); *commands.fail_keys.borrow_mut() = true;
    assert!(keys(&commands,&["ctrl".into(),"a".into()]).is_err());
    assert_eq!(commands.calls.borrow()[1].0,vec!["ydotool","key","30:0","29:0"]);
}

#[test]
fn texto_com_alvo_desconhecido_e_programa_inexistente_falham() {
    let (mut desktop, commands, _) = desktop(vec![]);
    let observed = desktop.observe().unwrap(); commands.calls.borrow_mut().clear();
    let mut text = action(ActionType::Text,Some("e9")); text.value = Some("x".into());
    let error = desktop.act(&observed,&text).unwrap_err(); assert_eq!(error.kind,ErrorKind::ValueError); assert!(error.msg.contains("não pertence"));
    assert!(!commands.calls.borrow().iter().any(|c| c.0[0] == "wtype" || c.0.get(1).is_some_and(|s| s == "dispatch")));
    let observed = desktop.observe().unwrap();
    let mut launch = action(ActionType::Launch,None); launch.application = Some("programa-que-nao-existe-hcc".into());
    assert!(desktop.act(&observed,&launch).unwrap_err().msg.contains("não encontrado"));
}

#[test]
fn exec_cmd_refuses_missing_program_before_dispatch() {
    let (mut desktop, commands, _) = desktop(vec![]); let observed = desktop.observe().unwrap();
    commands.calls.borrow_mut().clear();
    let mut launch = action(ActionType::Launch,None); launch.application = Some("missing".into());
    assert_eq!(desktop.act(&observed,&launch).unwrap_err().to_string(),"ValueError: programa não encontrado: missing");
    assert!(!commands.calls.borrow().iter().any(|c| c.0.get(1).is_some_and(|s| s == "dispatch")));
}

#[test]
fn dispatch_exit_0_with_error_text_is_failure() {
    let commands = FakeCommands::default(); *commands.dispatch_output.borrow_mut() = b"error: invalid dispatcher\n".to_vec();
    assert_eq!(dispatch(&commands,"x").unwrap_err(),"hyprctl dispatch falhou: error: invalid dispatcher");
}

#[test]
fn botao_visivel_recebe_clique_real_e_item_de_lista_a_acao() {
    let mut button = node("push button","Configurações"); button.info.actions = vec!["click".into()]; button.info.extents = Some((1872,7,28,28));
    let mut item = node("list item","Downloads"); item.info.actions = vec!["open".into()]; item.info.extents = Some((5,43,170,60));
    let (mut desktop, commands, tree) = desktop(vec![button,item]);
    commands.active.borrow_mut()["size"] = json!([1908,958]); commands.clients.borrow_mut()[0]["size"] = json!([1908,958]);
    let observed = desktop.observe().unwrap(); commands.calls.borrow_mut().clear();
    desktop.act(&observed,&action(ActionType::Invoke,Some("e0"))).unwrap();
    assert!(commands.calls.borrow().iter().any(|c| c.0 == vec!["hyprctl","dispatch","hl.dsp.cursor.move({ x = 3428, y = 72 })"]));
    assert!(commands.calls.borrow().iter().any(|c| c.0 == vec!["ydotool","click","0xc0"]));
    let observed = desktop.observe().unwrap(); commands.calls.borrow_mut().clear();
    desktop.act(&observed,&action(ActionType::Invoke,Some("e1"))).unwrap();
    assert_eq!(*tree.events.borrow(),vec!["invoke:3:0"]);
    assert!(!commands.calls.borrow().iter().any(|c| c.0[0] == "ydotool"));
}

#[test]
fn real_click_uses_current_window_position() {
    let mut button = node("push button","Salvar"); button.info.actions = vec!["click".into()];
    let (mut desktop, commands, _) = desktop(vec![button]); let observed = desktop.observe().unwrap();
    commands.active.borrow_mut()["at"] = json!([1600,100]); commands.calls.borrow_mut().clear();
    desktop.act(&observed,&action(ActionType::Invoke,Some("e0"))).unwrap();
    assert!(commands.calls.borrow().iter().any(|c| c.0 == vec!["hyprctl","dispatch","hl.dsp.cursor.move({ x = 1605, y = 105 })"]));
}

#[test]
fn mouse_soma_origem_do_monitor_e_rolagem_desce_com_delta_positivo() {
    let (mut desktop, commands, _) = desktop(vec![]); let observed = desktop.observe().unwrap(); commands.calls.borrow_mut().clear();
    let mouse: Action = serde_json::from_value(json!({"type":"mouse","x":10,"y":20,"button":"left","mode":"scroll","delta":3})).unwrap();
    desktop.act(&observed,&mouse).unwrap();
    assert!(commands.calls.borrow().iter().any(|c| c.0 == vec!["hyprctl","dispatch","hl.dsp.cursor.move({ x = 1546, y = 20 })"]));
    assert!(commands.calls.borrow().iter().any(|c| c.0 == vec!["ydotool","mousemove","--wheel","-x","0","-y","-3"]));
    let observed = desktop.observe().unwrap();
    assert!(desktop.act(&observed,&Action { x:Some(1920), ..mouse }).unwrap_err().msg.contains("fora da tela"));
}

#[test]
fn observation_consumed_even_when_action_fails_and_focus_changes_block_input() {
    let (mut desktop, commands, _) = desktop(vec![]); let observed = desktop.observe().unwrap();
    let invalid = action(ActionType::Invoke,Some("e99")); assert!(desktop.act(&observed,&invalid).is_err());
    assert_eq!(desktop.act(&observed,&invalid).unwrap_err().msg,"observação expirada; observe novamente");
    let observed = desktop.observe().unwrap(); commands.active.borrow_mut()["address"] = json!("0xb");
    let error = desktop.act(&observed,&invalid).unwrap_err(); assert_eq!(error.msg,"o foco mudou; observe novamente antes de agir");
}

#[test]
fn desktop_empty_and_locked_screen() {
    let (mut desktop, commands, _) = desktop(vec![]); *commands.active.borrow_mut() = json!({}); *commands.clients.borrow_mut() = json!([]);
    let observed = desktop.observe().unwrap(); assert_eq!(observed.foreground,"desktop"); assert_eq!(observed.windows[0].name,"Área de trabalho");
    assert_eq!(observed.screen.width,1536); *commands.locked.borrow_mut() = true;
    assert_eq!(desktop.observe().unwrap_err().msg,"tela bloqueada");
}

#[test]
fn screenshot_uses_reference_monitor_and_caps_width() {
    let (mut desktop, commands, _) = desktop(vec![]); assert_eq!(desktop.screenshot_png().unwrap(),b"PNG");
    assert!(commands.calls.borrow().iter().any(|c| c.0 == vec!["grim","-o","DP-2","-s","0.6666666666666666","-"]));
    commands.monitors.borrow_mut()[1].as_object_mut().unwrap().remove("name");
    assert!(desktop.screenshot_png().is_err());
}

#[test]
fn set_value_confirms_readback_and_text_uses_stdin() {
    let mut field = node("entry","Nome"); field.info.states.push("editable".into()); field.info.text = Some("old".into());
    let (mut desktop, commands, tree) = desktop(vec![field]); *tree.readback.borrow_mut() = Some(" ação\r\n".into());
    let observed = desktop.observe().unwrap();
    let set = Action { value:Some("ação\n".into()), ..action(ActionType::SetValue,Some("e0")) };
    desktop.act(&observed,&set).unwrap();
    assert!(commands.calls.borrow().iter().any(|c| c.0 == vec!["wtype","-"] && c.1.as_deref() == Some("ação\n".as_bytes())));
    let observed = desktop.observe().unwrap(); *tree.readback.borrow_mut() = Some("errado".into());
    assert!(desktop.act(&observed,&set).unwrap_err().msg.contains("valor digitado não confirmado"));
}

#[test]
fn set_value_without_text_requires_confirmed_focus() {
    let mut field = node("entry","Nome"); field.focus = false; field.info.states.push("editable".into());
    let (mut desktop, commands, _) = desktop(vec![field]); let observed = desktop.observe().unwrap();
    let set = Action { value:Some("ação".into()), ..action(ActionType::SetValue,Some("e0")) };
    assert_eq!(desktop.act(&observed,&set).unwrap_err().msg,"campo não recebeu o foco; nada foi digitado");
    assert!(!commands.calls.borrow().iter().any(|c| c.0[0] == "wtype" || c.0.get(1).is_some_and(|s| s == "key")));
}

#[test]
fn password_values_are_never_returned_or_read_back() {
    let mut field = node("password text","Senha"); field.info.states.push("editable".into()); field.info.text = Some("secret".into());
    let (mut desktop, _, tree) = desktop(vec![field]); let observed = desktop.observe().unwrap();
    assert!(observed.elements[0].password); assert_eq!(observed.elements[0].value,None);
    let set = Action { value:Some("secret".into()), ..action(ActionType::SetValue,Some("e0")) };
    desktop.act(&observed,&set).unwrap(); assert!(!tree.events.borrow().iter().any(|e| e.starts_with("text:")));
}

#[test]
fn failed_drag_still_releases_button() {
    let (mut desktop, commands, _) = desktop(vec![]); let observed = desktop.observe().unwrap();
    let drag: Action = serde_json::from_value(json!({"type":"mouse","x":1,"y":2,"x2":-1,"y2":2,"button":"left","mode":"drag"})).unwrap();
    assert!(desktop.act(&observed,&drag).is_err());
    assert!(commands.calls.borrow().iter().any(|c| c.0 == vec!["ydotool","click","0x80"]));
}

#[test]
fn traversal_keeps_children_after_the_fortieth() {
    let mut root = node("panel", ""); root.children = (1..=45).collect();
    let tree = FakeTree { nodes: Rc::new(RefCell::new([vec![root], (1..=45).map(|n| node("label", &n.to_string())).collect()].concat())), ..Default::default() };
    let (controls,_,cut) = walk(&tree, vec![0], Rect(0,0,100,100), Duration::from_secs(4),800).unwrap();
    assert_eq!(controls.len(),45); assert_eq!(controls[44].name,"45"); assert!(!cut);
}

#[test]
fn observation_uses_own_pid_and_includes_visible_popups() {
    let (mut desktop, _, tree) = desktop(vec![node("label","Editor content")]);
    let mut popup = node("menu", "Menu"); popup.children = vec![4];
    tree.nodes.borrow_mut()[0].children.push(3);
    tree.nodes.borrow_mut().extend([popup,node("menu item","Abrir")]);
    let observed = desktop.observe().unwrap();
    assert_eq!(observed.elements.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),vec!["Editor content","Abrir"]);
    tree.nodes.borrow_mut()[0].pid = Some(8);
    assert!(desktop.observe().unwrap().elements.is_empty());
}

#[test]
fn changed_control_is_not_clicked() {
    let (mut desktop, commands, tree) = desktop(vec![node("push button","Salvar")]);
    let observed = desktop.observe().unwrap(); tree.nodes.borrow_mut()[2].info.name = b"Apagar".to_vec();
    commands.calls.borrow_mut().clear();
    assert_eq!(desktop.act(&observed,&action(ActionType::Invoke,Some("e0"))).unwrap_err().msg,"controle mudou; observe novamente");
    assert!(!commands.calls.borrow().iter().any(|c| c.0[0] == "ydotool"));
}

#[test]
fn missing_action_is_failure_not_success() {
    let mut button = node("push button", "Salvar"); button.info.extents = None;
    let (mut desktop, _, _) = desktop(vec![button]); let observed = desktop.observe().unwrap();
    let error = desktop.act(&observed,&action(ActionType::Invoke,Some("e0"))).unwrap_err();
    assert!(error.msg.starts_with("controle sem ação click:"));
}

#[test]
fn text_length_uses_characters_and_invalid_keys_never_press() {
    let (mut desktop, commands, _) = desktop(vec![]); let observed = desktop.observe().unwrap();
    desktop.act(&observed,&Action { value:Some("ç".repeat(20000)), ..action(ActionType::Text,None) }).unwrap();
    let observed = desktop.observe().unwrap(); commands.calls.borrow_mut().clear();
    assert_eq!(desktop.act(&observed,&Action { value:Some("ç".repeat(20001)), ..action(ActionType::Text,None) }).unwrap_err().msg,"texto inválido");
    assert!(!commands.calls.borrow().iter().any(|c| c.0[0] == "wtype"));
    assert!(keys(&commands,&["ctrl".into(),"unknown".into()]).is_err());
    assert!(!commands.calls.borrow().iter().any(|c| c.0[0] == "ydotool"));
}

#[test]
fn focus_fallback_uses_current_geometry_for_text_and_set_value() {
    for kind in [ActionType::Text, ActionType::SetValue] {
        let mut field = node("entry", "Nome"); field.focus = false;
        field.info.states.push("editable".into()); field.info.text = Some("old".into());
        field.info.extents = Some((10,20,30,40));
        let (mut desktop, commands, tree) = desktop(vec![field]);
        let observed = desktop.observe().unwrap();
        commands.active.borrow_mut()["at"] = json!([1600,100]);
        tree.nodes.borrow_mut()[2].info.extents = Some((30,40,20,20));
        *tree.readback.borrow_mut() = Some("new".into());
        commands.calls.borrow_mut().clear();
        desktop.act(&observed, &Action { value:Some("new".into()), ..action(kind,Some("e0")) }).unwrap();
        assert!(commands.calls.borrow().iter().any(|c| c.0 == vec!["hyprctl","dispatch","hl.dsp.cursor.move({ x = 1640, y = 150 })"]), "{kind}: stale focus coordinates");
        assert!(!commands.calls.borrow().iter().any(|c| c.0 == vec!["hyprctl","dispatch","hl.dsp.cursor.move({ x = 1567, y = 91 })"]));
    }
}

#[test]
fn failed_drag_press_still_releases_and_preserves_original_error() {
    let (mut desktop, commands, _) = desktop(vec![]); let observed = desktop.observe().unwrap();
    *commands.fail_press.borrow_mut() = true; commands.calls.borrow_mut().clear();
    let drag: Action = serde_json::from_value(json!({"type":"mouse","x":1,"y":2,"x2":3,"y2":4,"button":"left","mode":"drag"})).unwrap();
    let error = desktop.act(&observed,&drag).unwrap_err();
    assert_eq!(error.to_string(),"RuntimeError: ydotool falhou (2): press falhou");
    assert_eq!(commands.calls.borrow().last().unwrap().0, vec!["ydotool","click","0x80"]);
}

#[test]
fn stderr_timeout_words_do_not_change_runtime_error_kind() {
    let (mut desktop, commands, _) = desktop(vec![]);
    *commands.active_error.borrow_mut() = Some("hyprctl falhou (1): excedeu 15 segundos".into());
    assert_eq!(desktop.observe().unwrap_err().kind,ErrorKind::RuntimeError);
    *commands.active_error.borrow_mut() = Some("hyprctl falhou (1): AT-SPI excedeu 4 segundos".into());
    assert_eq!(desktop.observe().unwrap_err().kind,ErrorKind::RuntimeError);
    *commands.active_error.borrow_mut() = Some("hyprctl excedeu 15 segundos".into());
    assert_eq!(desktop.observe().unwrap_err().kind,ErrorKind::RuntimeError);
    *commands.active_error.borrow_mut() = Some("AT-SPI excedeu 4 segundos".into());
    assert_eq!(desktop.observe().unwrap_err().kind,ErrorKind::RuntimeError);
}

fn captured_stderr(test: &str) -> Option<String> {
    if std::env::var("HCC_LINUX_STDERR_CAPTURE").as_deref() == Ok(test) { return None; }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"]).env("HCC_LINUX_STDERR_CAPTURE", test).output().unwrap();
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    Some(String::from_utf8_lossy(&output.stderr).into_owned())
}

#[test]
fn role_enum_matches_libatspi_and_preserves_extended_names() {
    assert_eq!(nome_papel(atspi::Role::Frame,Some("window".into())),"frame");
    assert_eq!(nome_papel(atspi::Role::Entry,Some("text box".into())),"entry");
    assert_eq!(nome_papel(atspi::Role::PasswordText,None),"password text");
    assert_eq!(nome_papel(atspi::Role::Button,None),"button");
    assert_eq!(nome_papel(atspi::Role::PageTabList,Some("tab list".into())),"page tab list");
    assert_eq!(nome_papel(atspi::Role::Extended,Some("Custom Role".into())),"Custom Role");
}

#[test]
fn walk_skips_node_with_any_dbus_error() {
    if let Some(stderr) = captured_stderr("walk_skips_node_with_any_dbus_error") {
        assert!(stderr.contains("hcc-agente-linux: ignorado walk.read: org.freedesktop.DBus.Error.Failed: x"));
        assert!(stderr.contains("hcc-agente-linux: ignorado walk.read: AT-SPI excedeu 4 segundos"));
        assert!(stderr.contains("hcc-agente-linux: ignorado walk.children: filhos indisponíveis"));
        assert!(stderr.contains("hcc-agente-linux: ignorado walk.read: erro\\nmultilinha"));
        return;
    }
    for error in ["org.freedesktop.DBus.Error.Failed: x","org.freedesktop.DBus.Error.AccessDenied: x","AT-SPI excedeu 4 segundos","erro\nmultilinha"] {
        let mut failed = node("label","Indisponível"); failed.read_error = Some(error.into());
        let tree = FakeTree { nodes: Rc::new(RefCell::new(vec![failed,node("label","Pronto")])), ..Default::default() };
        let (elements, _, cut) = walk(&tree,vec![0,1],Rect(0,0,100,100),Duration::from_secs(4),800).unwrap();
        assert_eq!(elements.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),vec!["Pronto"]); assert!(!cut);
    }
    let mut list = node("list","Lista"); list.children_error = Some("filhos indisponíveis".into());
    let tree = FakeTree { nodes: Rc::new(RefCell::new(vec![list])), ..Default::default() };
    let (elements, _, _) = walk(&tree,vec![0],Rect(0,0,100,100),Duration::from_secs(4),800).unwrap();
    assert_eq!(elements[0].name,"Lista");
}

#[test]
fn focus_error_falls_back_to_click() {
    if let Some(stderr) = captured_stderr("focus_error_falls_back_to_click") {
        assert!(stderr.contains("hcc-agente-linux: ignorado focus_field.focus: foco indisponível"));
        assert!(!stderr.contains("private-typed-value"));
        return;
    }
    for kind in [ActionType::Text,ActionType::SetValue] {
        let mut field = node("entry","Nome"); field.focus_error = Some("foco indisponível".into());
        field.info.text = Some("old".into()); field.info.states.push("editable".into());
        let (mut desktop, commands, tree) = desktop(vec![field]); let observed = desktop.observe().unwrap();
        *tree.readback.borrow_mut() = Some("private-typed-value".into());
        desktop.act(&observed,&Action { value:Some("private-typed-value".into()), ..action(kind,Some("e0")) }).unwrap();
        assert!(commands.calls.borrow().iter().any(|c| c.0 == vec!["hyprctl","dispatch","hl.dsp.cursor.move({ x = 1547, y = 56 })"]));
        assert!(commands.calls.borrow().iter().any(|c| c.0 == vec!["ydotool","click","0xc0"]));
    }
}

#[test]
fn pid_error_skips_app_and_reports_it() {
    if let Some(stderr) = captured_stderr("pid_error_skips_app_and_reports_it") {
        assert!(stderr.contains("hcc-agente-linux: ignorado roots.pid: pid indisponível")); return;
    }
    let (mut desktop, _, tree) = desktop(vec![node("label","Pronto")]);
    tree.nodes.borrow_mut()[0].pid_error = Some("pid indisponível".into());
    assert!(desktop.observe().unwrap().elements.is_empty());
}

#[test]
fn double_release_failure_preserves_original_and_reports_secondary() {
    if let Some(stderr) = captured_stderr("double_release_failure_preserves_original_and_reports_secondary") {
        assert!(stderr.contains("hcc-agente-linux: ignorado keys.release: ydotool falhou (3): release falhou"));
        assert!(stderr.contains("hcc-agente-linux: ignorado mouse.release: RuntimeError: ydotool falhou (3): release falhou")); return;
    }
    let commands = FakeCommands::default(); *commands.fail_keys.borrow_mut() = true; *commands.fail_release.borrow_mut() = true;
    assert_eq!(keys(&commands,&["ctrl".into(),"a".into()]).unwrap_err(),"ydotool falhou (2): falhou");
    let (mut desktop, commands, _) = desktop(vec![]); let observed = desktop.observe().unwrap();
    *commands.fail_press.borrow_mut() = true; *commands.fail_release.borrow_mut() = true;
    let drag: Action = serde_json::from_value(json!({"type":"mouse","x":1,"y":2,"x2":3,"y2":4,"button":"left","mode":"drag"})).unwrap();
    assert_eq!(desktop.act(&observed,&drag).unwrap_err().to_string(),"RuntimeError: ydotool falhou (2): press falhou");
}

#[test]
fn password_without_confirmed_focus_does_not_type() {
    let mut field = node("password text","Senha"); field.focus = false;
    field.info.text = Some("secret".into()); field.info.states.push("editable".into());
    let (mut desktop, commands, _) = desktop(vec![field]); let observed = desktop.observe().unwrap();
    let set = Action { value:Some("secret".into()), ..action(ActionType::SetValue,Some("e0")) };
    assert_eq!(desktop.act(&observed,&set).unwrap_err().msg,"campo não recebeu o foco; nada foi digitado");
    assert!(!commands.calls.borrow().iter().any(|c| c.0[0] == "wtype"));
}

fn with_xdg_apps(test: &str, populate: impl FnOnce(&std::path::Path)) -> bool {
    if std::env::var("HCC_APPS_TEST").as_deref() == Ok(test) { return false; }
    let root = std::env::temp_dir().join(format!("hcc-apps-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir(&root).unwrap();
    for dir in ["user", "system", "extra"] { std::fs::create_dir_all(root.join(dir).join("applications")).unwrap(); }
    populate(&root);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"]).env("HCC_APPS_TEST", test)
        .env("XDG_DATA_HOME", root.join("user"))
        .env("XDG_DATA_DIRS", std::env::join_paths([root.join("system"), root.join("extra")]).unwrap())
        .output().unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "child test did not run: {}", String::from_utf8_lossy(&output.stdout));
    true
}

#[test]
fn installed_apps_from_xdg_are_cached_and_filtered() {
    if with_xdg_apps("installed_apps_from_xdg_are_cached_and_filtered", |root| {
        for (file, text) in [
            ("user/applications/editor.desktop", "[Desktop Entry]\nName=Editor de Texto\nExec=gnome-text-editor %U\n[Desktop Action Other]\nName=Other\nExec=other\n"),
            ("system/applications/files.desktop", "[Desktop Entry]\nName=Arquivos\nExec=\"/opt/My App/files\" %f\n"),
            ("extra/applications/duplicate.desktop", "[Desktop Entry]\nName=Editor de Texto\nExec=gnome-text-editor %U\n"),
            ("user/applications/nodisplay.desktop", "[Desktop Entry]\nName=Invisible\nExec=invisible\nNoDisplay=true\n"),
            ("system/applications/hidden.desktop", "[Desktop Entry]\nName=Hidden\nExec=hidden\nHidden=true\n"),
            ("user/applications/empty.desktop", "[Desktop Entry]\nName=Empty\nExec=%U\n"),
            ("user/applications/not-desktop.txt", "[Desktop Entry]\nName=Not Desktop\nExec=not-desktop\n"),
        ] { std::fs::write(root.join(file), text).unwrap(); }
    }) { return; }
    let (mut desktop, commands, _) = desktop(vec![]);
    let expected = json!([r#"Arquivos => "/opt/My App/files""#, "Editor de Texto => gnome-text-editor"]);
    assert_eq!(serde_json::to_value(desktop.observe().unwrap()).unwrap()["apps"], expected);
    let home = std::env::var_os("XDG_DATA_HOME").unwrap();
    std::fs::write(std::path::PathBuf::from(home).join("applications/editor.desktop"), "[Desktop Entry]\nName=Changed\nExec=changed\n").unwrap();
    assert_eq!(serde_json::to_value(desktop.observe().unwrap()).unwrap()["apps"], expected);
    assert!(!commands.calls.borrow().iter().any(|c| c.0[0] != "pgrep" && c.0 != ["hyprctl", "-j", "activewindow"] && c.0 != ["hyprctl", "-j", "monitors"] && c.0 != ["hyprctl", "-j", "clients"]));
}

#[test]
fn installed_exec_preserves_arguments_and_quotes() {
    if with_xdg_apps("installed_exec_preserves_arguments_and_quotes", |root| {
        for (i, (name, exec)) in [
            ("A Flatpak", "flatpak run org.x.App %U"),
            ("B Env", "env VAR=x app %f"),
            ("C Quote", r#""/opt/My App/files" %f"#),
            ("Conversor => PDF", "app %F %u %U %i %c %k %d %D %n %N %v %m"),
            ("D Escape", r#"app "a \\"b\\"""#),
            ("E Percent", "app 100%%"),
            ("F Arrowarg", r#"app "left => right""#),
            ("Z Invalid", "VAR=x app"),
            ("Z Unbalanced", "app \"unfinished"),
        ].into_iter().enumerate() {
            std::fs::write(root.join(format!("user/applications/{i}.desktop")), format!("[Desktop Entry]\nName={name}\nExec={exec}\n")).unwrap();
        }
    }) { return; }
    let (mut desktop, _, _) = desktop(vec![]);
    assert_eq!(serde_json::to_value(desktop.observe().unwrap()).unwrap()["apps"], json!([
        "A Flatpak => flatpak run org.x.App",
        "B Env => env VAR=x app",
        r#"C Quote => "/opt/My App/files""#,
        "Conversor -> PDF => app",
        r#"D Escape => app "a \"b\"""#,
        "E Percent => app 100%",
        r#"F Arrowarg => app "left => right""#,
    ]));
}

#[test]
#[should_panic(expected = "child test did not run")]
fn xdg_fixture_refuses_missing_child_test() {
    with_xdg_apps("missing_child_test_fixture", |_| {});
}

#[test]
fn user_hidden_desktop_entry_overrides_system() {
    if with_xdg_apps("user_hidden_desktop_entry_overrides_system", |root| {
        std::fs::write(root.join("user/applications/editor.desktop"), "[Desktop Entry]\nHidden=true\n").unwrap();
        std::fs::write(root.join("system/applications/editor.desktop"), "[Desktop Entry]\nName=Removed Editor\nExec=removed-editor\n").unwrap();
        std::fs::write(root.join("user/applications/files.desktop"), "[Desktop Entry]\nName=Files\nExec=user-files\n").unwrap();
        std::fs::write(root.join("system/applications/files.desktop"), "[Desktop Entry]\nName=System Files\nExec=system-files\n").unwrap();
    }) { return; }
    let (mut desktop, _, _) = desktop(vec![]);
    assert_eq!(serde_json::to_value(desktop.observe().unwrap()).unwrap()["apps"], json!(["Files => user-files"]));
}

#[test]
fn installed_apps_are_sorted_deduplicated_and_capped() {
    if with_xdg_apps("installed_apps_are_sorted_deduplicated_and_capped", |root| {
        for i in (0..155).rev() {
            std::fs::write(root.join(format!("user/applications/{i}.desktop")), format!("[Desktop Entry]\nName=App {i:03}\nExec=app{i:03} %f\n")).unwrap();
        }
        std::fs::write(root.join("system/applications/duplicate.desktop"), "[Desktop Entry]\nName=App 000\nExec=app000\n").unwrap();
    }) { return; }
    let (mut desktop, _, _) = desktop(vec![]);
    let wire = serde_json::to_value(desktop.observe().unwrap()).unwrap();
    let apps = wire["apps"].as_array().expect("installed apps missing");
    assert_eq!(apps.len(), 150);
    assert_eq!(apps[0], "App 000 => app000");
    assert_eq!(apps[149], "App 149 => app149");
    assert!(apps.windows(2).all(|pair| pair[0].as_str().unwrap() < pair[1].as_str().unwrap()));
}

#[test]
#[ignore = "desktop real"]
fn observe_real_desktop() {
    let mut desktop = LinuxDesktop::new().unwrap(); let observed = desktop.observe().unwrap();
    assert!(observed.screen.width > 0 && observed.screen.height > 0);
    assert!(observed.windows.iter().any(|w| w.id == observed.foreground));
    println!("{observed:?}");
}
