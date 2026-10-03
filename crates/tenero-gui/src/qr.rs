//! A QR code for the receive screen: the dark and light squares of an address, for the window to draw.
//! (The `qrcode` crate makes the code; this is only its output as a grid.)

use qrcode::{Color, QrCode};

/// The side length and the squares (row by row, `true` = dark) of the QR code of `text`; `None` if it does not fit.
pub fn modules(text: &str) -> Option<(usize, Vec<bool>)> {
    let code = QrCode::new(text.as_bytes()).ok()?;
    let width = code.width();
    let squares: Vec<bool> = code
        .to_colors()
        .into_iter()
        .map(|c| c == Color::Dark)
        .collect();
    (squares.len() == width * width).then_some((width, squares))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_is_square_and_has_the_three_corner_markers() {
        let (w, q) = modules("hello").unwrap();
        assert_eq!(q.len(), w * w);
        assert!(
            w >= 21 && (w - 17) % 4 == 0,
            "a QR side is 17 + 4 * version: {w}"
        );
        let at = |x: usize, y: usize| q[y * w + x];
        // each finder marker is a dark 7x7 ring with a dark 3x3 centre, at three corners (not the bottom right)
        for (ox, oy) in [(0, 0), (w - 7, 0), (0, w - 7)] {
            for i in 0..7 {
                assert!(
                    at(ox + i, oy) && at(ox + i, oy + 6) && at(ox, oy + i) && at(ox + 6, oy + i)
                );
            }
            assert!(!at(ox + 1, oy + 1) && at(ox + 3, oy + 3));
        }
    }

    #[test]
    fn an_address_fits_and_text_too_long_does_not() {
        let addr = format!("tni1{}", "ab".repeat(68));
        let (w, _) = modules(&addr).unwrap();
        assert!(w <= 100, "{w}");
        assert!(modules(&"x".repeat(5000)).is_none());
    }
}
