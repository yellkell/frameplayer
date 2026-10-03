//! SubRip (`.srt`) parsing, plus the HTML-like inline markup shared with
//! WebVTT (`<b>`, `<i>`, `<u>`, `<font color>`, `<c.class>`, `<v>`) and the
//! `{\anN}` alignment tags common in SRT files.

use super::{Cue, Span, SubtitleTrack, TextCue};
use crate::error::Result;

/// `#rrggbb`, `#rgb` or a few CSS names → RGBA.
pub fn parse_color(s: &str) -> Option<[u8; 4]> {
    let s = s.trim().trim_matches('"').trim_matches('\'');
    if let Some(hex) = s.strip_prefix('#') {
        let v = u32::from_str_radix(hex, 16).ok()?;
        return match hex.len() {
            6 => Some([(v >> 16) as u8, (v >> 8) as u8, v as u8, 255]),
            3 => Some([
                ((v >> 8) & 0xf) as u8 * 17,
                ((v >> 4) & 0xf) as u8 * 17,
                (v & 0xf) as u8 * 17,
                255,
            ]),
            _ => None,
        };
    }
    Some(match s.to_ascii_lowercase().as_str() {
        "white" => [255, 255, 255, 255],
        "black" => [0, 0, 0, 255],
        "red" => [255, 0, 0, 255],
        "lime" | "green" => [0, 255, 0, 255],
        "blue" => [0, 0, 255, 255],
        "yellow" => [255, 255, 0, 255],
        "cyan" => [0, 255, 255, 255],
        "magenta" => [255, 0, 255, 255],
        _ => return None,
    })
}

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", "\u{a0}")
        .replace("&lrm;", "")
        .replace("&rlm;", "")
        .replace("&amp;", "&")
}

/// Parse inline markup into spans. Returns the spans and an alignment
/// override from `{\anN}`.
pub fn parse_markup(text: &str) -> (Vec<Span>, Option<u8>) {
    #[derive(Clone, Copy, Default)]
    struct St {
        b: bool,
        i: bool,
        u: bool,
        color: Option<[u8; 4]>,
    }
    let mut stack: Vec<(String, St)> = Vec::new();
    let mut cur = St::default();
    let mut spans: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let mut align = None;
    let flush = |buf: &mut String, spans: &mut Vec<Span>, st: St| {
        if buf.is_empty() {
            return;
        }
        let text = decode_entities(buf);
        buf.clear();
        if let Some(last) = spans.last_mut() {
            if last.bold == st.b
                && last.italic == st.i
                && last.underline == st.u
                && last.color == st.color
            {
                last.text.push_str(&text);
                return;
            }
        }
        spans.push(Span {
            text,
            bold: st.b,
            italic: st.i,
            underline: st.u,
            color: st.color,
        });
    };
    let mut rest = text;
    while !rest.is_empty() {
        if rest.starts_with('{') {
            if let Some(end) = rest.find('}') {
                let inner = &rest[1..end];
                if inner.starts_with('\\') {
                    for tag in inner.split('\\').filter(|t| !t.is_empty()) {
                        if let Some(n) = tag.strip_prefix("an").and_then(|n| n.parse::<u8>().ok()) {
                            align = Some(n.clamp(1, 9));
                        } else if tag == "i1" {
                            flush(&mut buf, &mut spans, cur);
                            cur.i = true;
                        } else if tag == "i0" {
                            flush(&mut buf, &mut spans, cur);
                            cur.i = false;
                        } else if tag == "b1" {
                            flush(&mut buf, &mut spans, cur);
                            cur.b = true;
                        } else if tag == "b0" {
                            flush(&mut buf, &mut spans, cur);
                            cur.b = false;
                        }
                    }
                    rest = &rest[end + 1..];
                    continue;
                }
            }
        }
        if rest.starts_with('<') {
            if let Some(end) = rest.find('>') {
                let tag = &rest[1..end];
                let closing = tag.starts_with('/');
                let name_full = tag.trim_start_matches('/');
                let name = name_full
                    .split(|c: char| c == '.' || c.is_whitespace())
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let known = matches!(
                    name.as_str(),
                    "b" | "i" | "u" | "font" | "c" | "v" | "lang" | "ruby" | "rt" | "s"
                );
                // WebVTT inline timestamps (<00:00:01.000>) are dropped.
                let is_timestamp = name_full.chars().next().is_some_and(|c| c.is_ascii_digit());
                if known || is_timestamp {
                    flush(&mut buf, &mut spans, cur);
                    if is_timestamp {
                    } else if closing {
                        if let Some(i) = stack.iter().rposition(|(n, _)| *n == name) {
                            cur = stack[i].1;
                            stack.truncate(i);
                        }
                    } else {
                        stack.push((name.clone(), cur));
                        match name.as_str() {
                            "b" => cur.b = true,
                            "i" => cur.i = true,
                            "u" => cur.u = true,
                            "font" => {
                                let lower = name_full.to_ascii_lowercase();
                                if let Some(i) = lower.find("color=") {
                                    let v = &name_full[i + 6..];
                                    let v = v.trim_start_matches(['"', '\'']);
                                    let end = v
                                        .find(|c: char| c == '"' || c == '\'' || c.is_whitespace())
                                        .unwrap_or(v.len());
                                    cur.color = parse_color(&v[..end]).or(cur.color);
                                }
                            }
                            "c" => {
                                // <c.yellow> class colours.
                                for class in name_full.split('.').skip(1) {
                                    if let Some(c) = parse_color(class) {
                                        cur.color = Some(c);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    rest = &rest[end + 1..];
                    continue;
                }
            }
        }
        let ch = rest.chars().next().unwrap();
        buf.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    flush(&mut buf, &mut spans, cur);
    (spans, align)
}

/// Markup of one cue → [`TextCue`].
pub fn parse_cue_text(text: &str) -> TextCue {
    let text = text.replace("\r\n", "\n");
    let (spans, align) = parse_markup(text.trim_end_matches('\n'));
    TextCue {
        spans,
        alignment: align.unwrap_or(2),
        position: None,
        style: None,
        layer: 0,
    }
}

/// Parse a whole `.srt` file.
pub fn parse(text: &str) -> Result<SubtitleTrack> {
    let text = text
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut cues = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if let Some((a, b)) = line.split_once("-->") {
            let start = super::parse_timestamp(a);
            // Strip trailing positioning (X1:… Y2:…).
            let b = b.split_whitespace().next().unwrap_or("");
            let end = super::parse_timestamp(b);
            i += 1;
            let mut body = Vec::new();
            while i < lines.len() && !lines[i].trim().is_empty() {
                // A missing blank line before the next cue: stop at "N\n--> " patterns.
                if lines[i].contains("-->") && !body.is_empty() {
                    if body
                        .last()
                        .is_some_and(|l: &&str| l.trim().parse::<u64>().is_ok())
                    {
                        body.pop();
                    }
                    break;
                }
                body.push(lines[i]);
                i += 1;
            }
            if let (Some(s), Some(e)) = (start, end) {
                if e > s {
                    cues.push(Cue::text(s, e, parse_cue_text(&body.join("\n"))));
                }
            }
            continue;
        }
        i += 1;
    }
    Ok(SubtitleTrack::new(cues))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitle::CueContent;
    use fp_core::MediaTime;

    const SAMPLE: &str = "\u{feff}1\r\n00:00:01,000 --> 00:00:04,000\r\nHello <b>bold</b> and <i>italic</i>\r\nSecond line\r\n\r\n2\r\n00:00:05,500 --> 00:00:07,000 X1:100 X2:200\r\n{\\an8}<font color=\"#ff0000\">Red top</font> &amp; more\r\n\r\n3\r\n00:00:08,000 --> 00:00:07,000\r\nbackwards (dropped)\r\n";

    #[test]
    fn parses_cues_and_markup() {
        let t = parse(SAMPLE).unwrap();
        assert_eq!(t.len(), 2);
        let c = &t.cues()[0];
        assert_eq!(
            (c.start, c.end),
            (MediaTime::from_millis(1000), MediaTime::from_millis(4000))
        );
        let CueContent::Text(tc) = &c.content else {
            panic!()
        };
        assert_eq!(tc.plain_text(), "Hello bold and italic\nSecond line");
        assert!(tc.spans[1].bold && !tc.spans[1].italic);
        assert!(tc.spans[3].italic);
        let CueContent::Text(tc) = &t.cues()[1].content else {
            panic!()
        };
        assert_eq!(tc.alignment, 8);
        assert_eq!(tc.spans[0].color, Some([255, 0, 0, 255]));
        assert_eq!(tc.plain_text(), "Red top & more");
        assert_eq!(t.active_at(MediaTime::from_millis(6000)).len(), 1);
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#0f0"), Some([0, 255, 0, 255]));
        assert_eq!(parse_color("yellow"), Some([255, 255, 0, 255]));
        assert_eq!(parse_color("bogus"), None);
    }
}
