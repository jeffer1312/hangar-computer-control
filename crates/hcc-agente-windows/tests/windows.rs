//! Pure rules of the Windows agent; runs on every OS (port of the pure parts of test_desktop_agent.py).

use hcc_agente_windows::regras::*;
use hcc_protocolo::{Action, ActionType, Amount, Button, DesktopError, Direction, ErrorKind, MouseMode};

fn nomes(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn codigos(v: &[&str]) -> Vec<String> {
    teclas(Some(&nomes(v))).unwrap().into_iter().map(|t| t.nome).collect()
}

#[test]
fn letters_and_digits_use_supported_syntax() {
    assert_eq!(codigos(&["CTRL", "SHIFT", "S"]), ["VK_CONTROL", "VK_SHIFT", "s"]);
    assert_eq!(codigos(&["Ctrl", "1"]), ["VK_CONTROL", "1"]);
    let s = teclas(Some(&nomes(&["S"]))).unwrap();
    assert_eq!(s[0].vk, 0x53);
    assert_eq!(teclas(Some(&nomes(&["z"]))).unwrap()[0].vk, 0x5A);
    assert_eq!(teclas(Some(&nomes(&["0"]))).unwrap()[0].vk, 0x30);
    assert_eq!(teclas(Some(&nomes(&["9"]))).unwrap()[0].vk, 0x39);
}

#[test]
fn every_alias_maps_like_the_python() {
    let casos = [
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
        ("ArrowDown", "DOWN", 0x28),
    ];
    for (nome, codigo, vk) in casos {
        let t = &teclas(Some(&nomes(&[nome]))).unwrap()[0];
        assert_eq!((t.nome.as_str(), t.vk), (codigo, vk), "{nome}");
    }
}

#[test]
fn function_keys_f1_to_f24() {
    assert_eq!(codigos(&["f1", "F12", "f24"]), ["F1", "F12", "F24"]);
    assert_eq!(teclas(Some(&nomes(&["F1"]))).unwrap()[0].vk, 0x70);
    assert_eq!(teclas(Some(&nomes(&["F24"]))).unwrap()[0].vk, 0x87);
    for invalida in ["F0", "F25", "F01", "f+5", "F 5"] {
        assert_eq!(teclas(Some(&nomes(&[invalida]))).unwrap_err().to_string(), format!("ValueError: tecla desconhecida: {invalida}"));
    }
}

#[test]
fn invalid_sequence_is_rejected_before_any_input() {
    let mut enviados = Vec::new();
    let resultado = teclas(Some(&nomes(&["Ctrl", "ç"]))).and_then(|t| acorde(&t, |t, baixo| {
        enviados.push((t.nome.clone(), baixo));
        Ok(())
    }));
    assert_eq!(resultado.unwrap_err().to_string(), "ValueError: tecla desconhecida: ç");
    assert!(enviados.is_empty());
    for invalida in [vec![], vec!["a"; 9]] {
        let e = teclas(Some(&nomes(&invalida))).unwrap_err();
        assert_eq!(e.to_string(), "ValueError: keys precisa conter de 1 a 8 teclas");
    }
    assert_eq!(teclas(None).unwrap_err().to_string(), "ValueError: keys precisa conter de 1 a 8 teclas");
    for invalida in ["", "ctrl+s", "é", "-"] {
        assert!(teclas(Some(&nomes(&[invalida]))).is_err(), "{invalida}");
    }
}

#[test]
fn chord_presses_in_order_and_releases_in_reverse() {
    let t = teclas(Some(&nomes(&["Ctrl", "Shift", "S"]))).unwrap();
    let mut enviados = Vec::new();
    acorde(&t, |t, baixo| {
        enviados.push(format!("{{{} {}}}", t.nome, if baixo { "down" } else { "up" }));
        Ok(())
    })
    .unwrap();
    assert_eq!(enviados, ["{VK_CONTROL down}", "{VK_SHIFT down}", "{s down}", "{s up}", "{VK_SHIFT up}", "{VK_CONTROL up}"]);
}

#[test]
fn keyup_failure_does_not_leave_modifiers_pressed() {
    let t = teclas(Some(&nomes(&["Ctrl", "Shift", "S"]))).unwrap();
    let mut enviados = Vec::new();
    let e = acorde(&t, |t, baixo| {
        enviados.push(format!("{{{} {}}}", t.nome, if baixo { "down" } else { "up" }));
        if t.nome == "s" && !baixo {
            return Err(DesktopError { kind: ErrorKind::RuntimeError, msg: "falha de envio".into() });
        }
        Ok(())
    })
    .unwrap_err();
    assert!(enviados.contains(&"{VK_SHIFT up}".to_string()));
    assert!(enviados.contains(&"{VK_CONTROL up}".to_string()));
    assert_eq!(e.to_string(), "RuntimeError: falha ao liberar teclas: s: falha de envio");
}

#[test]
fn keydown_failure_releases_what_was_pressed_and_reports_the_press_error() {
    let t = teclas(Some(&nomes(&["Ctrl", "Shift", "S"]))).unwrap();
    let mut enviados = Vec::new();
    let e = acorde(&t, |t, baixo| {
        enviados.push(format!("{{{} {}}}", t.nome, if baixo { "down" } else { "up" }));
        if t.nome == "VK_SHIFT" && baixo {
            return Err(DesktopError { kind: ErrorKind::RuntimeError, msg: "bloqueado".into() });
        }
        Ok(())
    })
    .unwrap_err();
    // Python appends the key before sending it, so the one that failed is released too.
    assert_eq!(enviados, ["{VK_CONTROL down}", "{VK_SHIFT down}", "{VK_SHIFT up}", "{VK_CONTROL up}"]);
    assert_eq!(e.to_string(), "RuntimeError: bloqueado");
}

#[test]
fn extended_keys_are_flagged() {
    for nome in ["delete", "home", "end", "pageup", "pagedown", "arrowleft", "arrowright", "arrowup", "arrowdown", "insert", "win"] {
        assert!(teclas(Some(&nomes(&[nome]))).unwrap()[0].estendida, "{nome}");
    }
    for nome in ["ctrl", "shift", "enter", "a", "F5", "space", "backspace"] {
        assert!(!teclas(Some(&nomes(&[nome]))).unwrap()[0].estendida, "{nome}");
    }
}

#[test]
fn text_is_unicode_with_newlines_as_enter() {
    let toques = texto("aç\n😀");
    assert_eq!(
        toques,
        [Toque::Unicode('a' as u16), Toque::Unicode(0xE7), Toque::Tecla(0x0D), Toque::Unicode(0xD83D), Toque::Unicode(0xDE00)]
    );
    assert_eq!(texto("a\tb"), [Toque::Unicode('a' as u16), Toque::Unicode('b' as u16)]);
    assert_eq!(texto("a\r\nb"), [Toque::Unicode('a' as u16), Toque::Tecla(0x0D), Toque::Unicode('b' as u16)]);
    // Characters that were special to pywinauto's send_keys go through literally.
    assert_eq!(texto("+^%~(){}"), "+^%~(){}".chars().map(|c| Toque::Unicode(c as u16)).collect::<Vec<_>>());
}

#[test]
fn real_click_decision_per_role() {
    for role in ["Button", "MenuItem", "TabItem", "CheckBox", "Hyperlink", "SplitButton"] {
        assert!(deve_clicar(role, true), "{role}");
        assert!(!deve_clicar(role, false), "{role}");
    }
    for role in ["ListItem", "TreeItem", "DataItem"] {
        assert!(!deve_clicar(role, true), "{role}");
        assert!(!deve_clicar(role, false), "{role}");
    }
}

#[test]
fn on_screen_needs_size_and_not_offscreen() {
    assert!(na_tela([10, 10, 20, 20], false));
    assert!(!na_tela([10, 10, 10, 20], false));
    assert!(!na_tela([10, 10, 20, 10], false));
    assert!(!na_tela([10, 10, 20, 20], true));
}

#[test]
fn popup_root_rule() {
    assert!(e_popup(true, "TPopupMenu", true, true));
    assert!(e_popup(false, "#32768", true, true));
    assert!(!e_popup(false, "TMainForm", true, true), "second main window of the same process has no owner");
    assert!(!e_popup(true, "TMessageForm", false, true), "disabled window is behind a modal");
    assert!(!e_popup(true, "TMessageForm", true, false), "other process");
    assert!(!e_popup(false, "#32768", true, false));
}

#[test]
fn scroll_amount_mapping() {
    use Amount::*;
    use Direction::*;
    assert_eq!(quantidade_scroll(Up, Large), (2, 0));
    assert_eq!(quantidade_scroll(Up, Small), (2, 1));
    assert_eq!(quantidade_scroll(Down, Large), (2, 3));
    assert_eq!(quantidade_scroll(Down, Small), (2, 4));
    assert_eq!(quantidade_scroll(Left, Large), (0, 2));
    assert_eq!(quantidade_scroll(Left, Small), (1, 2));
    assert_eq!(quantidade_scroll(Right, Large), (3, 2));
    assert_eq!(quantidade_scroll(Right, Small), (4, 2));
}

#[test]
fn set_value_read_back_normalisation() {
    assert!(valor_confirmado("linha 1\r\nlinha 2  ", "linha 1\nlinha 2"));
    assert!(valor_confirmado("  ação\n", "ação"));
    assert!(valor_confirmado("a\r\nb", "a\r\nb"));
    assert!(!valor_confirmado("antigo", "novo"));
    assert!(!valor_confirmado("", "x"));
}

#[test]
fn unconfirmed_value_message_uses_python_repr_of_80_chars() {
    assert_eq!(valor_nao_confirmado("antigo").to_string(), "RuntimeError: valor digitado não confirmado; campo ficou com 'antigo'");
    assert_eq!(valor_nao_confirmado("it's").msg, "valor digitado não confirmado; campo ficou com \"it's\"");
    assert_eq!(valor_nao_confirmado("a'\"b").msg, "valor digitado não confirmado; campo ficou com 'a\\'\"b'");
    assert_eq!(valor_nao_confirmado("l1\r\nl2\t\\").msg, "valor digitado não confirmado; campo ficou com 'l1\\r\\nl2\\t\\\\'");
    assert_eq!(valor_nao_confirmado("\u{1}").msg, "valor digitado não confirmado; campo ficou com '\\x01'");
    assert_eq!(valor_nao_confirmado("a\u{a0}b\u{200b}\u{feff}").msg, "valor digitado não confirmado; campo ficou com 'a\\xa0b\\u200b\\ufeff'");
    let longo = "é".repeat(100);
    assert_eq!(valor_nao_confirmado(&longo).msg, format!("valor digitado não confirmado; campo ficou com '{}'", "é".repeat(80)));
}

fn mouse(x: i32, y: i32, mode: MouseMode) -> Action {
    Action { kind: ActionType::Mouse, x: Some(x), y: Some(y), button: Some(Button::Left), mode: Some(mode), ..Default::default() }
}

#[test]
fn mouse_bounds_are_the_primary_screen() {
    let erro = |a: &Action| gesto(a, 1920, 1080).unwrap_err().to_string();
    assert_eq!(gesto(&mouse(0, 0, MouseMode::Click), 1920, 1080).unwrap(), Gesto::Clique { botao: Button::Left, x: 0, y: 0, duplo: false });
    assert_eq!(gesto(&mouse(1919, 1079, MouseMode::Double), 1920, 1080).unwrap(), Gesto::Clique { botao: Button::Left, x: 1919, y: 1079, duplo: true });
    for (x, y) in [(1920, 0), (0, 1080), (-1, 0), (0, -1)] {
        assert_eq!(erro(&mouse(x, y, MouseMode::Click)), "ValueError: coordenada fora da tela");
    }
    assert_eq!(erro(&Action { x: None, ..mouse(0, 0, MouseMode::Click) }), "ValueError: coordenada fora da tela");
    assert_eq!(gesto(&mouse(5, 6, MouseMode::Move), 1920, 1080).unwrap(), Gesto::Mover { x: 5, y: 6 });
}

#[test]
fn mouse_drag_destination_and_scroll_delta() {
    let arrasto = Action { x2: Some(100), y2: Some(200), button: Some(Button::Right), ..mouse(1, 2, MouseMode::Drag) };
    assert_eq!(gesto(&arrasto, 1920, 1080).unwrap(), Gesto::Arrastar { botao: Button::Right, x: 1, y: 2, x2: 100, y2: 200 });
    let fora = Action { x2: Some(1920), ..arrasto.clone() };
    assert_eq!(gesto(&fora, 1920, 1080).unwrap_err().to_string(), "ValueError: destino fora da tela");
    let sem = Action { y2: None, ..arrasto };
    assert_eq!(gesto(&sem, 1920, 1080).unwrap_err().to_string(), "ValueError: destino fora da tela");
    // Positive delta scrolls down: the wheel moves by -delta notches of 120.
    let rolar = Action { delta: Some(3), ..mouse(10, 10, MouseMode::Scroll) };
    assert_eq!(gesto(&rolar, 1920, 1080).unwrap(), Gesto::Rolar { x: 10, y: 10, roda: -360 });
    let rolar = Action { delta: None, ..mouse(10, 10, MouseMode::Scroll) };
    assert_eq!(gesto(&rolar, 1920, 1080).unwrap(), Gesto::Rolar { x: 10, y: 10, roda: 0 });
    let rolar = Action { delta: Some(-51), ..mouse(10, 10, MouseMode::Scroll) };
    assert_eq!(gesto(&rolar, 1920, 1080).unwrap_err().to_string(), "ValueError: delta inválido");
}

#[test]
fn absolute_coordinates_land_on_the_same_pixel() {
    for largura in [800, 1366, 1920, 2560, 3840] {
        assert_eq!(absoluto(0, largura), 0);
        assert!(absoluto(largura - 1, largura) <= 65535);
        for x in 0..largura {
            // SendInput maps a normalised n back to the pixel floor(n * largura / 65536).
            assert_eq!(absoluto(x, largura) as i64 * largura as i64 / 65536, x as i64, "{x}/{largura}");
        }
    }
}

#[test]
fn element_clicks_use_the_virtual_screen() {
    // Secondary monitor to the left: virtual screen starts at -1280 and spans 3200 px.
    for x in [-1280, -1, 0, 1919] {
        let n = absoluto_em(x, -1280, 3200);
        assert!((0..=65535).contains(&n));
        assert_eq!(n as i64 * 3200 / 65536 - 1280, x as i64, "{x}");
    }
    assert_eq!(absoluto_em(5, 0, 1920), absoluto(5, 1920));
}

#[test]
fn values_are_cut_at_4000_characters() {
    assert_eq!(corta(&"ç".repeat(4001), 4000), "ç".repeat(4000));
    assert_eq!(corta("abc", 4000), "abc");
}

#[test]
fn screenshot_is_reduced_to_1280_wide_opaque_png() {
    assert_eq!(tamanho_reduzido(2560, 1440), (1280, 720));
    assert_eq!(tamanho_reduzido(1280, 1024), (1280, 1024));
    assert_eq!(tamanho_reduzido(800, 600), (800, 600));
    // 1366x768 → 768 * 1280 / 1366 = 719.62 → 720.
    assert_eq!(tamanho_reduzido(1366, 768), (1280, 720));
    // GDI hands BGRA with alpha 0: the PNG must not come out transparent.
    let rgba = [10u8, 20, 30, 0].repeat(1600 * 900);
    let png = png_reduzido(rgba, 1600, 900).unwrap();
    let img = image::load_from_memory(&png).unwrap();
    assert_eq!((img.width(), img.height()), (1280, 720));
    assert!(!img.color().has_alpha());
    assert_eq!(img.to_rgb8().get_pixel(0, 0).0, [10, 20, 30]);
}

#[cfg(windows)]
#[test]
#[ignore = "desktop real"]
fn observe_real_desktop() {
    use hcc_protocolo::Desktop;
    let mut desktop = hcc_agente_windows::WindowsDesktop::new().unwrap();
    let obs = desktop.observe().unwrap();
    println!("session_id={} foreground={} windows={} elements={} truncated={}", obs.session_id, obs.foreground, obs.windows.len(), obs.elements.len(), obs.truncated);
    for w in &obs.windows {
        println!("{} {:?} {}", w.id, w.name, w.class_name);
    }
    assert!(obs.windows.iter().any(|w| w.id == obs.foreground));
    assert!(obs.screen.width > 0 && obs.screen.height > 0);
}
