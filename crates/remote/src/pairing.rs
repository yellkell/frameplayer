//! Pairing: the URL a phone opens to get the web remote with its token, rendered as a QR code.

use crate::RemoteError;
use qrcode::render::svg;
use qrcode::{Color, EcLevel, QrCode};
use std::net::IpAddr;

/// `http://<lan_ip>:<port>/?token=<token>` (IPv6 addresses are bracketed). The web page stores
/// the token and strips it from the address bar.
pub fn pairing_url(lan_ip: IpAddr, port: u16, token: &str) -> String {
    let host = match lan_ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    let enc: String = token
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("http://{host}:{port}/?token={enc}")
}

fn encode(data: &str) -> Result<QrCode, RemoteError> {
    QrCode::with_error_correction_level(data.as_bytes(), EcLevel::M)
        .map_err(|e| RemoteError::Qr(e.to_string()))
}

/// QR code as a standalone SVG document (black on white, quiet zone included).
pub fn qr_svg(data: &str) -> Result<String, RemoteError> {
    let code = encode(data)?;
    Ok(code
        .render::<svg::Color<'_>>()
        .min_dimensions(256, 256)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .quiet_zone(true)
        .build())
}

/// QR code as an 8-bit greyscale PNG with `scale` pixels per module and a 4-module quiet zone.
pub fn qr_png(data: &str, scale: u32) -> Result<Vec<u8>, RemoteError> {
    let code = encode(data)?;
    let modules = code.width() as u32;
    let colors = code.to_colors();
    let scale = scale.clamp(1, 64);
    let quiet = 4;
    let size = (modules + 2 * quiet) * scale;
    let mut pixels = vec![255u8; (size * size) as usize];
    for (i, c) in colors.iter().enumerate() {
        if *c != Color::Dark {
            continue;
        }
        let (mx, my) = (i as u32 % modules, i as u32 / modules);
        for y in 0..scale {
            let row = ((my + quiet) * scale + y) * size;
            let x0 = (mx + quiet) * scale;
            pixels[(row + x0) as usize..(row + x0 + scale) as usize].fill(0);
        }
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, size, size);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc
            .write_header()
            .map_err(|e| RemoteError::Qr(e.to_string()))?;
        w.write_image_data(&pixels)
            .map_err(|e| RemoteError::Qr(e.to_string()))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(
            pairing_url("192.168.1.5".parse().unwrap(), 23560, "abcXYZ09"),
            "http://192.168.1.5:23560/?token=abcXYZ09"
        );
        assert_eq!(
            pairing_url("fe80::1".parse().unwrap(), 80, "a b+"),
            "http://[fe80::1]:80/?token=a%20b%2B"
        );
    }

    #[test]
    fn svg_output() {
        let s = qr_svg("http://192.168.1.5:23560/?token=abc").unwrap();
        assert!(s.contains("<svg") && s.contains("</svg>") && s.contains("#000000"));
    }

    #[test]
    fn png_output() {
        let bytes = qr_png("http://192.168.1.5:23560/?token=abc", 4).unwrap();
        let dec = png::Decoder::new(std::io::Cursor::new(&bytes));
        let mut r = dec.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size()];
        let info = r.next_frame(&mut buf).unwrap();
        let modules = encode("http://192.168.1.5:23560/?token=abc")
            .unwrap()
            .width() as u32;
        assert_eq!(info.width, (modules + 8) * 4);
        assert_eq!(info.width, info.height);
        let px = |x: u32, y: u32| buf[(y * info.width + x) as usize];
        assert_eq!(px(0, 0), 255, "quiet zone is white");
        assert_eq!(px(16, 16), 0, "top-left finder pattern corner is dark");
        assert_eq!(px(16 + 4, 16 + 4), 255, "finder pattern ring gap is light");
    }
}
