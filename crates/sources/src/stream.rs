//! Adaptive streams (HLS / DASH) as the player sees them: a list of
//! variants to choose from and a [`SegmentStream`] for the chosen one.
//!
//! Playback is VOD-oriented: a live HLS playlist is fetched once (no
//! refresh loop), which covers the "stream from my server" use case.

use crate::dash;
use crate::error::{Result, SourceError};
use crate::hls::{self, Playlist};
use crate::http::HttpClient;
use crate::segments::SegmentStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFormat {
    Hls,
    Dash,
}

/// A selectable rendition of an adaptive stream.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamVariant {
    /// Opaque handle for [`open_variant`]: media playlist URL for HLS,
    /// representation id for DASH.
    pub id: String,
    pub bandwidth: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub frame_rate: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct StreamManifest {
    pub url: String,
    pub format: StreamFormat,
    pub variants: Vec<StreamVariant>,
    hls_media: Option<hls::MediaPlaylist>,
    dash: Option<dash::Mpd>,
}

impl StreamManifest {
    /// Variant with the highest bandwidth whose height ≤ `max_height`.
    pub fn best_variant(&self, max_height: Option<u32>) -> Option<&StreamVariant> {
        let fits = |v: &&StreamVariant| max_height.is_none_or(|m| v.height.is_none_or(|h| h <= m));
        self.variants
            .iter()
            .filter(fits)
            .max_by_key(|v| v.bandwidth)
            .or_else(|| self.variants.iter().min_by_key(|v| v.bandwidth))
    }
}

/// Guess whether a URL is an adaptive manifest from its path.
pub fn detect_format(url: &str) -> Option<StreamFormat> {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if path.ends_with(".m3u8") || path.ends_with(".m3u") {
        Some(StreamFormat::Hls)
    } else if path.ends_with(".mpd") {
        Some(StreamFormat::Dash)
    } else {
        None
    }
}

/// Fetch and parse a manifest. The format is sniffed from the content, so
/// extension-less URLs work too.
pub async fn resolve(client: &HttpClient, url: &str) -> Result<StreamManifest> {
    let resp = client.get(url).await?;
    let final_url = resp.url().to_string();
    let text = resp.text().await?;
    from_text(&text, &final_url)
}

/// Build a manifest from already-fetched text.
pub fn from_text(text: &str, url: &str) -> Result<StreamManifest> {
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with("#EXTM3U") {
        return Ok(match hls::parse(text, url)? {
            Playlist::Master(m) => StreamManifest {
                url: url.into(),
                format: StreamFormat::Hls,
                variants: m
                    .variants
                    .iter()
                    .filter(|v| !v.iframe_only)
                    .map(|v| StreamVariant {
                        id: v.uri.clone(),
                        bandwidth: v.bandwidth,
                        width: v.resolution.map(|r| r.0),
                        height: v.resolution.map(|r| r.1),
                        codecs: v.codecs.clone(),
                        frame_rate: v.frame_rate,
                    })
                    .collect(),
                hls_media: None,
                dash: None,
            },
            Playlist::Media(p) => StreamManifest {
                url: url.into(),
                format: StreamFormat::Hls,
                variants: vec![StreamVariant {
                    id: url.into(),
                    bandwidth: 0,
                    width: None,
                    height: None,
                    codecs: None,
                    frame_rate: None,
                }],
                hls_media: Some(p),
                dash: None,
            },
        });
    }
    if trimmed.starts_with('<') {
        let mpd = dash::parse(text, url)?;
        let variants = mpd
            .video_representations()
            .map(|r| StreamVariant {
                id: r.id.clone(),
                bandwidth: r.bandwidth,
                width: r.width,
                height: r.height,
                codecs: r.codecs.clone(),
                frame_rate: r.frame_rate,
            })
            .collect();
        return Ok(StreamManifest {
            url: url.into(),
            format: StreamFormat::Dash,
            variants,
            hls_media: None,
            dash: Some(mpd),
        });
    }
    Err(SourceError::parse("neither an HLS playlist nor a DASH MPD"))
}

/// Open the segment stream for `variant_id` (see [`StreamVariant::id`]).
///
/// For DASH this returns the video representation only; demuxed audio
/// adaptation sets can be opened with [`open_dash_representation`].
pub async fn open_variant(
    client: &HttpClient,
    manifest: &StreamManifest,
    variant_id: &str,
) -> Result<SegmentStream> {
    match manifest.format {
        StreamFormat::Hls => {
            let media = match &manifest.hls_media {
                Some(m) if variant_id == manifest.url => m.clone(),
                _ => {
                    let resp = client.get(variant_id).await?;
                    let url = resp.url().to_string();
                    match hls::parse(&resp.text().await?, &url)? {
                        Playlist::Media(m) => m,
                        Playlist::Master(_) => {
                            return Err(SourceError::Protocol(
                                "variant URL is another master playlist".into(),
                            ))
                        }
                    }
                }
            };
            Ok(SegmentStream::new(
                client.clone(),
                media.init,
                media.segments,
            ))
        }
        StreamFormat::Dash => open_dash_representation(client, manifest, variant_id),
    }
}

/// Open any DASH representation (video, audio or text) by id.
pub fn open_dash_representation(
    client: &HttpClient,
    manifest: &StreamManifest,
    rep_id: &str,
) -> Result<SegmentStream> {
    let mpd = manifest
        .dash
        .as_ref()
        .ok_or_else(|| SourceError::Protocol("not a DASH manifest".into()))?;
    let rep = mpd
        .periods
        .first()
        .into_iter()
        .flat_map(|p| p.adaptation_sets.iter())
        .flat_map(|a| a.representations.iter())
        .find(|r| r.id == rep_id)
        .ok_or_else(|| SourceError::NotFound(format!("representation {rep_id}")))?;
    Ok(SegmentStream::new(
        client.clone(),
        rep.init.clone(),
        rep.segments.clone(),
    ))
}

/// The parsed MPD, for callers that need audio/text adaptation sets.
pub fn dash_mpd(manifest: &StreamManifest) -> Option<&dash::Mpd> {
    manifest.dash.as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_detection() {
        assert_eq!(
            detect_format("https://h/x/master.m3u8?t=1"),
            Some(StreamFormat::Hls)
        );
        assert_eq!(
            detect_format("https://h/x/Manifest.MPD"),
            Some(StreamFormat::Dash)
        );
        assert_eq!(detect_format("https://h/x.mp4"), None);
    }

    #[test]
    fn manifest_from_text() {
        let m = from_text(
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=5,RESOLUTION=10x20\nv.m3u8\n",
            "http://h/m.m3u8",
        )
        .unwrap();
        assert_eq!(m.format, StreamFormat::Hls);
        assert_eq!(m.variants[0].id, "http://h/v.m3u8");
        assert_eq!(m.best_variant(None).unwrap().height, Some(20));
        let d = from_text(r#"<MPD mediaPresentationDuration="PT4S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="r" bandwidth="9" height="1080"><BaseURL>v.mp4</BaseURL></Representation></AdaptationSet></Period></MPD>"#, "http://h/a.mpd").unwrap();
        assert_eq!(d.format, StreamFormat::Dash);
        let s = open_dash_representation(&HttpClient::new(None).unwrap(), &d, "r").unwrap();
        assert_eq!(s.segments()[0].url, "http://h/v.mp4");
        assert!(from_text("garbage", "http://h/").is_err());
    }
}
