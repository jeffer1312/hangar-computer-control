//! The drawn cursor for `ver`: a click-through, never-focused topmost window with an arrow that glides to the
//! target before the action, so the user sees where the agent is about to act.

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreatePen, CreateSolidBrush, DeleteObject, EndPaint, FillRect, PAINTSTRUCT, PS_SOLID, Polygon, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, HWND_TOPMOST, LWA_COLORKEY, MSG, PM_REMOVE, PeekMessageW,
    RegisterClassW, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOSIZE, SetLayeredWindowAttributes, SetWindowPos, ShowWindow, TranslateMessage,
    WINDOW_EX_STYLE, WM_PAINT, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::w;

const LADO: i32 = 32;
/// Painted in this color, the background disappears (LWA_COLORKEY).
const FUNDO: COLORREF = COLORREF(0x00FF00FF);

pub(crate) struct Cursor {
    hwnd: HWND,
    onde: POINT,
}

unsafe extern "system" fn desenhar(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_PAINT {
        // SAFETY: standard WM_PAINT handling on our own window; every GDI object made here is deleted.
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            let fundo = CreateSolidBrush(FUNDO);
            FillRect(dc, &RECT { left: 0, top: 0, right: LADO, bottom: LADO }, fundo);
            let preenchimento = CreateSolidBrush(COLORREF(0x001E1EE6));
            let borda = CreatePen(PS_SOLID, 2, COLORREF(0x00FFFFFF));
            let (b0, p0) = (SelectObject(dc, preenchimento.into()), SelectObject(dc, borda.into()));
            let seta = [POINT { x: 1, y: 1 }, POINT { x: 1, y: 24 }, POINT { x: 8, y: 18 }, POINT { x: 13, y: 29 },
                POINT { x: 17, y: 27 }, POINT { x: 12, y: 17 }, POINT { x: 21, y: 17 }];
            let _ = Polygon(dc, &seta);
            SelectObject(dc, b0);
            SelectObject(dc, p0);
            let _ = DeleteObject(preenchimento.into());
            let _ = DeleteObject(borda.into());
            let _ = DeleteObject(fundo.into());
            let _ = EndPaint(hwnd, &ps);
        }
        return LRESULT(0);
    }
    // SAFETY: default handling of every other message.
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

impl Cursor {
    /// Created where the real pointer is, so the first glide starts from a natural place.
    pub(crate) fn novo() -> Option<Cursor> {
        // SAFETY: class registration and window creation on this thread; failures return None.
        unsafe {
            let instancia = GetModuleHandleW(None).ok()?;
            let classe = WNDCLASSW { lpfnWndProc: Some(desenhar), hInstance: instancia.into(), lpszClassName: w!("HccCursor"), ..Default::default() };
            RegisterClassW(&classe);
            let estilo = WINDOW_EX_STYLE(WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0 | WS_EX_TOPMOST.0 | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0);
            let mut onde = POINT::default();
            let _ = GetCursorPos(&mut onde);
            let hwnd = CreateWindowExW(estilo, w!("HccCursor"), w!(""), WS_POPUP, onde.x, onde.y, LADO, LADO, None, None, Some(instancia.into()), None).ok()?;
            SetLayeredWindowAttributes(hwnd, FUNDO, 255, LWA_COLORKEY).ok()?;
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let mut c = Cursor { hwnd, onde };
            c.bombear();
            Some(c)
        }
    }

    /// Glides to `(x, y)` in about 300 ms; the arrow's tip ends on the point.
    pub(crate) fn mover(&mut self, x: i32, y: i32) {
        const PASSOS: i32 = 15;
        let (x0, y0) = (self.onde.x, self.onde.y);
        for i in 1..=PASSOS {
            let (px, py) = (x0 + (x - x0) * i / PASSOS, y0 + (y - y0) * i / PASSOS);
            // SAFETY: moving our own window; it never takes the focus.
            let _ = unsafe { SetWindowPos(self.hwnd, Some(HWND_TOPMOST), px, py, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE) };
            self.bombear();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        self.onde = POINT { x, y };
    }

    /// The window lives on the agent's thread: its paint messages are handled here.
    fn bombear(&mut self) {
        let mut msg = MSG::default();
        // SAFETY: plain message pumping for windows of this thread.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        // SAFETY: destroying our own window.
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.hwnd) };
    }
}
