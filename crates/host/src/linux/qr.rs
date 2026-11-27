//! QR codes for the pairing link: as rows of modules for the settings page, and as text for a
//! terminal.

use qrcode::{EcLevel, QrCode};

/// Modules of light border round the code.
const QUIET: usize = 4;

/// The code's rows, `1` for a dark module, without the border; `None` if `text` is too long.
pub fn rows(text: &str) -> Option<Vec<String>> {
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M).ok()?;
    let n = code.width();
    Some(
        (0..n)
            .map(|y| {
                (0..n)
                    .map(|x| {
                        if code[(x, y)] == qrcode::Color::Dark {
                            '1'
                        } else {
                            '0'
                        }
                    })
                    .collect()
            })
            .collect(),
    )
}

/// The code drawn with half blocks, two rows to a line, light modules lit (for a dark terminal).
pub fn text(text: &str) -> Option<String> {
    let rows = rows(text)?;
    let n = rows.len() + 2 * QUIET;
    let dark = |x: usize, y: usize| {
        let (Some(x), Some(y)) = (x.checked_sub(QUIET), y.checked_sub(QUIET)) else {
            return false;
        };
        rows.get(y)
            .and_then(|r| r.as_bytes().get(x))
            .is_some_and(|b| *b == b'1')
    };
    let mut out = String::new();
    for y in (0..n).step_by(2) {
        for x in 0..n {
            out.push(match (dark(x, y), dark(x, y + 1) || y + 1 >= n) {
                (false, false) => '█',
                (false, true) => '▀',
                (true, false) => '▄',
                (true, true) => ' ',
            });
        }
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pairing_link_fits_a_small_code() {
        let url = format!("https://192.168.100.200:8443/#pair={}", "A".repeat(43));
        let code = rows(&url).unwrap();
        assert!(code.len() <= 41 && code.iter().all(|r| r.len() == code.len()));
        let drawn = text(&url).unwrap();
        assert_eq!(drawn.lines().count(), (code.len() + 2 * QUIET).div_ceil(2));
        assert!(rows(&"x".repeat(5000)).is_none());
    }
}
