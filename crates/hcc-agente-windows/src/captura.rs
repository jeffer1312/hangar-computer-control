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
