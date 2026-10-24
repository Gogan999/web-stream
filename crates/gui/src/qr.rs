//! QR codes drawn straight into the window: the pairing link for a phone's
//! camera.

use eframe::egui::{self, Color32, Rect, Sense, vec2};
use qrcode::{EcLevel, QrCode};

/// Modules of white border round the code, as the standard asks.
const QUIET: usize = 4;

/// Draws `text` as a QR code `size` points wide, black on white whatever
/// the theme (phone cameras need the contrast).
pub fn show(ui: &mut egui::Ui, text: &str, size: f32) -> egui::Response {
    let Ok(code) = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M) else {
        return ui.weak("(too long for a QR code)");
    };
    let n = code.width();
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::WHITE);
    let module = size / (n + 2 * QUIET) as f32;
    for y in 0..n {
        for x in 0..n {
            if code[(x, y)] == qrcode::Color::Dark {
                let min = rect.min + vec2((x + QUIET) as f32, (y + QUIET) as f32) * module;
                // A hair wider, so no seams show between modules.
                painter.rect_filled(
                    Rect::from_min_size(min, vec2(module + 0.3, module + 0.3)),
                    0.0,
                    Color32::BLACK,
                );
            }
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pairing_link_fits_a_small_code() {
        let url = format!("https://192.168.100.200:8443/#pair={}", "A".repeat(43));
        let code = QrCode::with_error_correction_level(url.as_bytes(), EcLevel::M).unwrap();
        // Version 6 or below: big modules, easy for a phone to read off a screen.
        assert!(code.width() <= 41, "{}", code.width());
    }
}
