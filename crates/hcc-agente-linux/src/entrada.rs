use crate::comandos::Comandos;
use hcc_protocolo::Rect;

pub fn lua_string(text: &str) -> String {
    let mut literal = String::from("\"");
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b" ._/-".contains(&byte) { literal.push(char::from(byte)); }
        else { literal.push_str(&format!("\\{byte:03}")); }
    }
    literal.push('"');
    literal
}

pub fn evdev(names: &[String]) -> Result<Vec<u16>, String> {
    if !(1..=8).contains(&names.len()) { return Err("keys precisa conter de 1 a 8 teclas".into()); }
    names.iter().map(|name| {
        let key = name.to_lowercase();
        let code = match key.as_str() {
            "ctrl" | "control" => Some(29), "alt" => Some(56), "shift" => Some(42),
            "win" | "meta" => Some(125), "enter" => Some(28), "escape" => Some(1),
            "tab" => Some(15), "space" => Some(57), "backspace" => Some(14), "delete" => Some(111),
            "home" => Some(102), "end" => Some(107), "pageup" => Some(104), "pagedown" => Some(109),
            "arrowleft" => Some(105), "arrowright" => Some(106), "arrowup" => Some(103),
            "arrowdown" => Some(108), "insert" => Some(110), "0" => Some(11),
            _ => {
                if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()).filter(|n| key == format!("f{n}")) {
                    match n { 1..=10 => Some(58 + n), 11 => Some(87), 12 => Some(88), 13..=24 => Some(170 + n), _ => None }
                } else if key.len() == 1 {
                    let byte = key.as_bytes()[0];
                    if (b'1'..=b'9').contains(&byte) { Some(u16::from(byte - b'0') + 1) }
                    else { [("qwertyuiop",16),("asdfghjkl",30),("zxcvbnm",44)].iter()
                        .find_map(|(row, start)| row.find(char::from(byte)).map(|i| start + i as u16)) }
                } else { None }
            }
        };
        code.ok_or_else(|| format!("tecla desconhecida: {name}"))
    }).collect()
}

pub fn keys(commands: &impl Comandos, names: &[String]) -> Result<(), String> {
    let codes = evdev(names)?;
    let events: Vec<String> = codes.iter().map(|c| format!("{c}:1")).chain(codes.iter().rev().map(|c| format!("{c}:0"))).collect();
    let args: Vec<&str> = ["ydotool", "key"].into_iter().chain(events.iter().map(String::as_str)).collect();
    if let Err(error) = commands.run(&args, None) {
        let releases: Vec<String> = codes.iter().rev().map(|c| format!("{c}:0")).collect();
        let args: Vec<&str> = ["ydotool", "key"].into_iter().chain(releases.iter().map(String::as_str)).collect();
        if let Err(release) = commands.run(&args, None) { crate::ignorado("keys.release", &release); }
        return Err(error);
    }
    Ok(())
}

pub fn para_global(area: Rect, x: i32, y: i32) -> Result<(i32, i32), String> {
    let width = i64::from(area.2) - i64::from(area.0);
    let height = i64::from(area.3) - i64::from(area.1);
    if x < 0 || y < 0 || i64::from(x) >= width || i64::from(y) >= height { return Err("coordenada fora da tela".into()); }
    Ok((area.0.checked_add(x).ok_or("coordenada fora da tela")?, area.1.checked_add(y).ok_or("coordenada fora da tela")?))
}

pub(crate) fn shell_join(parts: &[&str]) -> String {
    parts.iter().map(|part| {
        if !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b)) { part.to_string() }
        else { format!("'{}'", part.replace('\'', "'\"'\"'")) }
    }).collect::<Vec<_>>().join(" ")
}
