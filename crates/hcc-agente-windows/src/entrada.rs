//! Keyboard and mouse input through SendInput.

use crate::regras::{self, Gesto, Tecla, Toque, runtime_err};
use hcc_protocolo::{Button, DesktopError};
use std::time::Duration;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, MapVirtualKeyW, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN};

fn enviar(entradas: &[INPUT]) -> Result<(), DesktopError> {
    // SAFETY: a slice of fully initialised INPUT structs with their real size.
    let n = unsafe { SendInput(entradas, std::mem::size_of::<INPUT>() as i32) };
    if n as usize != entradas.len() {
        return Err(runtime_err(format!("SendInput falhou: {}", windows::core::Error::from_thread().message())));
    }
    Ok(())
}

fn teclado(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

fn mouse(dx: i32, dy: i32, dados: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: dados as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

fn virtual_key(vk: u16, baixo: bool, estendida: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if estendida {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !baixo {
        flags |= KEYEVENTF_KEYUP;
    }
    // Consoles and nested RDP read the scan code, not the virtual key.
    // SAFETY: pure table lookup.
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    teclado(vk, scan, flags)
}

pub(crate) fn tecla(t: &Tecla, baixo: bool) -> Result<(), DesktopError> {
    enviar(&[virtual_key(t.vk, baixo, t.estendida)])
}

/// Alt down/up in one call: counts as user input for the foreground lock.
pub(crate) fn tocar_alt() {
    let _ = enviar(&[virtual_key(0x12, true, false), virtual_key(0x12, false, false)]);
}

/// Types `valor` like pywinauto's vk_packet: each UTF-16 unit as a Unicode packet, layout-independent.
pub(crate) fn digitar(valor: &str, pausa: Duration) -> Result<(), DesktopError> {
    for toque in regras::texto(valor) {
        let par = match toque {
            Toque::Unicode(u) => [teclado(0, u, KEYEVENTF_UNICODE), teclado(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)],
            Toque::Tecla(vk) => [virtual_key(vk, true, false), virtual_key(vk, false, false)],
        };
        enviar(&par)?;
        if !pausa.is_zero() {
            std::thread::sleep(pausa);
        }
    }
    Ok(())
}

/// Where a pixel goes: the primary screen for `mouse` actions, the virtual desktop for element clicks.
#[derive(Clone, Copy)]
struct Area {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    flags: MOUSE_EVENT_FLAGS,
}

impl Area {
    fn primaria(w: i32, h: i32) -> Area {
        Area { x: 0, y: 0, w, h, flags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE }
    }

    fn virtual_() -> Area {
        // SAFETY: argument-less metrics.
        let (x, y, w, h) = unsafe {
            (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN), GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN))
        };
        Area { x, y, w, h, flags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK }
    }

    fn mover(self, x: i32, y: i32) -> INPUT {
        mouse(regras::absoluto_em(x, self.x, self.w), regras::absoluto_em(y, self.y, self.h), 0, self.flags)
    }
}

fn botao(b: Button, baixo: bool) -> INPUT {
    let flags = match (b, baixo) {
        (Button::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (Button::Left, false) => MOUSEEVENTF_LEFTUP,
        (Button::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (Button::Right, false) => MOUSEEVENTF_RIGHTUP,
        (Button::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (Button::Middle, false) => MOUSEEVENTF_MIDDLEUP,
    };
    mouse(0, 0, 0, flags)
}

/// Move, press and release in one SendInput: atomic, so the button can't stay down between calls.
fn clicar(area: Area, b: Button, x: i32, y: i32) -> Result<(), DesktopError> {
    enviar(&[area.mover(x, y), botao(b, true), botao(b, false)])
}

/// Releases the button if the drag fails midway; the success path releases explicitly.
struct Solta(Option<Button>);

impl Drop for Solta {
    fn drop(&mut self) {
        if let Some(b) = self.0 {
            let _ = enviar(&[botao(b, false)]);
        }
    }
}

pub(crate) fn gesto(g: Gesto, w: i32, h: i32) -> Result<(), DesktopError> {
    let area = Area::primaria(w, h);
    match g {
        Gesto::Clique { botao: b, x, y, duplo } => {
            clicar(area, b, x, y)?;
            if duplo {
                clicar(area, b, x, y)?;
            }
            Ok(())
        }
        Gesto::Mover { x, y } => enviar(&[area.mover(x, y)]),
        Gesto::Rolar { x, y, roda } => enviar(&[area.mover(x, y), mouse(0, 0, roda, MOUSEEVENTF_WHEEL)]),
        Gesto::Arrastar { botao: b, x, y, x2, y2 } => {
            enviar(&[area.mover(x, y), botao(b, true)])?;
            let mut solta = Solta(Some(b));
            // pywinauto moves over 0.2 s; some apps only start a drag after intermediate moves.
            const PASSOS: i32 = 10;
            for i in 1..=PASSOS {
                enviar(&[area.mover(x + (x2 - x) * i / PASSOS, y + (y2 - y) * i / PASSOS)])?;
                std::thread::sleep(Duration::from_millis(20));
            }
            enviar(&[botao(b, false)])?;
            solta.0 = None;
            Ok(())
        }
    }
}

/// Real click at the centre of the control, as a person would (pywinauto click_input); the
/// control may sit on any monitor.
pub(crate) fn clicar_centro(rect: (i32, i32, i32, i32)) -> Result<(), DesktopError> {
    let (l, t, r, b) = rect;
    clicar(Area::virtual_(), Button::Left, l + (r - l) / 2, t + (b - t) / 2)
}
