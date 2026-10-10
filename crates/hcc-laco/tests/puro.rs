//! Loop logic that lives inline in laco_uia.executar, pinned with the cases the Python tests use.

use std::collections::HashSet;

use hcc_laco::barreiras::{Barreiras, aviso, objetivo_pede_fechar};
use hcc_laco::candidatos::{candidatos, segredos};
use hcc_laco::estado::{assinatura_tela, estado_compacto};
use hcc_laco::geometria::para_tela;
use hcc_laco::tipos::Registro;
use hcc_protocolo::{Action, ActionType, Button, Element, ElementAction, MouseMode, Observation, Rect, Screen, Window};
use serde_json::{Map, Value, json};

fn el(id: &str, name: &str, role: &str, actions: &[ElementAction], value: Option<&str>, rect: Option<Rect>) -> Element {
    Element {
        id: id.into(),
        name: name.into(),
        role: role.into(),
        value: value.map(Into::into),
        enabled: true,
        rect,
        focused: false,
        actions: actions.to_vec(),
        password: false,
    }
}

fn obs(elements: Vec<Element>) -> Observation {
    Observation {
        observation_id: "o1".into(),
        connected: true,
        session_id: 1,
        foreground: "w1".into(),
        windows: vec![Window { id: "w1".into(), name: "Editor".into(), process_id: 1, class_name: "TMainForm".into(), rect: Rect(0, 0, 800, 600) }],
        elements,
        apps: vec![],
        truncated: false,
        timestamp: 0.0,
        screen: Screen { width: 800, height: 600 },
    }
}

fn dados(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn reg(action: &str, changed: Option<bool>) -> Registro {
    Registro {
        action: action.into(),
        result: "ok".into(),
        screen_changed: changed,
        target_rect: None,
        clique: None,
        executada: action != "WAIT",
    }
}

fn clique(x: i32, y: i32) -> Registro {
    Registro { clique: Some((MouseMode::Click, x, y)), ..reg(&format!("mouse 'link Mais' click ({x},{y})"), Some(true)) }
}

fn mouse(x: i32, y: i32, mode: MouseMode) -> Action {
    Action { kind: ActionType::Mouse, x: Some(x), y: Some(y), button: Some(Button::Left), mode: Some(mode), ..Default::default() }
}

fn candidato(acao: Action) -> hcc_laco::tipos::Candidato {
    let o = obs(vec![]);
    let d = hcc_laco::descrever::descrever(&acao, &o, &HashSet::new(), true);
    let i = hcc_laco::descrever::intencao(&acao, &o);
    hcc_laco::tipos::Candidato { acao, descricao: d, intencao: i }
}

#[test]
fn installed_apps_are_offered_to_jev_and_omitted_when_empty() {
    let original = obs(vec![]);
    let mut wire = serde_json::to_value(&original).unwrap();
    assert!(wire.get("apps").is_none());
    let without = estado_compacto("abrir editor", &Map::new(), &original, &[], None);
    assert!(without.get("installedApps").is_none());
    wire["apps"] = json!(["Editor de Texto => gnome-text-editor"]);
    let installed: Observation = serde_json::from_value(wire).unwrap();
    let state = estado_compacto("abrir editor", &Map::new(), &installed, &[], None);
    assert_eq!(state["installedApps"], json!(["Editor de Texto => gnome-text-editor"]));
    let mut old_wire = serde_json::to_value(installed).unwrap();
    old_wire.as_object_mut().unwrap().remove("apps");
    let old_agent: Observation = serde_json::from_value(old_wire).unwrap();
    assert_eq!(estado_compacto("abrir editor", &Map::new(), &old_agent, &[], None), without);
    assert!(serde_json::to_value(old_agent).unwrap().get("apps").is_none());
}

#[test]
fn installed_apps_generate_launch_candidates_with_names() {
    let mut wire = serde_json::to_value(obs(vec![])).unwrap();
    wire["apps"] = json!(["Editor de Texto => gnome-text-editor", r#"Arquivo => "/opt/My App/editor""#, "invalid", " => "]);
    let installed: Observation = serde_json::from_value(wire).unwrap();
    let all = candidatos(&installed, &Map::new(), &HashSet::new());
    let launches: Vec<_> = all.iter().filter(|c| c.acao.kind == ActionType::Launch).collect();
    assert_eq!(launches.len(), 2);
    assert_eq!(launches[0].acao.application.as_deref(), Some("gnome-text-editor"));
    assert_eq!(launches[0].acao.rotulo.as_deref(), Some("Editor de Texto"));
    assert!(launches[0].descricao.contains("Editor de Texto"));
    assert!(launches[0].descricao.contains("gnome-text-editor"));
    assert_eq!(launches[0].intencao.aplicacao.as_deref(), Some("gnome-text-editor"));
    assert_eq!(launches[1].acao.application.as_deref(), Some("/opt/My App/editor"));
    let original = candidatos(&obs(vec![]), &Map::new(), &HashSet::new());
    let remaining: Vec<_> = all.iter().filter(|c| c.acao.kind != ActionType::Launch).map(|c| &c.acao).collect();
    assert_eq!(remaining, original.iter().map(|c| &c.acao).collect::<Vec<_>>());
}

#[test]
fn installed_name_with_separator_keeps_executable() {
    let mut wire = serde_json::to_value(obs(vec![])).unwrap();
    wire["apps"] = json!(["Conversor -> PDF => /usr/bin/editor"]);
    let installed: Observation = serde_json::from_value(wire).unwrap();
    let all = candidatos(&installed, &Map::new(), &HashSet::new());
    let launch = all.iter().find(|c| c.acao.kind == ActionType::Launch).unwrap();
    assert_eq!(launch.acao.application.as_deref(), Some("/usr/bin/editor"));
    assert_eq!(launch.acao.rotulo.as_deref(), Some("Conversor -> PDF"));
}

#[test]
fn launch_candidates_keep_exec_arguments() {
    for (entry, name, executable, args) in [
        ("A Flatpak => flatpak run org.x.App", "A Flatpak", "flatpak", vec!["run", "org.x.App"]),
        ("B Env => env VAR=x app", "B Env", "env", vec!["VAR=x", "app"]),
        (r#"C Quote => "/opt/My App/files""#, "C Quote", "/opt/My App/files", vec![]),
        ("Conversor -> PDF => app", "Conversor -> PDF", "app", vec![]),
        (r#"D Escape => app "a \"b\"""#, "D Escape", "app", vec![r#"a "b""#]),
        ("E Percent => app 100%", "E Percent", "app", vec!["100%"]),
        (r#"F Arrowarg => app "left => right""#, "F Arrowarg", "app", vec!["left => right"]),
    ] {
        let mut wire = serde_json::to_value(obs(vec![])).unwrap();
        wire["apps"] = json!([entry]);
        let installed: Observation = serde_json::from_value(wire).unwrap();
        let all = candidatos(&installed, &Map::new(), &HashSet::new());
        let launch = all.iter().find(|c| c.acao.kind == ActionType::Launch).unwrap();
        assert_eq!(launch.acao.application.as_deref(), Some(executable), "{entry}");
        assert_eq!(launch.acao.rotulo.as_deref(), Some(name), "{entry}");
        let expected_args = (!args.is_empty()).then(|| args.iter().map(|a| a.to_string()).collect());
        assert_eq!(launch.acao.args, expected_args, "{entry}");
        assert!(launch.descricao.contains(name), "{}", launch.descricao);
        for arg in args { assert!(launch.descricao.contains(arg), "{}", launch.descricao); }
    }
}

#[test]
fn malformed_installed_command_is_not_offered() {
    let mut wire = serde_json::to_value(obs(vec![])).unwrap();
    wire["apps"] = json!(["Broken => app \"unfinished", "Empty => \"\"", "Missing => "]);
    let installed: Observation = serde_json::from_value(wire).unwrap();
    assert!(!candidatos(&installed, &Map::new(), &HashSet::new()).iter().any(|c| c.acao.kind == ActionType::Launch));
}

#[test]
fn installed_apps_do_not_expose_secrets_in_compact_state() {
    let mut wire = serde_json::to_value(obs(vec![])).unwrap();
    wire["apps"] = json!(["Editor private-value => editor-private-value"]);
    let installed: Observation = serde_json::from_value(wire).unwrap();
    let state = estado_compacto("abrir editor", &dados(json!({"senha":"private-value"})), &installed, &[], None);
    assert_eq!(state["installedApps"], json!(["Editor <senha> => editor-<senha>"]));
    assert!(!state.to_string().contains("private-value"));
}

#[test]
fn secret_never_in_compact_state() {
    let d = dados(json!({"nome": "Ana", "senha": "s3gr3d0", "Token_API": 77}));
    let mut eco = el("e0", "s3gr3d0", "Edit", &[ElementAction::SetValue], Some("antes s3gr3d0 depois"), None);
    eco.password = true;
    let o = obs(vec![eco, el("e1", "77", "Text", &[], Some(&format!("{}x", "a".repeat(195) + "s3gr3d0")), None)]);
    let valor = estado_compacto("entrar", &d, &o, &[], None);
    let estado = serde_json::to_string(&valor).unwrap();
    assert!(!estado.contains("s3gr3"), "{estado}");
    assert!(!estado.contains("77"), "{estado}");
    assert!(estado.contains(r#""senha":"<senha>""#), "{estado}");
    assert!(estado.contains(r#""Token_API":"<senha>""#), "{estado}");
    assert_eq!(valor["elements"][1]["value"], format!("{}<senh", "a".repeat(195)));
}

#[test]
fn secret_masked_in_every_field() {
    let d = dados(json!({"usuario": "s3gr3d0", "senha": "s3gr3d0", "pin": "a"}));
    let mut o = obs(vec![el("e0", "Nome", "Edit", &[ElementAction::SetValue], Some("a"), None)]);
    o.windows[0].name = "Login s3gr3d0".into();
    o.windows.push(Window { id: "w2".into(), name: "s3gr3d0".into(), process_id: 2, class_name: "X".into(), rect: Rect(0, 0, 1, 1) });
    let recentes = [Registro { result: "NOT executed: s3gr3d0".into(), ..reg("invoke Text 's3gr3d0'", Some(false)) }];
    let valor = estado_compacto("entrar com s3gr3d0", &d, &o, &recentes, Some("vi s3gr3d0"));
    let estado = serde_json::to_string(&valor).unwrap();
    assert!(!estado.contains("s3gr3d0"), "{estado}");
    assert_eq!(valor["data"], json!({"usuario": "<senha>", "senha": "<senha>", "pin": "<senha>"}));
    assert_eq!(valor["window"], "Login <senha>");
    assert_eq!(valor["elements"][0]["value"], "<senha>");
    assert_eq!(valor["recentActions"][0]["action"], "invoke Text '<senha>'");
}

#[test]
fn hostile_rect_does_not_overflow() {
    let o = obs(vec![el("e0", "Menu", "MenuItem", &[ElementAction::Invoke], None, Some(Rect(i32::MAX - 1, 0, i32::MAX, 10)))]);
    let cands = candidatos(&o, &Map::new(), &HashSet::new());
    assert_eq!(cands[0].acao.x, Some(i32::MAX - 1));
}

#[test]
fn secret_values_python_str() {
    let d = dados(json!({"senha": "s", "pin": 12, "PASSWORD": true, "x_token": 1.0, "passwd": null, "nome": "n"}));
    let want: HashSet<String> = ["s", "12", "True", "1.0", "None"].into_iter().map(String::from).collect();
    assert_eq!(segredos(&d), want);
}

#[test]
fn password_value_is_masked_and_menu_item_is_real_click() {
    let mut senha = el("e1", "", "Edit", &[ElementAction::SetValue], None, None);
    senha.password = true;
    let o = obs(vec![
        el("e0", "Internação", "MenuItem", &[ElementAction::Expand, ElementAction::Invoke], None, Some(Rect(180, 23, 240, 42))),
        senha,
    ]);
    let d = dados(json!({"senha": "segredo"}));
    let s = segredos(&d);
    let cands = candidatos(&o, &d, &s);
    assert!(cands.iter().any(|c| c.acao == Action { target: Some("e0".into()), ..mouse(210, 32, MouseMode::Click) }));
    assert!(!cands.iter().any(|c| matches!(c.acao.kind, ActionType::Expand | ActionType::Invoke)));
    assert!(cands.iter().all(|c| !c.descricao.contains("segredo")));
    let text = Action { kind: ActionType::Text, value: Some("segredo".into()), ..Default::default() };
    assert!(!hcc_laco::descrever::descrever(&text, &o, &s, true).contains("segredo"));
}

#[test]
fn three_clicks_within_15px_are_one() {
    let hist = [clique(500, 80), clique(502, 80)];
    let cands = vec![
        candidato(mouse(504, 80, MouseMode::Click)),
        candidato(mouse(515, 95, MouseMode::Click)),
        candidato(mouse(518, 80, MouseMode::Click)),
        candidato(mouse(504, 80, MouseMode::Double)),
    ];
    let restam: Vec<_> = Barreiras::barrar(cands, &hist).into_iter().map(|c| c.acao).collect();
    assert_eq!(restam, vec![mouse(518, 80, MouseMode::Click), mouse(504, 80, MouseMode::Double)]);
    let um = Barreiras::barrar(vec![candidato(mouse(504, 80, MouseMode::Click))], &hist[..1]);
    assert_eq!(um.len(), 1);
}

#[test]
fn hostile_click_history_does_not_overflow() {
    let hist = [clique(-1, 0), clique(-1, 0)];
    assert_eq!(Barreiras::barrar(vec![candidato(mouse(i32::MAX, 0, MouseMode::Click))], &hist).len(), 1);
    let hist = [clique(i32::MAX, 0), clique(i32::MAX, 0)];
    assert_eq!(Barreiras::barrar(vec![candidato(mouse(i32::MIN, 0, MouseMode::Click))], &hist).len(), 1);
}

#[test]
fn twice_executed_is_barred() {
    let x = candidato(Action { kind: ActionType::Keys, keys: Some(vec!["Enter".into()]), ..Default::default() });
    let y = candidato(Action { kind: ActionType::Keys, keys: Some(vec!["Tab".into()]), ..Default::default() });
    let z = candidato(Action { kind: ActionType::Keys, keys: Some(vec!["Escape".into()]), ..Default::default() });
    let hist = [
        reg("keys Enter", Some(true)),
        reg("keys Tab", Some(true)),
        reg("WAIT", None),
        reg("keys Enter", Some(true)),
        reg("keys Escape", Some(true)),
    ];
    let restam: Vec<_> = Barreiras::barrar(vec![x.clone(), y.clone(), z.clone()], &hist).into_iter().map(|c| c.descricao).collect();
    assert_eq!(restam, vec!["keys Tab", "keys Escape"]);
    // Sem efeito nas últimas 3 executadas também sai; mais antiga que isso, não.
    let hist = [reg("keys Tab", Some(false)), reg("a", Some(true)), reg("b", Some(true)), reg("keys Escape", Some(false))];
    let restam: Vec<_> = Barreiras::barrar(vec![y, z], &hist).into_iter().map(|c| c.descricao).collect();
    assert_eq!(restam, vec!["keys Tab"]);
}

#[test]
fn three_without_effect_stops() {
    let hist = [reg("a", Some(false)), reg("b", Some(false)), reg("WAIT", None), reg("c", Some(false))];
    assert_eq!(Barreiras::tres_sem_efeito(&hist).as_deref(), Some("três ações seguidas sem efeito na tela: a; b; c"));
    assert_eq!(Barreiras::tres_sem_efeito(&hist[..3]), None);
    let hist = [reg("a", Some(false)), reg("b", None), reg("c", Some(false))];
    assert_eq!(Barreiras::tres_sem_efeito(&hist), None);
}

#[test]
fn reclique_offered_once_with_suffix() {
    let o = obs(vec![]);
    let mut hist = vec![
        Registro { target_rect: Some(Rect(10, 20, 31, 41)), ..reg("invoke MenuItem 'Cadastro'", Some(false)) },
        reg("WAIT", None),
    ];
    let c = Barreiras::reclique(&mut hist, &o).unwrap();
    assert_eq!(c.acao, mouse(20, 30, MouseMode::Click));
    assert_eq!(c.descricao, "mouse click (20,30)");
    assert_eq!(hist[0].result, "ok; a real mouse click on the same control is now offered");
    Barreiras::reclique(&mut hist, &o).unwrap();
    assert_eq!(hist[0].result, "ok; a real mouse click on the same control is now offered");
    hist[0].screen_changed = Some(true);
    assert!(Barreiras::reclique(&mut hist, &o).is_none());
    let mut sem_rect = vec![reg("keys Enter", Some(false))];
    assert!(Barreiras::reclique(&mut sem_rect, &o).is_none());
}

#[test]
fn consecutive_same_warning_not_counted() {
    let texto = "Unexpected Memory Leak detected";
    let caixa = obs(vec![el("e0", texto, "Text", &[], None, None), el("e1", "OK", "Button", &[ElementAction::Invoke], None, None)]);
    let limpa = obs(vec![el("e0", "Nome", "Edit", &[ElementAction::SetValue], Some("Ana"), None)]);
    let mut b = Barreiras::default();
    assert_eq!(b.aviso_repetido(&caixa), None);
    assert_eq!(b.aviso_repetido(&caixa), None);
    assert_eq!(b.aviso_repetido(&limpa), None);
    assert_eq!(
        b.aviso_repetido(&caixa).as_deref(),
        Some("o aplicativo respondeu duas vezes com o mesmo aviso: Unexpected Memory Leak detected")
    );
}

#[test]
fn warning_text_rules() {
    let longo = "x".repeat(500);
    let o = obs(vec![
        el("e0", "curto demais 15", "Text", &[], None, None),
        el("e1", "nome ignorado", "Edit", &[], Some("valor do campo vence"), None),
        el("e2", "Botão com nome comprido", "Button", &[], None, None),
        el("e3", &longo, "Text", &[], Some(""), None),
    ]);
    assert_eq!(aviso(&o), Some(format!("valor do campo vence {longo}")));
    let mut b = Barreiras::default();
    b.aviso_repetido(&o);
    b.aviso_repetido(&obs(vec![]));
    let motivo = b.aviso_repetido(&o).unwrap();
    assert_eq!(motivo.chars().count(), "o aplicativo respondeu duas vezes com o mesmo aviso: ".chars().count() + 400);
    let muitos = obs((0..26).map(|i| el(&format!("e{i}"), "um aviso bem comprido aqui", "Text", &[], None, None)).collect());
    assert_eq!(aviso(&muitos), None);
}

#[test]
fn goal_close_regex_requires_window_word() {
    assert!(!objetivo_pede_fechar("fechar a aba Relatório pelo X dela"));
    assert!(!objetivo_pede_fechar("fechar a aba Escalas pelo X dela"));
    assert!(objetivo_pede_fechar("fechar o aplicativo"));
    assert!(objetivo_pede_fechar("Encerrar\na Aplicação"));
    assert!(objetivo_pede_fechar("please QUIT the app"));
    assert!(!objetivo_pede_fechar("abrir a janela e salvar"));
    assert!(!objetivo_pede_fechar("desfechar o aplicativo"));
}

#[test]
fn scaling_rounds_half_to_even() {
    let tela = Screen { width: 1920, height: 1080 };
    assert_eq!(para_tela(&mouse(695, 1, MouseMode::Click), &tela, "").x, Some(1042));
    assert_eq!(para_tela(&mouse(697, 3, MouseMode::Click), &tela, ""), mouse(1046, 4, MouseMode::Click));
    let pequena = Screen { width: 1024, height: 768 };
    assert_eq!(para_tela(&mouse(695, 1, MouseMode::Click), &pequena, ""), mouse(695, 1, MouseMode::Click));
}

#[test]
fn signature_includes_foreground_title() {
    let antes = obs(vec![]);
    let mut depois = antes.clone();
    assert_eq!(assinatura_tela(&antes), assinatura_tela(&depois));
    depois.windows[0].name = "Example Domain - Google Chrome".into();
    assert_ne!(assinatura_tela(&antes), assinatura_tela(&depois));
    depois.foreground = "w9".into();
    assert_eq!(assinatura_tela(&depois).1, "");
}
