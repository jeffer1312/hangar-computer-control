//! Windows session checks (windows_uia.py:20-61).

use crate::regras::runtime_err;
use hcc_protocolo::DesktopError;
use windows::Win32::Foundation::{HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTS_CONNECTSTATE_CLASS, WTSActive, WTSConnectState, WTSFreeMemory, WTSQuerySessionInformationW};
use windows::Win32::System::StationsAndDesktops::{CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, OpenInputDesktop};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId, WaitForSingleObject};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetShellWindow, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
use windows::core::{HSTRING, PWSTR};

fn win(e: windows::core::Error) -> DesktopError {
    let c = e.code().0 as u32;
    // HRESULT_FROM_WIN32 (0x8007xxxx) carries the Win32 code Python's WinError shows.
    let codigo = if c & 0xFFFF_0000 == 0x8007_0000 { (c & 0xFFFF) as i64 } else { e.code().0 as i64 };
    runtime_err(format!("[WinError {codigo}] {}", e.message()))
}

pub(crate) fn dpi_por_monitor() {
    // Fails only when the manifest already set it; either way the process is DPI aware.
    // SAFETY: process-wide setting with a predefined context.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

pub(crate) struct Sessao {
    pub id: u32,
    // Held for the life of the process: it marks this session as taken.
    _mutex: HANDLE,
}

impl Sessao {
    pub fn abrir() -> Result<Sessao, DesktopError> {
        let mut id = 0u32;
        // SAFETY: `id` outlives the call.
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut id) }.map_err(win)?;
        if id == 0 {
            return Err(runtime_err("o agente deve rodar na sessão gráfica, não na sessão 0"));
        }
        let nome = HSTRING::from(format!("Local\\HangarComputerControl-{id}"));
        // SAFETY: named mutex with default security; the handle is kept, never closed.
        let mutex = unsafe { CreateMutexW(None, false, &nome) }.map_err(|_| runtime_err("outro agente já controla esta sessão Windows"))?;
        // SAFETY: valid handle from CreateMutexW; zero timeout.
        let espera = unsafe { WaitForSingleObject(mutex, 0) };
        if espera != WAIT_OBJECT_0 && espera != WAIT_ABANDONED {
            return Err(runtime_err("outro agente já controla esta sessão Windows"));
        }
        Ok(Sessao { id, _mutex: mutex })
    }

    /// Active (not disconnected) session and an input desktop we can read (not locked, not UAC).
    pub fn disponivel(&self) -> Result<(), DesktopError> {
        let mut estado = PWSTR::null();
        let mut tamanho = 0u32;
        // SAFETY: out-pointers outlive the call; the buffer is freed below with WTSFreeMemory.
        unsafe { WTSQuerySessionInformationW(None, self.id, WTSConnectState, &mut estado, &mut tamanho) }.map_err(win)?;
        // SAFETY: for WTSConnectState the buffer holds one WTS_CONNECTSTATE_CLASS.
        let ativo = unsafe { *(estado.0 as *const WTS_CONNECTSTATE_CLASS) } == WTSActive;
        // SAFETY: buffer allocated by WTSQuerySessionInformationW.
        unsafe { WTSFreeMemory(estado.0 as *mut _) };
        if !ativo {
            return Err(runtime_err("sessão Windows desconectada"));
        }
        // SAFETY: the desktop handle is closed right after.
        match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) } {
            Ok(d) if !d.is_invalid() => {
                // SAFETY: handle just opened.
                let _ = unsafe { CloseDesktop(d) };
                Ok(())
            }
            _ => Err(runtime_err("desktop bloqueado, desconectado ou protegido por UAC")),
        }
    }
}

/// Foreground window, or the shell window on an empty desktop (Program Manager plays the part).
pub(crate) fn primeiro_plano() -> isize {
    // SAFETY: argument-less queries.
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_invalid() { unsafe { GetShellWindow() }.0 as isize } else { fg.0 as isize }
}

/// Primary screen in physical pixels.
pub(crate) fn tela() -> (i32, i32) {
    // SAFETY: argument-less metrics.
    unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
}
