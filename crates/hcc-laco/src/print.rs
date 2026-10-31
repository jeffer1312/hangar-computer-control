//! The action's print: the cycle's screenshot with the target box and the click point drawn.

use hcc_protocolo::Rect;
use image::{ImageFormat, Rgba, RgbaImage};

const COR: Rgba<u8> = Rgba([230, 30, 30, 255]);

/// Screen point `(x, y)` lands on image pixel `((x - origem.0) * escala, (y - origem.1) * escala)`.
pub fn anotar(tela: &[u8], caixa: Option<Rect>, ponto: Option<(i32, i32)>, origem: (i32, i32), escala: f64) -> Result<Vec<u8>, String> {
    let mut img = image::load_from_memory(tela).map_err(|e| e.to_string())?.to_rgba8();
    let para = |x: i32, y: i32| (((f64::from(x - origem.0)) * escala).round() as i64, ((f64::from(y - origem.1)) * escala).round() as i64);
    if let Some(r) = caixa {
        let ((x0, y0), (x1, y1)) = (para(r.0, r.1), para(r.2, r.3));
        // Two pixels wide, so it shows on a phone too.
        for d in 0..2 {
            for x in x0..=x1 {
                pintar(&mut img, x, y0 + d);
                pintar(&mut img, x, y1 - d);
            }
            for y in y0..=y1 {
                pintar(&mut img, x0 + d, y);
                pintar(&mut img, x1 - d, y);
            }
        }
    }
    if let Some((x, y)) = ponto {
        let (cx, cy) = para(x, y);
        for dx in -3..=3_i64 {
            for dy in -3..=3_i64 {
                if dx * dx + dy * dy <= 9 {
                    pintar(&mut img, cx + dx, cy + dy);
                }
            }
        }
    }
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(png)
}

fn pintar(img: &mut RgbaImage, x: i64, y: i64) {
    if let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) && x < img.width() && y < img.height() {
        img.put_pixel(x, y, COR);
    }
}
