//! Subtitles: SRT, WebVTT, ASS/SSA and PGS parsing, a time-indexed
//! [`SubtitleTrack`], decoding of embedded subtitle packets, and stereo
//! placement (per-eye disparity for a chosen depth, HereSphere-style).
//!
//! Text cues are reduced to styled spans (bold / italic / underline /
//! colour) plus alignment and an optional position, which is what the VR
//! UI text renderer draws; unsupported ASS override tags are stripped.
//! PGS cues are decoded to RGBA bitmaps.

pub mod ass;
pub mod pgs;
pub mod srt;
pub mod stereo;
pub mod webvtt;

use crate::error::{Result, VideoError};
use crate::packet::{CodecId, Packet, TrackDesc};
use fp_core::MediaTime;

/// End time used for cues whose end is not known yet (PGS until cleared).
pub const OPEN_END: MediaTime = MediaTime(i64::MAX / 4);

/// One run of uniformly styled text. `\n` inside `text` is a line break.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// RGBA.
    pub color: Option<[u8; 4]>,
}

/// Numpad-style alignment (1 = bottom-left … 9 = top-right; 2 = bottom-centre).
pub type Alignment = u8;

#[derive(Debug, Clone, PartialEq)]
pub struct TextCue {
    pub spans: Vec<Span>,
    pub alignment: Alignment,
    /// Explicit anchor position, normalised 0..1 over the video frame.
    pub position: Option<(f32, f32)>,
    /// ASS style name, if any.
    pub style: Option<String>,
    pub layer: i32,
}

impl TextCue {
    pub fn plain(text: &str) -> Self {
        TextCue {
            spans: vec![Span {
                text: text.to_string(),
                ..Default::default()
            }],
            alignment: 2,
            position: None,
            style: None,
            layer: 0,
        }
    }

    /// Concatenated text without styling.
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

/// A bitmap subtitle (PGS) positioned on the video canvas.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleBitmap {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Straight-alpha RGBA, `width × height × 4`.
    pub rgba: Vec<u8>,
    /// Size of the canvas the coordinates refer to (usually the video size).
    pub canvas_width: u32,
    pub canvas_height: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CueContent {
    Text(TextCue),
    Bitmap(SubtitleBitmap),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub start: MediaTime,
    pub end: MediaTime,
    pub content: CueContent,
}

impl Cue {
    pub fn text(start: MediaTime, end: MediaTime, t: TextCue) -> Self {
        Cue {
            start,
            end,
            content: CueContent::Text(t),
        }
    }
    pub fn is_active(&self, t: MediaTime) -> bool {
        self.start <= t && t < self.end
    }
}

/// A sorted set of cues with fast lookup by time.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SubtitleTrack {
    cues: Vec<Cue>,
    max_duration: i64,
    /// User timing adjustment (positive = later).
    pub delay: MediaTime,
}

impl SubtitleTrack {
    pub fn new(mut cues: Vec<Cue>) -> Self {
        cues.sort_by_key(|c| c.start);
        let max_duration = cues
            .iter()
            .map(|c| (c.end - c.start).0.min(OPEN_END.0))
            .max()
            .unwrap_or(0);
        SubtitleTrack {
            cues,
            max_duration,
            delay: MediaTime::ZERO,
        }
    }

    pub fn cues(&self) -> &[Cue] {
        &self.cues
    }

    pub fn len(&self) -> usize {
        self.cues.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    /// Insert a cue keeping order; exact duplicates (same start and
    /// content, e.g. re-demuxed after a seek) are ignored.
    pub fn insert(&mut self, cue: Cue) {
        let pos = self.cues.partition_point(|c| c.start < cue.start);
        if self.cues[pos..]
            .iter()
            .take_while(|c| c.start == cue.start)
            .any(|c| c.content == cue.content)
        {
            return;
        }
        self.max_duration = self
            .max_duration
            .max((cue.end - cue.start).0.min(OPEN_END.0));
        self.cues.insert(pos, cue);
    }

    /// Close cues with an open end that started before `at` (PGS clear).
    pub fn close_open_cues(&mut self, at: MediaTime) {
        for c in self
            .cues
            .iter_mut()
            .filter(|c| c.end == OPEN_END && c.start < at)
        {
            c.end = at;
        }
        self.max_duration = self
            .cues
            .iter()
            .map(|c| (c.end - c.start).0.min(OPEN_END.0))
            .max()
            .unwrap_or(0);
    }

    /// Cues visible at media time `t` (after applying `delay`), in start order.
    pub fn active_at(&self, t: MediaTime) -> Vec<&Cue> {
        let t = t - self.delay;
        let hi = self.cues.partition_point(|c| c.start <= t);
        let lo_time = t.0.saturating_sub(self.max_duration);
        let lo = self.cues[..hi].partition_point(|c| c.start.0 < lo_time);
        self.cues[lo..hi]
            .iter()
            .filter(|c| c.is_active(t))
            .collect()
    }

    /// Parse a subtitle file, choosing the format from the extension and
    /// falling back to content sniffing.
    pub fn from_file_bytes(name: &str, data: &[u8]) -> Result<Self> {
        let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        if ext == "sup" || data.starts_with(b"PG") {
            return pgs::parse_sup(data);
        }
        let text = decode_text(data);
        let t = text.trim_start();
        if ext == "vtt" || t.starts_with("WEBVTT") {
            return webvtt::parse(&text);
        }
        if matches!(ext.as_str(), "ass" | "ssa") || t.starts_with("[Script Info]") {
            return ass::parse(&text).map(|a| a.track);
        }
        if ext == "srt" || t.contains("-->") {
            return srt::parse(&text);
        }
        Err(VideoError::Unsupported(format!(
            "subtitle format of {name}"
        )))
    }
}

/// Decode subtitle text: UTF-8 (with or without BOM), UTF-16 with BOM, else Latin-1.
pub fn decode_text(data: &[u8]) -> String {
    if let Some(rest) = data.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    let utf16 = |le: bool| {
        let units: Vec<u16> = data[2..]
            .chunks_exact(2)
            .map(|c| {
                if le {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if data.starts_with(&[0xff, 0xfe]) {
        return utf16(true);
    }
    if data.starts_with(&[0xfe, 0xff]) {
        return utf16(false);
    }
    match std::str::from_utf8(data) {
        Ok(s) => s.to_string(),
        Err(_) => data.iter().map(|&b| b as char).collect(),
    }
}

/// Parse `[hh:]mm:ss[.,]fff` (also `h:mm:ss.cc` for ASS) into a time.
pub fn parse_timestamp(s: &str) -> Option<MediaTime> {
    let s = s.trim();
    let (hms, frac) = match s.rfind(['.', ',']) {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    let nums: Option<Vec<i64>> = parts.iter().map(|p| p.trim().parse::<i64>().ok()).collect();
    let nums = nums?;
    let (h, m, sec) = match nums.as_slice() {
        [h, m, s] => (*h, *m, *s),
        [m, s] => (0, *m, *s),
        _ => return None,
    };
    let frac_us = if frac.is_empty() {
        0
    } else {
        let digits: String = frac
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .take(6)
            .collect();
        if digits.is_empty() {
            return None;
        }
        digits.parse::<i64>().ok()? * 10i64.pow(6 - digits.len() as u32)
    };
    Some(MediaTime((h * 3600 + m * 60 + sec) * 1_000_000 + frac_us))
}

/// Turns embedded subtitle packets of one track into cues.
pub struct EmbeddedSubtitleDecoder {
    codec: CodecId,
    ass_header: Option<ass::AssScript>,
    pgs: pgs::PgsDecoder,
}

/// What a packet changed in the track.
#[derive(Debug, Clone, PartialEq)]
pub enum SubtitleUpdate {
    Add(Cue),
    /// Close open-ended cues at this time (PGS "clear screen").
    CloseOpen(MediaTime),
}

impl EmbeddedSubtitleDecoder {
    pub fn new(track: &TrackDesc) -> Self {
        let ass_header = (track.codec == CodecId::Ass && !track.codec_private.is_empty())
            .then(|| ass::parse(&decode_text(&track.codec_private)).ok())
            .flatten();
        EmbeddedSubtitleDecoder {
            codec: track.codec.clone(),
            ass_header,
            pgs: pgs::PgsDecoder::default(),
        }
    }

    pub fn decode(&mut self, pkt: &Packet) -> Vec<SubtitleUpdate> {
        let end = if pkt.duration.0 > 0 {
            pkt.pts + pkt.duration
        } else {
            pkt.pts + MediaTime::from_secs_f64(3.0)
        };
        match &self.codec {
            CodecId::SubRip => {
                let text = decode_text(&pkt.data);
                vec![SubtitleUpdate::Add(Cue::text(
                    pkt.pts,
                    end,
                    srt::parse_cue_text(&text),
                ))]
            }
            CodecId::MovText => {
                // tx3g: u16 length + UTF-8 text (+ style boxes, ignored).
                if pkt.data.len() < 2 {
                    return vec![];
                }
                let len = u16::from_be_bytes([pkt.data[0], pkt.data[1]]) as usize;
                let text = String::from_utf8_lossy(&pkt.data[2..(2 + len).min(pkt.data.len())])
                    .into_owned();
                if text.is_empty() {
                    return vec![];
                }
                vec![SubtitleUpdate::Add(Cue::text(
                    pkt.pts,
                    end,
                    TextCue::plain(&text),
                ))]
            }
            CodecId::WebVtt => {
                // Matroska: cue text; MP4 `wvtt`: vttc boxes with a payl child.
                let mut texts = Vec::new();
                let boxes: Vec<_> = crate::bytes::iter_boxes(&pkt.data).collect();
                if boxes
                    .iter()
                    .any(|b| &b.kind == b"vttc" || &b.kind == b"vtte")
                {
                    for b in boxes.iter().filter(|b| &b.kind == b"vttc") {
                        if let Some(p) = crate::bytes::find_box(b.payload, b"payl") {
                            texts.push(String::from_utf8_lossy(p).into_owned());
                        }
                    }
                } else {
                    texts.push(decode_text(&pkt.data));
                }
                texts
                    .into_iter()
                    .map(|t| {
                        SubtitleUpdate::Add(Cue::text(pkt.pts, end, webvtt::parse_cue_text(&t, "")))
                    })
                    .collect()
            }
            CodecId::Ass => {
                let text = decode_text(&pkt.data);
                match ass::parse_mkv_packet(self.ass_header.as_ref(), &text) {
                    Some(c) => vec![SubtitleUpdate::Add(Cue::text(pkt.pts, end, c))],
                    None => vec![],
                }
            }
            CodecId::Pgs => self.pgs.decode_segments(&pkt.data, pkt.pts),
            _ => vec![],
        }
    }

    /// PlayRes / styles from the ASS header, if any.
    pub fn ass_script(&self) -> Option<&ass::AssScript> {
        self.ass_header.as_ref()
    }
}

impl SubtitleTrack {
    pub fn apply(&mut self, u: SubtitleUpdate) {
        match u {
            SubtitleUpdate::Add(c) => self.insert(c),
            SubtitleUpdate::CloseOpen(t) => self.close_open_cues(t),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(s: i64, e: i64, t: &str) -> Cue {
        Cue::text(
            MediaTime::from_millis(s),
            MediaTime::from_millis(e),
            TextCue::plain(t),
        )
    }

    #[test]
    fn active_lookup_with_overlaps() {
        let mut tr = SubtitleTrack::new(vec![
            cue(0, 10_000, "long"),
            cue(1000, 2000, "a"),
            cue(3000, 4000, "b"),
        ]);
        let at = |ms| {
            tr.active_at(MediaTime::from_millis(ms))
                .iter()
                .map(|c| match &c.content {
                    CueContent::Text(t) => t.plain_text(),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(at(1500), vec!["long", "a"]);
        assert_eq!(at(2500), vec!["long"]);
        assert_eq!(at(3999), vec!["long", "b"]);
        assert!(at(10_000).is_empty());
        tr.delay = MediaTime::from_millis(1000);
        assert_eq!(tr.active_at(MediaTime::from_millis(2500)).len(), 2);
        tr.insert(cue(1000, 2000, "a"));
        assert_eq!(tr.len(), 3, "duplicate ignored");
    }

    #[test]
    fn open_cues_close() {
        let mut tr = SubtitleTrack::default();
        tr.insert(Cue::text(
            MediaTime::from_millis(100),
            OPEN_END,
            TextCue::plain("x"),
        ));
        assert_eq!(tr.active_at(MediaTime::from_secs_f64(1000.0)).len(), 1);
        tr.close_open_cues(MediaTime::from_millis(500));
        assert!(tr.active_at(MediaTime::from_millis(600)).is_empty());
        assert_eq!(tr.active_at(MediaTime::from_millis(400)).len(), 1);
    }

    #[test]
    fn timestamps() {
        assert_eq!(
            parse_timestamp("01:02:03,456"),
            Some(MediaTime(3_723_456_000))
        );
        assert_eq!(parse_timestamp("02:03.5"), Some(MediaTime(123_500_000)));
        assert_eq!(parse_timestamp("0:00:01.25"), Some(MediaTime(1_250_000)));
        assert_eq!(parse_timestamp("nope"), None);
    }

    #[test]
    fn text_decoding() {
        assert_eq!(decode_text(b"\xef\xbb\xbfhi"), "hi");
        assert_eq!(decode_text(&[0xff, 0xfe, b'h', 0, b'i', 0]), "hi");
        assert_eq!(decode_text(&[b'c', 0xe9]), "c\u{e9}");
    }

    #[test]
    fn embedded_srt_and_tx3g() {
        let mut t = TrackDesc::new(3, crate::packet::TrackKind::Subtitle, CodecId::SubRip);
        let mut d = EmbeddedSubtitleDecoder::new(&t);
        let pkt = Packet {
            track: 3,
            pts: MediaTime::from_millis(500),
            dts: MediaTime::from_millis(500),
            duration: MediaTime::from_millis(1500),
            keyframe: true,
            data: b"<i>Hello</i>".to_vec(),
        };
        let u = d.decode(&pkt);
        let SubtitleUpdate::Add(c) = &u[0] else {
            panic!()
        };
        assert_eq!(c.end, MediaTime::from_millis(2000));
        let CueContent::Text(tc) = &c.content else {
            panic!()
        };
        assert!(tc.spans[0].italic);
        t.codec = CodecId::MovText;
        let mut d = EmbeddedSubtitleDecoder::new(&t);
        let pkt = Packet {
            data: vec![0, 2, b'o', b'k'],
            ..pkt
        };
        assert_eq!(d.decode(&pkt).len(), 1);
    }
}
