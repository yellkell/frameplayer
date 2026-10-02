//! HLS (RFC 8216) playlist parsing: master playlists with variants and
//! renditions, media playlists with byte ranges, init maps and keys.
//! All URIs are resolved to absolute URLs against the playlist URL.

use crate::error::{Result, SourceError};
use crate::segments::{ByteRange, EncryptionKey, Segment};
use url::Url;

#[derive(Debug, Clone, PartialEq)]
pub enum Playlist {
    Master(MasterPlaylist),
    Media(MediaPlaylist),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MasterPlaylist {
    pub variants: Vec<Variant>,
    pub renditions: Vec<Rendition>,
}

impl MasterPlaylist {
    /// Highest-bandwidth variant whose height fits `max_height`, falling
    /// back to the smallest variant if none fit.
    pub fn best_variant(&self, max_height: Option<u32>) -> Option<&Variant> {
        let fits =
            |v: &&Variant| max_height.is_none_or(|m| v.resolution.is_none_or(|(_, h)| h <= m));
        self.variants
            .iter()
            .filter(fits)
            .max_by_key(|v| v.bandwidth)
            .or_else(|| self.variants.iter().min_by_key(|v| v.bandwidth))
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Variant {
    pub uri: String,
    pub bandwidth: u64,
    pub average_bandwidth: Option<u64>,
    pub resolution: Option<(u32, u32)>,
    pub codecs: Option<String>,
    pub frame_rate: Option<f64>,
    pub audio_group: Option<String>,
    pub subtitles_group: Option<String>,
    /// From EXT-X-I-FRAME-STREAM-INF (trick-play only).
    pub iframe_only: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Rendition {
    /// AUDIO, VIDEO, SUBTITLES, CLOSED-CAPTIONS.
    pub kind: String,
    pub group_id: String,
    pub name: String,
    pub language: Option<String>,
    pub uri: Option<String>,
    pub default: bool,
    pub autoselect: bool,
    pub channels: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MediaPlaylist {
    pub target_duration: f64,
    pub media_sequence: u64,
    /// VOD or EVENT, if declared.
    pub playlist_type: Option<String>,
    /// EXT-X-ENDLIST seen: the playlist is complete.
    pub ended: bool,
    /// EXT-X-MAP init segment (fMP4).
    pub init: Option<Segment>,
    pub segments: Vec<Segment>,
}

impl MediaPlaylist {
    pub fn duration(&self) -> f64 {
        self.segments.iter().map(|s| s.duration).sum()
    }

    pub fn is_live(&self) -> bool {
        !self.ended && self.playlist_type.as_deref() != Some("VOD")
    }
}

/// Parse an attribute list: `A=1,B="x,y",C=0x1F`.
pub fn parse_attributes(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim().to_string();
        rest = &rest[eq + 1..];
        let value;
        if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"').unwrap_or(r.len());
            value = r[..end].to_string();
            rest = r.get(end + 1..).unwrap_or("");
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            value = rest[..end].trim().to_string();
            rest = &rest[end..];
        }
        rest = rest.strip_prefix(',').unwrap_or(rest);
        out.push((key, value));
    }
    out
}

fn attr<'a>(a: &'a [(String, String)], k: &str) -> Option<&'a str> {
    a.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
}

fn resolve(base: &Url, uri: &str) -> String {
    base.join(uri)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| uri.to_string())
}

/// `n[@o]`
fn parse_byterange(s: &str) -> Option<(u64, Option<u64>)> {
    let (n, o) = match s.split_once('@') {
        Some((n, o)) => (n, Some(o.trim().parse().ok()?)),
        None => (s, None),
    };
    Some((n.trim().parse().ok()?, o))
}

/// Parse a playlist fetched from `base_url`.
pub fn parse(text: &str, base_url: &str) -> Result<Playlist> {
    let base = Url::parse(base_url)?;
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some("#EXTM3U") {
        return Err(SourceError::parse("not an M3U8 playlist (missing #EXTM3U)"));
    }
    let is_master =
        text.contains("#EXT-X-STREAM-INF") || text.contains("#EXT-X-I-FRAME-STREAM-INF");
    if is_master {
        parse_master(lines, &base).map(Playlist::Master)
    } else {
        parse_media(lines, &base).map(Playlist::Media)
    }
}

fn parse_master<'a>(lines: impl Iterator<Item = &'a str>, base: &Url) -> Result<MasterPlaylist> {
    let mut m = MasterPlaylist::default();
    let mut pending: Option<Variant> = None;
    for line in lines {
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(variant_from_attrs(&parse_attributes(rest)));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-I-FRAME-STREAM-INF:") {
            let a = parse_attributes(rest);
            let mut v = variant_from_attrs(&a);
            v.iframe_only = true;
            if let Some(u) = attr(&a, "URI") {
                v.uri = resolve(base, u);
                m.variants.push(v);
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let a = parse_attributes(rest);
            m.renditions.push(Rendition {
                kind: attr(&a, "TYPE").unwrap_or("").to_string(),
                group_id: attr(&a, "GROUP-ID").unwrap_or("").to_string(),
                name: attr(&a, "NAME").unwrap_or("").to_string(),
                language: attr(&a, "LANGUAGE").map(str::to_string),
                uri: attr(&a, "URI").map(|u| resolve(base, u)),
                default: attr(&a, "DEFAULT") == Some("YES"),
                autoselect: attr(&a, "AUTOSELECT") == Some("YES"),
                channels: attr(&a, "CHANNELS").map(str::to_string),
            });
        } else if !line.starts_with('#') {
            if let Some(mut v) = pending.take() {
                v.uri = resolve(base, line);
                m.variants.push(v);
            }
        }
    }
    Ok(m)
}

fn variant_from_attrs(a: &[(String, String)]) -> Variant {
    Variant {
        uri: String::new(),
        bandwidth: attr(a, "BANDWIDTH")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        average_bandwidth: attr(a, "AVERAGE-BANDWIDTH").and_then(|v| v.parse().ok()),
        resolution: attr(a, "RESOLUTION").and_then(|r| {
            let (w, h) = r.split_once(['x', 'X'])?;
            Some((w.parse().ok()?, h.parse().ok()?))
        }),
        codecs: attr(a, "CODECS").map(str::to_string),
        frame_rate: attr(a, "FRAME-RATE").and_then(|v| v.parse().ok()),
        audio_group: attr(a, "AUDIO").map(str::to_string),
        subtitles_group: attr(a, "SUBTITLES").map(str::to_string),
        iframe_only: false,
    }
}

fn parse_media<'a>(lines: impl Iterator<Item = &'a str>, base: &Url) -> Result<MediaPlaylist> {
    let mut p = MediaPlaylist::default();
    let mut duration: Option<f64> = None;
    let mut range: Option<(u64, Option<u64>)> = None;
    let mut discontinuity = false;
    let mut key: Option<EncryptionKey> = None;
    // Next implicit byte-range offset per resource URL.
    let mut next_offset: std::collections::HashMap<String, u64> = Default::default();

    for line in lines {
        if let Some(v) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            p.target_duration = v.trim().parse().unwrap_or(0.0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            p.media_sequence = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-PLAYLIST-TYPE:") {
            p.playlist_type = Some(v.trim().to_string());
        } else if line == "#EXT-X-ENDLIST" {
            p.ended = true;
        } else if line == "#EXT-X-DISCONTINUITY" {
            discontinuity = true;
        } else if let Some(v) = line.strip_prefix("#EXTINF:") {
            let d = v.split(',').next().unwrap_or("0").trim();
            duration = Some(
                d.parse()
                    .map_err(|_| SourceError::parse(format!("bad EXTINF duration {d:?}")))?,
            );
        } else if let Some(v) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            range = parse_byterange(v);
        } else if let Some(v) = line.strip_prefix("#EXT-X-KEY:") {
            let a = parse_attributes(v);
            let method = attr(&a, "METHOD").unwrap_or("NONE").to_string();
            key = (method != "NONE").then(|| EncryptionKey {
                method,
                uri: attr(&a, "URI").map(|u| resolve(base, u)),
                iv: attr(&a, "IV").map(str::to_string),
            });
        } else if let Some(v) = line.strip_prefix("#EXT-X-MAP:") {
            let a = parse_attributes(v);
            if let Some(u) = attr(&a, "URI") {
                let mut s = Segment::new(resolve(base, u), 0.0);
                s.range = attr(&a, "BYTERANGE")
                    .and_then(parse_byterange)
                    .map(|(n, o)| ByteRange {
                        offset: o.unwrap_or(0),
                        length: n,
                    });
                p.init = Some(s);
            }
        } else if !line.starts_with('#') {
            let url = resolve(base, line);
            let mut s = Segment::new(url.clone(), duration.take().unwrap_or(0.0));
            if let Some((n, o)) = range.take() {
                let offset = o.unwrap_or_else(|| *next_offset.get(&url).unwrap_or(&0));
                next_offset.insert(url, offset + n);
                s.range = Some(ByteRange { offset, length: n });
            }
            s.discontinuity = std::mem::take(&mut discontinuity);
            s.key = key.clone();
            p.segments.push(s);
        }
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = r#"#EXTM3U
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,CHANNELS="2",URI="audio/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=60000000,AVERAGE-BANDWIDTH=50000000,RESOLUTION=7680x3840,CODECS="hvc1.2.4.L183.B0,mp4a.40.2",FRAME-RATE=59.940,AUDIO="aud"
8k/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=20000000,RESOLUTION=3840x1920,CODECS="hvc1.2.4.L153.B0"
https://cdn.example.com/4k/index.m3u8?sig=a,b
#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1920x960,URI="iframes.m3u8"
"#;

    #[test]
    fn master_playlist() {
        let Playlist::Master(m) = parse(MASTER, "https://host/vr/master.m3u8").unwrap() else {
            panic!()
        };
        assert_eq!(m.variants.len(), 3);
        let v = &m.variants[0];
        assert_eq!(v.uri, "https://host/vr/8k/index.m3u8");
        assert_eq!(v.bandwidth, 60_000_000);
        assert_eq!(v.resolution, Some((7680, 3840)));
        assert_eq!(v.codecs.as_deref(), Some("hvc1.2.4.L183.B0,mp4a.40.2"));
        assert_eq!(v.frame_rate, Some(59.94));
        assert_eq!(v.audio_group.as_deref(), Some("aud"));
        assert_eq!(
            m.variants[1].uri,
            "https://cdn.example.com/4k/index.m3u8?sig=a,b"
        );
        assert!(m.variants[2].iframe_only);
        assert_eq!(
            m.renditions[0].uri.as_deref(),
            Some("https://host/vr/audio/en.m3u8")
        );
        assert!(m.renditions[0].default);
        assert_eq!(
            m.best_variant(Some(2160)).unwrap().resolution,
            Some((3840, 1920))
        );
        assert_eq!(m.best_variant(None).unwrap().bandwidth, 60_000_000);
        assert_eq!(m.best_variant(Some(100)).unwrap().bandwidth, 1_000_000);
    }

    #[test]
    fn media_playlist_with_byteranges() {
        let text = r#"#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:3
#EXT-X-PLAYLIST-TYPE:VOD
#EXT-X-MAP:URI="video.mp4",BYTERANGE="720@0"
#EXTINF:6.006,
#EXT-X-BYTERANGE:1000@720
video.mp4
#EXTINF:6.006,title
#EXT-X-BYTERANGE:2000
video.mp4
#EXT-X-DISCONTINUITY
#EXT-X-KEY:METHOD=AES-128,URI="key.bin",IV=0x1
#EXTINF:3.5,
seg3.m4s
#EXT-X-ENDLIST
"#;
        let Playlist::Media(p) = parse(text, "http://h/a/b.m3u8").unwrap() else {
            panic!()
        };
        assert_eq!(p.target_duration, 6.0);
        assert_eq!(p.media_sequence, 3);
        assert!(p.ended && !p.is_live());
        let init = p.init.as_ref().unwrap();
        assert_eq!(init.url, "http://h/a/video.mp4");
        assert_eq!(
            init.range,
            Some(ByteRange {
                offset: 0,
                length: 720
            })
        );
        assert_eq!(p.segments.len(), 3);
        assert_eq!(
            p.segments[0].range,
            Some(ByteRange {
                offset: 720,
                length: 1000
            })
        );
        assert_eq!(
            p.segments[1].range,
            Some(ByteRange {
                offset: 1720,
                length: 2000
            })
        );
        assert!(p.segments[2].discontinuity && !p.segments[1].discontinuity);
        assert_eq!(
            p.segments[2].key.as_ref().unwrap().uri.as_deref(),
            Some("http://h/a/key.bin")
        );
        assert!((p.duration() - 15.512).abs() < 1e-9);
    }

    #[test]
    fn rejects_non_m3u8() {
        assert!(parse("<html>", "http://h/").is_err());
        let Playlist::Media(p) = parse("#EXTM3U\n#EXTINF:2,\na.ts\n", "http://h/x/").unwrap()
        else {
            panic!()
        };
        assert!(p.is_live());
        assert_eq!(p.segments[0].url, "http://h/x/a.ts");
    }

    #[test]
    fn attribute_lists() {
        let a = parse_attributes(r#"A=1,B="x,y",C=0x1F,D="""#);
        assert_eq!(
            a,
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "x,y".into()),
                ("C".into(), "0x1F".into()),
                ("D".into(), "".into())
            ]
        );
    }
}
