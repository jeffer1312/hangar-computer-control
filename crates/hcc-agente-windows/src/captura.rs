//! Screen capture of the primary monitor (windows_uia.py:362-370).

use crate::arvore::com;
use crate::regras;
use hcc_protocolo::DesktopError;
use uiautomation::screenshots::Screenshot;
use uiautomation::types::Rect;

pub(crate) fn png_tela(w: i32, h: i32) -> Result<Vec<u8>, DesktopError> {
    // capture_desktop() is the whole virtual screen; the protocol's screen is the primary one.
    let tela = Screenshot::capture_rect(Rect::new(0, 0, w, h)).map_err(com)?.to_rgba();
    regras::png_reduzido(tela.pixels().to_vec(), tela.width(), tela.height())
}

/// One window, even behind others: PrintWindow asks the window to draw itself (PW_RENDERFULLCONTENT also gets
/// Chromium and DirectComposition content).
pub(crate) fn png_janela(hwnd: isize) -> Result<Vec<u8>, DesktopError> {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits,
        ReleaseDC, SelectObject,
    };
    use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let h = HWND(hwnd as *mut _);
    let mut r = RECT::default();
    // SAFETY: `r` outlives the call.
    unsafe { GetWindowRect(h, &mut r) }.map_err(|e| regras::runtime_err(format!("janela sem retângulo: {e}")))?;
    let (w, alt) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || alt <= 0 {
        return Err(regras::runtime_err("janela sem tamanho para o print"));
    }
    let mut bgra = vec![0u8; (w as usize) * (alt as usize) * 4];
    // SAFETY: every GDI object created here is selected out and deleted before leaving; the buffer has the
    // size GetDIBits writes for a 32-bit top-down bitmap of w x alt.
    let copiado = unsafe {
        let tela = GetDC(None);
        let dc = CreateCompatibleDC(Some(tela));
        let bmp = CreateCompatibleBitmap(tela, w, alt);
        let antigo = SelectObject(dc, bmp.into());
        let desenhou = PrintWindow(h, dc, PRINT_WINDOW_FLAGS(2)).as_bool();
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -alt,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let linhas = GetDIBits(dc, bmp, 0, alt as u32, Some(bgra.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        SelectObject(dc, antigo);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, tela);
        desenhou && linhas == alt
    };
    if !copiado {
        return Err(regras::runtime_err("a janela não se desenhou para o print"));
    }
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    regras::png_reduzido(bgra, w as u32, alt as u32)
}
