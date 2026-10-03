//! WebVTT parsing (cue timings, `line`/`position`/`align` settings, inline
//! markup; `NOTE`, `STYLE` and `REGION` blocks are skipped).

use super::srt::parse_markup;
use super::{Cue, SubtitleTrack, TextCue};
use crate::error::{Result, VideoError};

fn percent(v: &str) -> Option<f32> {
    v.strip_suffix('%')
        .and_then(|n| n.trim().parse::<f32>().ok())
        .map(|p| (p / 100.0).clamp(0.0, 1.0))
}

/// Cue payload + settings string → [`TextCue`].
pub fn parse_cue_text(payload: &str, settings: &str) -> TextCue {
    let (spans, an) = parse_markup(payload.trim_end_matches('\n'));
    let mut align_h = 2u8; // centre
    let mut line: Option<f32> = None;
    let mut pos: Option<f32> = None;
    for s in settings.split_whitespace() {
        let Some((k, v)) = s.split_once(':') else {
            continue;
        };
        match k {
            "align" => {
                align_h = match v {
                    "start" | "left" => 1,
                    "end" | "right" => 3,
                    _ => 2,
                }
            }
            "line" => line = percent(v.split(',').next().unwrap_or("")),
            "position" => pos = percent(v.split(',').next().unwrap_or("")),
            _ => {}
        }
    }
    // Numpad alignment: bottom row by default, top row if the line is in the upper third.
    let row_base = match line {
        Some(l) if l < 0.33 => 6,
        Some(l) if l < 0.66 => 3,
        _ => 0,
    };
    let alignment = an.unwrap_or(row_base + align_h);
    let position = if line.is_some() || pos.is_some() {
        Some((pos.unwrap_or(0.5), line.unwrap_or(0.9)))
    } else {
        None
    };
    TextCue {
        spans,
        alignment,
        position,
        style: None,
        layer: 0,
    }
}

pub fn parse(text: &str) -> Result<SubtitleTrack> {
    let text = text
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    if !text.trim_start().starts_with("WEBVTT") {
        return Err(VideoError::invalid("missing WEBVTT header"));
    }
    let mut cues = Vec::new();
    for block in text.split("\n\n").skip(1) {
        let block = block.trim_matches('\n');
        if block.is_empty()
            || block.starts_with("NOTE")
            || block.starts_with("STYLE")
            || block.starts_with("REGION")
        {
            continue;
        }
        let mut lines = block.lines();
        let mut first = lines.next().unwrap_or("");
        if !first.contains("-->") {
            // Cue identifier.
            first = lines.next().unwrap_or("");
        }
        let Some((a, rest)) = first.split_once("-->") else {
            continue;
        };
        let rest = rest.trim();
        let (b, settings) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let (Some(s), Some(e)) = (super::parse_timestamp(a), super::parse_timestamp(b)) else {
            continue;
        };
        if e <= s {
            continue;
        }
        let payload: Vec<&str> = lines.collect();
        cues.push(Cue::text(
            s,
            e,
            parse_cue_text(&payload.join("\n"), settings),
        ));
    }
    Ok(SubtitleTrack::new(cues))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitle::CueContent;
    use fp_core::MediaTime;

    #[test]
    fn parses_vtt() {
        let vtt = "WEBVTT - demo\n\nNOTE a comment\n\nSTYLE\n::cue { color: red }\n\nintro\n00:01.000 --> 00:03.500 align:start line:10%\n<v Bob>Hi <c.yellow>there</c></v> &lt;3\n\n00:00:04.000 --> 00:00:05.000\n<i>two</i>\n<00:00:04.500>lines\n";
        let t = parse(vtt).unwrap();
        assert_eq!(t.len(), 2);
        let c = &t.cues()[0];
        assert_eq!(
            (c.start, c.end),
            (MediaTime::from_millis(1000), MediaTime::from_millis(3500))
        );
        let CueContent::Text(tc) = &c.content else {
            panic!()
        };
        assert_eq!(tc.plain_text(), "Hi there <3");
        assert_eq!(tc.alignment, 7, "top-left");
        assert_eq!(tc.spans[1].color, Some([255, 255, 0, 255]));
        assert!(tc.position.is_some());
        let CueContent::Text(tc) = &t.cues()[1].content else {
            panic!()
        };
        assert_eq!(tc.plain_text(), "two\nlines");
        assert!(parse("1\n00:00 --> 00:01\nx").is_err());
    }
}
