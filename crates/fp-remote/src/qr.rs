//! QR codes for pairing a phone with the web remote.

use crate::RemoteError;
use qrcode::{Color, EcLevel, QrCode};

/// A QR code as a square grid of modules, ready for the VR UI to draw.
///
/// `modules[y * size + x]` is `true` for a dark module. The grid does not
/// include the quiet zone; leave at least 4 light modules around it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QrMatrix {
    /// Modules per side.
    pub size: usize,
    /// Row-major, `size * size` entries, `true` = dark.
    pub modules: Vec<bool>,
}

impl QrMatrix {
    /// Whether the module at column `x`, row `y` is dark. Out of range is light.
    pub fn is_dark(&self, x: usize, y: usize) -> bool {
        x < self.size && y < self.size && self.modules[y * self.size + x]
    }
}

/// Encodes `url` as a QR code with medium error correction, which scans
/// reliably off a headset display and fits pairing URLs comfortably.
pub fn pairing_qr(url: &str) -> Result<QrMatrix, RemoteError> {
    let code = QrCode::with_error_correction_level(url.as_bytes(), EcLevel::M)
        .map_err(|e| RemoteError::Qr(e.to_string()))?;
    let size = code.width();
    let modules = code
        .to_colors()
        .into_iter()
        .map(|c| c == Color::Dark)
        .collect();
    Ok(QrMatrix { size, modules })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_url_encodes() {
        let url = format!("http://192.168.1.50:8790/?token={}", "ab".repeat(32));
        let m = pairing_qr(&url).unwrap();
        assert_eq!(m.modules.len(), m.size * m.size);
        // Versions are 21 + 4k modules wide.
        assert!(
            m.size >= 21 && (m.size - 21).is_multiple_of(4),
            "{}",
            m.size
        );
        // Finder patterns: dark corners top-left, top-right, bottom-left,
        // light separator just inside, light bottom-right corner region edge.
        let s = m.size;
        for (x, y) in [(0, 0), (s - 1, 0), (0, s - 1), (6, 6), (s - 7, 6)] {
            assert!(m.is_dark(x, y), "({x},{y})");
        }
        assert!(!m.is_dark(7, 7));
        assert!(!m.is_dark(1, 1));
        assert!(!m.is_dark(s, 0));
        // Deterministic.
        assert_eq!(pairing_qr(&url).unwrap(), m);
    }

    #[test]
    fn too_long_is_an_error() {
        assert!(matches!(
            pairing_qr(&"x".repeat(5000)),
            Err(RemoteError::Qr(_))
        ));
    }
}
