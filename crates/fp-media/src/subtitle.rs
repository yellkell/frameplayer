//! Subtitle cues: decoded from embedded streams or external files.

use crate::decode::{Decoder, HwDecode, Packet};
use crate::input::Input;
use crate::{Result, check};
use chardetng::EncodingDetector;
use encoding_rs::{Encoding, UTF_8};
use fp_core::ByteSource;
use fp_core::source::{MemorySource, is_subtitle_name};
use fp_ffmpeg_sys as ff;
use std::ffi::CStr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// A bitmap subtitle (PGS, DVD, DVB) in RGBA.
#[derive(Clone, Debug, PartialEq)]
pub struct SubBitmap {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// One timed subtitle.
#[derive(Clone, Debug, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    /// Plain text with line breaks; empty for bitmap cues.
    pub text: String,
    pub bitmaps: Vec<SubBitmap>,
}

/// Strips an ASS `Dialogue` payload to plain text: drops the leading fields
/// and `{...}` override blocks, turns `\N`/`\n` into newlines, `\h` into spaces.
pub fn ass_to_text(ass: &str) -> String {
    // FFmpeg's ASS packets: ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text
    let text = ass.splitn(9, ',').nth(8).unwrap_or(ass);
    let mut out = String::with_capacity(text.len());
    let mut depth = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            _ if depth > 0 => {}
            '\\' => match chars.peek() {
                Some('N') | Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('h') => {
                    chars.next();
                    out.push(' ');
                }
                _ => out.push('\\'),
            },
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Converts a decoded `AVSubtitle` to cues. `pkt_pts`/`pkt_duration` are in
/// seconds and used when the subtitle carries no end time.
pub(crate) fn convert(sub: &ff::AVSubtitle, pkt_pts: f64, pkt_duration: f64) -> Option<Cue> {
    let base = if sub.pts != ff::AV_NOPTS_VALUE {
        sub.pts as f64 / ff::AV_TIME_BASE as f64
    } else {
        pkt_pts
    };
    let start = base + sub.start_display_time as f64 / 1000.0;
    let end = if sub.end_display_time > 0 && sub.end_display_time != u32::MAX {
        base + sub.end_display_time as f64 / 1000.0
    } else if pkt_duration > 0.0 {
        pkt_pts + pkt_duration
    } else {
        start + 4.0
    };
    let mut text = Vec::new();
    let mut bitmaps = Vec::new();
    if sub.rects.is_null() {
        return None;
    }
    // SAFETY: rects holds num_rects valid pointers owned by `sub`.
    for &r in unsafe { std::slice::from_raw_parts(sub.rects, sub.num_rects as usize) } {
        let r = unsafe { &*r };
        match r.type_ {
            ff::SUBTITLE_ASS if !r.ass.is_null() => text.push(ass_to_text(
                &unsafe { CStr::from_ptr(r.ass) }.to_string_lossy(),
            )),
            ff::SUBTITLE_TEXT if !r.text.is_null() => text.push(
                unsafe { CStr::from_ptr(r.text) }
                    .to_string_lossy()
                    .trim()
                    .to_string(),
            ),
            ff::SUBTITLE_BITMAP
                if r.w > 0 && r.h > 0 && !r.data[0].is_null() && !r.data[1].is_null() =>
            {
                let (w, h) = (r.w as usize, r.h as usize);
                // SAFETY: data[0] is w*h palette indices with linesize[0];
                // data[1] is a 256-entry RGBA (native-endian u32) palette.
                let palette = unsafe { std::slice::from_raw_parts(r.data[1] as *const u32, 256) };
                let mut rgba = Vec::with_capacity(w * h * 4);
                for y in 0..h {
                    let row = unsafe {
                        std::slice::from_raw_parts(r.data[0].add(y * r.linesize[0] as usize), w)
                    };
                    for &i in row {
                        let c = palette[i as usize];
                        rgba.extend_from_slice(&[
                            (c >> 16) as u8,
                            (c >> 8) as u8,
                            c as u8,
                            (c >> 24) as u8,
                        ]);
                    }
                }
                bitmaps.push(SubBitmap {
                    x: r.x,
                    y: r.y,
                    width: w as u32,
                    height: h as u32,
                    rgba,
                });
            }
            _ => {}
        }
    }
    let text = text
        .into_iter()
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() && bitmaps.is_empty() {
        return None;
    }
    Some(Cue {
        start,
        end,
        text,
        bitmaps,
    })
}

/// Decodes one subtitle packet.
pub(crate) fn decode_packet(dec: &mut Decoder, pkt: &Packet) -> Option<Cue> {
    // SAFETY: decoder open for a subtitle stream; sub freed after use.
    unsafe {
        let mut sub: ff::AVSubtitle = std::mem::zeroed();
        let mut got = 0;
        let r = ff::avcodec_decode_subtitle2(dec.as_ptr(), &mut sub, &mut got, pkt.as_ptr());
        if r < 0 || got == 0 {
            return None;
        }
        let tb = crate::q2d(dec.time_base);
        let p = &*pkt.as_ptr();
        let pts = if p.pts != ff::AV_NOPTS_VALUE {
            p.pts as f64 * tb
        } else {
            0.0
        };
        let cue = convert(&sub, pts, p.duration as f64 * tb);
        ff::avsubtitle_free(&mut sub);
        cue
    }
}

/// Largest subtitle file that is read whole to detect its encoding.
const MAX_TEXT_SUBTITLE: u64 = 64 << 20;

fn read_all(src: &dyn ByteSource) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = src.read_at(out.len() as u64, &mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// Re-encodes subtitle text as UTF-8. FFmpeg drops cues in any other encoding,
/// and the bundled build has no iconv for its `sub_charenc` option. Without a
/// byte order mark the encoding is guessed from the text.
fn to_utf8(bytes: Vec<u8>) -> Vec<u8> {
    let encoding = match Encoding::for_bom(&bytes) {
        Some((encoding, _)) if encoding == UTF_8 => return bytes,
        Some((encoding, _)) => encoding,
        None if std::str::from_utf8(&bytes).is_ok() => return bytes,
        None => {
            let mut detector = EncodingDetector::new();
            detector.feed(&bytes, true);
            detector.guess(None, false)
        }
    };
    log::info!("subtitle encoding: {}", encoding.name());
    encoding.decode(&bytes).0.into_owned().into_bytes()
}

/// Loads every cue from a subtitle file (SRT, ASS, VTT) or from the first
/// subtitle stream of any container.
pub fn load_file(src: Arc<dyn ByteSource>, name: &str) -> Result<Vec<Cue>> {
    let src: Arc<dyn ByteSource> =
        if is_subtitle_name(name) && src.size().is_some_and(|s| s <= MAX_TEXT_SUBTITLE) {
            match read_all(src.as_ref()) {
                Ok(bytes) => Arc::new(MemorySource(to_utf8(bytes))),
                Err(e) => {
                    log::warn!("subtitle {name}: {e}");
                    src
                }
            }
        } else {
            src
        };
    let mut input = Input::open(src, name, Arc::new(AtomicBool::new(false)))?;
    let idx = input
        .best_stream(ff::AVMEDIA_TYPE_SUBTITLE)
        .ok_or(crate::Error::NoStream("subtitle"))?;
    let mut dec = Decoder::open(input.streams()[idx], HwDecode::Off)?;
    let mut cues = Vec::new();
    let pkt = Packet::new();
    while input.read(pkt.as_ptr())? {
        if pkt.stream_index() == idx
            && let Some(c) = decode_packet(&mut dec, &pkt)
        {
            cues.push(c);
        }
        // SAFETY: valid packet.
        unsafe { ff::av_packet_unref(pkt.as_ptr()) };
    }
    check(0, "")?;
    cues.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(cues)
}

/// Cues visible at time `t`.
pub fn active(cues: &[Cue], t: f64) -> Vec<&Cue> {
    cues.iter().filter(|c| c.start <= t && t < c.end).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::source::FileSource;

    #[test]
    fn strips_ass() {
        assert_eq!(
            ass_to_text("0,0,Default,,0,0,0,,Hello {\\i1}world{\\i0}"),
            "Hello world"
        );
        assert_eq!(
            ass_to_text("1,0,Default,,0,0,0,,Line one\\NLine, two\\hok"),
            "Line one\nLine, two ok"
        );
        assert_eq!(ass_to_text("plain"), "plain");
    }

    #[test]
    fn loads_srt_file() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/subs.srt");
        let cues = load_file(Arc::new(FileSource::open(&path).unwrap()), "subs.srt").unwrap();
        assert_eq!(cues.len(), 2);
        assert!(
            (cues[0].start - 0.2).abs() < 1e-6 && (cues[0].end - 1.0).abs() < 1e-6,
            "{:?}",
            cues[0]
        );
        assert_eq!(cues[0].text, "Hello world");
        assert_eq!(cues[1].text, "Second line\nwith break");
        assert_eq!(active(&cues, 0.5).len(), 1);
        assert_eq!(active(&cues, 1.1).len(), 0);
    }

    const SRT_TC: &str = "1\n00:00:00,200 --> 00:00:01,000\n你好,世界\n\n\
                          2\n00:00:01,200 --> 00:00:02,000\n這是一段繁體中文字幕\n";
    const SRT_SC: &str = "1\n00:00:00,200 --> 00:00:01,000\n你好,世界\n\n\
                          2\n00:00:01,200 --> 00:00:02,000\n这是一段简体中文字幕\n";

    fn cue_texts(bytes: Vec<u8>, name: &str) -> Vec<String> {
        load_file(Arc::new(MemorySource(bytes)), name)
            .unwrap()
            .into_iter()
            .map(|c| c.text)
            .collect()
    }

    #[test]
    fn loads_utf8_srt() {
        assert_eq!(
            cue_texts(SRT_TC.as_bytes().to_vec(), "movie.srt"),
            ["你好,世界", "這是一段繁體中文字幕"]
        );
    }

    #[test]
    fn loads_utf8_srt_with_bom() {
        let bytes = [b"\xEF\xBB\xBF".as_slice(), SRT_TC.as_bytes()].concat();
        assert_eq!(
            cue_texts(bytes, "movie.srt"),
            ["你好,世界", "這是一段繁體中文字幕"]
        );
    }

    #[test]
    fn loads_big5_srt() {
        let bytes = encoding_rs::BIG5.encode(SRT_TC).0.into_owned();
        assert_eq!(
            cue_texts(bytes, "movie.srt"),
            ["你好,世界", "這是一段繁體中文字幕"]
        );
    }

    #[test]
    fn loads_gbk_srt() {
        let bytes = encoding_rs::GBK.encode(SRT_SC).0.into_owned();
        assert_eq!(
            cue_texts(bytes, "movie.srt"),
            ["你好,世界", "这是一段简体中文字幕"]
        );
    }

    #[test]
    fn loads_utf16le_srt_with_bom() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(SRT_TC.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(
            cue_texts(bytes, "movie.srt"),
            ["你好,世界", "這是一段繁體中文字幕"]
        );
    }

    #[test]
    fn loads_big5_ass() {
        let ass = "[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\n\
                   Format: Name, Fontname, Fontsize\nStyle: Default,Arial,20\n\n\
                   [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                   Dialogue: 0,0:00:00.20,0:00:01.00,Default,,0,0,0,,這是一段繁體中文字幕\n";
        let bytes = encoding_rs::BIG5.encode(ass).0.into_owned();
        assert_eq!(cue_texts(bytes, "movie.ass"), ["這是一段繁體中文字幕"]);
    }

    #[test]
    fn loads_embedded_mkv_subtitles() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hevc10_tb.mkv");
        let cues = load_file(Arc::new(FileSource::open(&path).unwrap()), "hevc10_tb.mkv").unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[1].text, "Second line\nwith break");
        assert!((cues[1].end - 1.9).abs() < 1e-3, "{}", cues[1].end);
    }
}
