//! QR code widget: encodes a string with the `qrcode` crate and draws the
//! modules as quads (horizontal runs merged) on a white quiet zone.

use crate::geom::{Rect, Vec2};
use crate::theme::Color;
use crate::ui::Ui;

/// Quiet-zone width in modules required by the QR spec.
pub const QUIET_ZONE: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrMatrix {
    pub width: usize,
    /// Row-major, `true` = dark module.
    pub modules: Vec<bool>,
}

impl QrMatrix {
    pub fn encode(data: &str) -> Option<QrMatrix> {
        let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
        Some(QrMatrix {
            width: code.width(),
            modules: code
                .to_colors()
                .into_iter()
                .map(|c| c == qrcode::Color::Dark)
                .collect(),
        })
    }

    pub fn dark(&self, x: usize, y: usize) -> bool {
        self.modules[y * self.width + x]
    }

    /// Dark runs per row as `(row, first_col, len)`.
    pub fn runs(&self) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        for y in 0..self.width {
            let mut x = 0;
            while x < self.width {
                if self.dark(x, y) {
                    let start = x;
                    while x < self.width && self.dark(x, y) {
                        x += 1;
                    }
                    out.push((y, start, x - start));
                } else {
                    x += 1;
                }
            }
        }
        out
    }
}

/// Cache size limit; QR payloads rarely change (pairing URL).
const QR_CACHE_MAX: usize = 8;

impl Ui {
    /// Square QR code `size` px wide (including quiet zone). Returns its rect,
    /// or draws an error box if `data` is too long to encode.
    pub fn qr_code(&mut self, data: &str, size: f32) -> Rect {
        let rect = self.allocate(Vec2::splat(size));
        self.draw_qr(rect, data);
        rect
    }

    pub fn draw_qr(&mut self, rect: Rect, data: &str) {
        if !self.qr_cache.contains_key(data) {
            if self.qr_cache.len() >= QR_CACHE_MAX {
                self.qr_cache.clear();
            }
            self.qr_cache
                .insert(data.to_string(), QrMatrix::encode(data));
        }
        let Some(m) = self.qr_cache.get(data).cloned().flatten() else {
            self.painter
                .rect_rounded(rect, self.theme.corner_radius, self.theme.danger.alpha(0.4));
            self.draw_text_in(
                rect,
                "QR too long",
                self.theme.small_text_size,
                self.theme.text,
                crate::text::Align::Center,
            );
            return;
        };
        let side = rect.w.min(rect.h);
        let total = (m.width + QUIET_ZONE * 2) as f32;
        // Whole-pixel modules keep edges crisp on the layer texture.
        let module = (side / total).floor().max(1.0);
        let drawn = module * total;
        let origin = Vec2::new(
            rect.x + ((side - drawn) * 0.5).floor(),
            rect.y + ((side - drawn) * 0.5).floor(),
        );
        self.painter
            .rect_filled(Rect::new(origin.x, origin.y, drawn, drawn), Color::WHITE);
        let q = QUIET_ZONE as f32 * module;
        for (row, col, len) in m.runs() {
            let r = Rect::new(
                origin.x + q + col as f32 * module,
                origin.y + q + row as f32 * module,
                len as f32 * module,
                module,
            );
            self.painter.rect_filled(r, Color::BLACK);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_runs_cover_dark_modules() {
        let m = QrMatrix::encode("http://192.168.1.20:8642/pair?token=abcdef").unwrap();
        assert!(m.width >= 21 && (m.width - 17).is_multiple_of(4));
        // Finder pattern: top-left 7x7 has a dark border.
        assert!(m.dark(0, 0) && m.dark(6, 0) && m.dark(0, 6) && !m.dark(1, 1));
        let dark = m.modules.iter().filter(|&&d| d).count();
        let covered: usize = m.runs().iter().map(|r| r.2).sum();
        assert_eq!(dark, covered);
        assert!(QrMatrix::encode(&"x".repeat(5000)).is_none());
    }
}
