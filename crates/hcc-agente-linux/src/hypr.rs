use crate::comandos::Comandos;
use hcc_protocolo::{Rect, Window};
use serde_json::Value;

pub(crate) fn query(commands: &impl Comandos, name: &str) -> Result<Value, String> {
    let bytes = commands.run(&["hyprctl", "-j", name], None)?;
    serde_json::from_str(&String::from_utf8_lossy(&bytes)).map_err(|e| e.to_string())
}

pub fn dispatch(commands: &impl Comandos, lua: &str) -> Result<(), String> {
    let bytes = commands.run(&["hyprctl", "dispatch", lua], None)?;
    let output = String::from_utf8_lossy(&bytes);
    if output.trim() != "ok" {
        return Err(format!("hyprctl dispatch falhou: {}", output.trim().chars().take(300).collect::<String>()));
    }
    Ok(())
}

pub(crate) fn integer(value: &Value, key: &str) -> Result<i32, String> {
    value[key].as_i64().and_then(|v| i32::try_from(v).ok()).ok_or_else(|| format!("hyprctl retornou {key} inválido"))
}

pub(crate) fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key].as_str().ok_or_else(|| format!("hyprctl retornou {key} inválido"))
}

pub(crate) fn pair(value: &Value, key: &str) -> Result<(i32, i32), String> {
    let values = value[key].as_array().filter(|v| v.len() == 2).ok_or_else(|| format!("hyprctl retornou {key} inválido"))?;
    let convert = |v: &Value| v.as_i64().and_then(|v| i32::try_from(v).ok()).ok_or_else(|| format!("hyprctl retornou {key} inválido"));
    Ok((convert(&values[0])?, convert(&values[1])?))
}

pub fn monitor_logico(monitor: &Value) -> Result<Rect, String> {
    let (mut w, mut h) = (integer(monitor, "width")?, integer(monitor, "height")?);
    if monitor["transform"].as_i64().unwrap_or(0) % 2 != 0 { std::mem::swap(&mut w, &mut h); }
    let scale = monitor["scale"].as_f64().filter(|s| s.is_finite() && *s > 0.0).ok_or("hyprctl retornou scale inválido")?;
    let (x, y) = (integer(monitor, "x")?, integer(monitor, "y")?);
    let (width, height) = ((f64::from(w) / scale).round_ties_even(), (f64::from(h) / scale).round_ties_even());
    if width < 1.0 || height < 1.0 || width > f64::from(i32::MAX) || height > f64::from(i32::MAX) {
        return Err("hyprctl retornou tamanho de monitor inválido".into());
    }
    Ok(Rect(x, y, x.checked_add(width as i32).ok_or("coordenada fora da tela")?, y.checked_add(height as i32).ok_or("coordenada fora da tela")?))
}

pub fn reference_monitor<'a>(monitors: &'a Value, active: &Value) -> Result<&'a Value, String> {
    monitors.as_array().and_then(|monitors| monitors.iter().find(|m| {
        if active["address"].as_str().is_some_and(|a| !a.is_empty()) { m["id"] == active["monitor"] }
        else { m["focused"].as_bool().unwrap_or(false) }
    })).ok_or_else(|| "nenhum monitor focado no hyprctl".into())
}

pub(crate) fn client_rect(client: &Value, origin: (i32, i32)) -> Result<Rect, String> {
    let (x, y) = pair(client, "at")?;
    let (w, h) = pair(client, "size")?;
    let x = x.checked_sub(origin.0).ok_or("coordenada fora da tela")?;
    let y = y.checked_sub(origin.1).ok_or("coordenada fora da tela")?;
    Ok(Rect(x, y, x.checked_add(w).ok_or("coordenada fora da tela")?, y.checked_add(h).ok_or("coordenada fora da tela")?))
}

pub fn windows_from(clients: &Value, origin: (i32, i32)) -> Result<Vec<Window>, String> {
    clients.as_array().ok_or("hyprctl retornou clients inválido")?.iter()
        .filter(|c| c["mapped"].as_bool().unwrap_or(false) && !c["hidden"].as_bool().unwrap_or(false))
        .map(|c| Ok(Window {
            id: format!("w{}", string(c, "address")?), name: string(c, "title")?.into(),
            process_id: c["pid"].as_u64().and_then(|v| u32::try_from(v).ok()).ok_or("hyprctl retornou pid inválido")?,
            class_name: string(c, "class")?.into(), rect: client_rect(c, origin)?,
        })).collect()
}
