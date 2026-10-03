//! ASS / SSA parsing: `[Script Info]` (PlayRes), `[V4+ Styles]` /
//! `[V4 Styles]`, `[Events]` dialogue lines, and Matroska ASS packets.
//!
//! Override tags mapped onto [`Span`] styling: `\b`, `\i`, `\u`, `\c` /
//! `\1c`, `\an` / `\a`, `\pos`, `\r`; `\N`, `\n`, `\h` are handled; all other
//! tags (karaoke, transforms, drawing, …) are stripped. Full-fidelity
//! rendering would need libass (optional, not built in).

use super::{Cue, Span, SubtitleTrack, TextCue};
use crate::error::{Result, VideoError};

#[derive(Debug, Clone, PartialEq)]
pub struct AssStyle {
    pub name: String,
    pub fontname: String,
    pub fontsize: f32,
    pub primary: [u8; 4],
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// Numpad alignment.
    pub alignment: u8,
    pub margin_v: i32,
}

impl Default for AssStyle {
    fn default() -> Self {
        AssStyle {
            name: "Default".into(),
            fontname: "Arial".into(),
            fontsize: 20.0,
            primary: [255, 255, 255, 255],
            bold: false,
            italic: false,
            underline: false,
            alignment: 2,
            margin_v: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AssScript {
    pub play_res: (u32, u32),
    pub styles: Vec<AssStyle>,
    pub track: SubtitleTrack,
}

impl AssScript {
    pub fn style(&self, name: &str) -> Option<&AssStyle> {
        let n = name.trim_start_matches('*');
        self.styles
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(n))
            .or_else(|| self.styles.first())
    }
}

/// `&HAABBGGRR&` / `&HBBGGRR` / decimal → RGBA.
pub fn parse_ass_color(s: &str) -> Option<[u8; 4]> {
    let t = s
        .trim()
        .trim_start_matches('&')
        .trim_start_matches(['H', 'h'])
        .trim_end_matches('&');
    let v = if s.trim().starts_with('&') || s.trim().starts_with(['H', 'h']) {
        u32::from_str_radix(t, 16).ok()?
    } else {
        t.parse::<i64>().ok()? as u32
    };
    let a = 255 - ((v >> 24) & 0xff) as u8;
    Some([
        (v & 0xff) as u8,
        ((v >> 8) & 0xff) as u8,
        ((v >> 16) & 0xff) as u8,
        a,
    ])
}

fn ass_bool(s: &str) -> bool {
    s.trim().parse::<i32>().map(|v| v != 0).unwrap_or(false)
}

/// SSA v4 legacy alignment → numpad.
fn legacy_alignment(a: u8) -> u8 {
    match a {
        1..=3 => a,
        5..=7 => a + 2,
        9..=11 => a - 5,
        _ => 2,
    }
}

/// Parse a dialogue's text with override tags.
pub fn parse_text(text: &str, style: Option<&AssStyle>, play_res: (u32, u32)) -> TextCue {
    let base = style.cloned().unwrap_or_default();
    let mut cur = Span {
        text: String::new(),
        bold: base.bold,
        italic: base.italic,
        underline: base.underline,
        color: Some(base.primary),
    };
    let mut spans: Vec<Span> = Vec::new();
    let mut alignment = base.alignment;
    let mut position = None;
    let push = |spans: &mut Vec<Span>, cur: &mut Span| {
        if cur.text.is_empty() {
            return;
        }
        if let Some(last) = spans.last_mut() {
            if last.bold == cur.bold
                && last.italic == cur.italic
                && last.underline == cur.underline
                && last.color == cur.color
            {
                last.text.push_str(&cur.text);
                cur.text.clear();
                return;
            }
        }
        spans.push(cur.clone());
        cur.text.clear();
    };
    let mut rest = text;
    while !rest.is_empty() {
        if rest.starts_with('{') {
            let end = rest.find('}').unwrap_or(rest.len() - 1);
            let block = &rest[1..end];
            push(&mut spans, &mut cur);
            for tag in block.split('\\').skip(1) {
                let tag = tag.trim();
                let num = |p: &str| {
                    tag.strip_prefix(p)
                        .and_then(|v| v.trim().parse::<i32>().ok())
                };
                if tag.starts_with("pos(") {
                    let inner = tag.trim_start_matches("pos(").trim_end_matches(')');
                    let v: Vec<f32> = inner
                        .split(',')
                        .filter_map(|x| x.trim().parse().ok())
                        .collect();
                    if v.len() == 2 && play_res.0 > 0 && play_res.1 > 0 {
                        position = Some((v[0] / play_res.0 as f32, v[1] / play_res.1 as f32));
                    }
                } else if let Some(n) = num("an") {
                    alignment = (n as u8).clamp(1, 9);
                } else if tag.starts_with("1c")
                    || (tag.starts_with('c') && tag[1..].starts_with('&'))
                {
                    let v = tag.trim_start_matches("1c").trim_start_matches('c');
                    if let Some(mut c) = parse_ass_color(v) {
                        c[3] = cur.color.map_or(255, |o| o[3]);
                        cur.color = Some(c);
                    }
                } else if tag == "r" || (tag.starts_with('r') && !tag.starts_with("rnd")) {
                    cur.bold = base.bold;
                    cur.italic = base.italic;
                    cur.underline = base.underline;
                    cur.color = Some(base.primary);
                } else if let Some(n) = num("b") {
                    cur.bold = n == 1 || n >= 600;
                } else if let Some(n) = num("i") {
                    cur.italic = n != 0;
                } else if let Some(n) = num("u") {
                    cur.underline = n != 0;
                } else if let Some(n) = num("a") {
                    alignment = legacy_alignment(n as u8);
                }
            }
            rest = &rest[(end + 1).min(rest.len())..];
            continue;
        }
        if let Some(r) = rest
            .strip_prefix("\\N")
            .or_else(|| rest.strip_prefix("\\n"))
        {
            cur.text.push('\n');
            rest = r;
            continue;
        }
        if let Some(r) = rest.strip_prefix("\\h") {
            cur.text.push('\u{a0}');
            rest = r;
            continue;
        }
        let ch = rest.chars().next().unwrap();
        cur.text.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    push(&mut spans, &mut cur);
    TextCue {
        spans,
        alignment,
        position,
        style: Some(base.name),
        layer: 0,
    }
}

fn parse_style(format: &[String], values: &str, legacy: bool) -> AssStyle {
    let vals: Vec<&str> = values.splitn(format.len().max(1), ',').collect();
    let mut s = AssStyle::default();
    for (k, v) in format.iter().zip(vals) {
        let v = v.trim();
        match k.as_str() {
            "name" => s.name = v.to_string(),
            "fontname" => s.fontname = v.to_string(),
            "fontsize" => s.fontsize = v.parse().unwrap_or(s.fontsize),
            "primarycolour" | "primarycolor" => s.primary = parse_ass_color(v).unwrap_or(s.primary),
            "bold" => s.bold = ass_bool(v),
            "italic" => s.italic = ass_bool(v),
            "underline" => s.underline = ass_bool(v),
            "alignment" => {
                let a = v.parse::<u8>().unwrap_or(2);
                s.alignment = if legacy {
                    legacy_alignment(a)
                } else {
                    a.clamp(1, 9)
                };
            }
            "marginv" => s.margin_v = v.parse().unwrap_or(s.margin_v),
            _ => {}
        }
    }
    s
}

/// Parse a full `.ass` / `.ssa` script (also used for Matroska CodecPrivate headers).
pub fn parse(text: &str) -> Result<AssScript> {
    let text = text.trim_start_matches('\u{feff}');
    let mut section = String::new();
    let mut script = AssScript {
        play_res: (0, 0),
        ..Default::default()
    };
    let mut style_fmt: Vec<String> = Vec::new();
    let mut event_fmt: Vec<String> = Vec::new();
    let mut legacy = false;
    let mut cues = Vec::new();
    let mut saw_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_ascii_lowercase();
            saw_section = true;
            legacy = section == "v4 styles";
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim_start();
        match (section.as_str(), key.as_str()) {
            ("script info", "playresx") => script.play_res.0 = value.trim().parse().unwrap_or(0),
            ("script info", "playresy") => script.play_res.1 = value.trim().parse().unwrap_or(0),
            ("v4+ styles" | "v4 styles", "format") => {
                style_fmt = value
                    .split(',')
                    .map(|s| s.trim().to_ascii_lowercase())
                    .collect()
            }
            ("v4+ styles" | "v4 styles", "style") => {
                script.styles.push(parse_style(&style_fmt, value, legacy))
            }
            ("events", "format") => {
                event_fmt = value
                    .split(',')
                    .map(|s| s.trim().to_ascii_lowercase())
                    .collect()
            }
            ("events", "dialogue") => {
                if event_fmt.is_empty() {
                    event_fmt = [
                        "layer", "start", "end", "style", "name", "marginl", "marginr", "marginv",
                        "effect", "text",
                    ]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                }
                let vals: Vec<&str> = value.splitn(event_fmt.len(), ',').collect();
                let get = |k: &str| {
                    event_fmt
                        .iter()
                        .position(|f| f == k)
                        .and_then(|i| vals.get(i))
                        .copied()
                        .unwrap_or("")
                };
                let (Some(s), Some(e)) = (
                    super::parse_timestamp(get("start")),
                    super::parse_timestamp(get("end")),
                ) else {
                    continue;
                };
                if e <= s {
                    continue;
                }
                let style = script.style(get("style").trim()).cloned();
                let mut tc = parse_text(get("text"), style.as_ref(), script.play_res);
                tc.layer = get("layer").trim().parse().unwrap_or(0);
                cues.push(Cue::text(s, e, tc));
            }
            _ => {}
        }
    }
    if !saw_section {
        return Err(VideoError::invalid("not an ASS/SSA script"));
    }
    if script.play_res == (0, 0) {
        script.play_res = (384, 288); // libass default
    }
    script.track = SubtitleTrack::new(cues);
    Ok(script)
}

/// Matroska ASS block: `ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text`.
pub fn parse_mkv_packet(header: Option<&AssScript>, data: &str) -> Option<TextCue> {
    let f: Vec<&str> = data.splitn(9, ',').collect();
    if f.len() < 9 {
        return None;
    }
    let style = header.and_then(|h| h.style(f[2].trim()));
    let play_res = header.map_or((384, 288), |h| h.play_res);
    let mut tc = parse_text(f[8], style, play_res);
    tc.layer = f[1].trim().parse().unwrap_or(0);
    Some(tc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitle::CueContent;
    use fp_core::MediaTime;

    const SCRIPT: &str = "[Script Info]\nScriptType: v4.00+\nPlayResX: 1920\nPlayResY: 1080\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,48,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,40,1\nStyle: Sign,Arial,40,&H0000FFFF,&H000000FF,&H00000000,&H00000000,-1,0,0,0,100,100,0,0,1,2,0,8,10,10,40,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:03.50,Default,,0,0,0,,Hello, {\\i1}world{\\i0}!\\NNext line\nDialogue: 1,0:00:02.00,0:00:04.00,Sign,,0,0,0,,{\\pos(960,540)\\c&H0000FF&\\k20}Centre {\\fad(100,100)}red\nComment: 0,0:00:02.00,0:00:04.00,Default,,0,0,0,,ignored\n";

    #[test]
    fn parses_script() {
        let s = parse(SCRIPT).unwrap();
        assert_eq!(s.play_res, (1920, 1080));
        assert_eq!(s.styles.len(), 2);
        assert_eq!(
            s.styles[1].primary,
            [255, 255, 0, 255],
            "&H0000FFFF is yellow"
        );
        assert!(s.styles[1].bold);
        assert_eq!(s.track.len(), 2);
        let c = &s.track.cues()[0];
        assert_eq!(
            (c.start, c.end),
            (MediaTime::from_millis(1000), MediaTime::from_millis(3500))
        );
        let CueContent::Text(tc) = &c.content else {
            panic!()
        };
        assert_eq!(tc.plain_text(), "Hello, world!\nNext line");
        assert!(!tc.spans[0].italic && tc.spans[1].italic && !tc.spans[2].italic);
        let CueContent::Text(tc) = &s.track.cues()[1].content else {
            panic!()
        };
        assert_eq!(tc.alignment, 8);
        assert_eq!(tc.position, Some((0.5, 0.5)));
        assert_eq!(tc.spans[0].color, Some([255, 0, 0, 255]));
        assert_eq!(tc.plain_text(), "Centre red", "unknown tags stripped");
        assert_eq!(tc.layer, 1);
    }

    #[test]
    fn mkv_packet() {
        let s = parse(SCRIPT).unwrap();
        let tc = parse_mkv_packet(Some(&s), "3,0,Sign,,0,0,0,,Sign, with comma").unwrap();
        assert_eq!(tc.plain_text(), "Sign, with comma");
        assert!(tc.spans[0].bold);
        assert!(parse_mkv_packet(None, "bad").is_none());
    }

    #[test]
    fn legacy_ssa_alignment_and_colors() {
        assert_eq!(legacy_alignment(6), 8);
        assert_eq!(legacy_alignment(10), 5);
        assert_eq!(parse_ass_color("&H80FF0000"), Some([0, 0, 255, 127]));
        assert_eq!(parse_ass_color("16777215"), Some([255, 255, 255, 255]));
    }
}
