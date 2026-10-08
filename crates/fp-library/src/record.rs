//! Rows of the library as Rust values.

use std::fmt;
use std::path::PathBuf;

use fp_core::format::{DetectedFormat, Evidence};
use fp_core::view::Keyframes;
use fp_core::{Projection, StereoLayout, VideoFormat, ViewSettings};
use rusqlite::Row;
use rusqlite::types::Type;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// Primary key of a media row. Stable for the life of the database.
    MediaId
);
id_type!(
    /// Primary key of a playlist.
    PlaylistId
);
id_type!(
    /// Primary key of a marker (bookmark).
    MarkerId
);
id_type!(
    /// One playback session in the watch history, returned by
    /// [`Library::start_playback`](crate::Library::start_playback).
    SessionId
);

/// Coarse projection family, used for filtering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionKind {
    /// Flat screen.
    Flat,
    /// Equirectangular up to 180° wide.
    Equirect180,
    /// Equirectangular wider than 180° (normally 360°).
    Equirect360,
    /// Any fisheye.
    Fisheye,
    /// Equi-angular cubemap.
    Eac,
}

impl ProjectionKind {
    /// Family of a concrete projection.
    pub fn of(projection: &Projection) -> ProjectionKind {
        match *projection {
            Projection::Flat => ProjectionKind::Flat,
            Projection::Equirect { h_fov, .. } if h_fov <= 180.0 => ProjectionKind::Equirect180,
            Projection::Equirect { .. } => ProjectionKind::Equirect360,
            Projection::Fisheye { .. } => ProjectionKind::Fisheye,
            Projection::Eac { .. } => ProjectionKind::Eac,
        }
    }

    /// Value stored in the `projection_kind` column.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ProjectionKind::Flat => "flat",
            ProjectionKind::Equirect180 => "equirect180",
            ProjectionKind::Equirect360 => "equirect360",
            ProjectionKind::Fisheye => "fisheye",
            ProjectionKind::Eac => "eac",
        }
    }
}

/// Value stored in the `stereo` column.
pub(crate) fn stereo_str(stereo: StereoLayout) -> &'static str {
    match stereo {
        StereoLayout::Mono => "mono",
        StereoLayout::SideBySide => "side_by_side",
        StereoLayout::TopBottom => "top_bottom",
    }
}

pub(crate) fn evidence_str(evidence: Evidence) -> &'static str {
    match evidence {
        Evidence::User => "user",
        Evidence::Metadata => "metadata",
        Evidence::FileName => "file_name",
        Evidence::AspectRatio => "aspect_ratio",
        Evidence::Default => "default",
    }
}

fn parse_evidence(s: &str) -> Evidence {
    match s {
        "user" => Evidence::User,
        "metadata" => Evidence::Metadata,
        "file_name" => Evidence::FileName,
        "aspect_ratio" => Evidence::AspectRatio,
        _ => Evidence::Default,
    }
}

/// `(projection_kind, stereo)` column values for the effective format.
pub(crate) fn effective_columns(
    detected: &VideoFormat,
    user: Option<&VideoFormat>,
) -> (&'static str, &'static str) {
    let f = user.unwrap_or(detected);
    (
        ProjectionKind::of(&f.projection).as_str(),
        stereo_str(f.stereo),
    )
}

/// A sprite sheet of small frames at even intervals, for scrub previews.
///
/// Frame `i` (zero based) shows time `(i + 0.5) * interval` and sits at
/// column `i % columns`, row `i / columns`, each cell `cell_width` by
/// `cell_height` pixels. Frames smaller than a cell are centred on black.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewStrip {
    /// JPEG file, `<cache>/<id>_strip.jpg`.
    pub path: PathBuf,
    /// Number of frames.
    pub frames: u32,
    /// Frames per row.
    pub columns: u32,
    /// Width of one cell in pixels.
    pub cell_width: u32,
    /// Height of one cell in pixels.
    pub cell_height: u32,
    /// Seconds between frames.
    pub interval: f64,
}

/// One video in the library, mirroring a `media` row plus its tags.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaRecord {
    /// Row id.
    pub id: MediaId,
    /// Path for local files, URL for remote ones. Unique.
    pub location: String,
    /// Which configured source produced this row (`"local"`, a share id...).
    pub source_id: String,
    /// Display title (file stem unless the source gave one).
    pub title: String,
    /// Bytes, when known.
    pub size: Option<u64>,
    /// Modification time, Unix seconds, when known.
    pub mtime: Option<i64>,
    /// Seconds, once probed or declared by the source.
    pub duration: Option<f64>,
    /// Frame width in pixels, once probed.
    pub width: Option<u32>,
    /// Frame height in pixels, once probed.
    pub height: Option<u32>,
    /// Video codec name reported by the prober (`"hevc"`, `"av1"`...).
    pub video_codec: Option<String>,
    /// Format detected from metadata, file name or aspect ratio.
    pub detected: DetectedFormat,
    /// Format the user forced for this file.
    pub user_format: Option<VideoFormat>,
    /// Per-video picture corrections, when the user changed any.
    pub view_settings: Option<ViewSettings>,
    /// Settings that change over the timeline.
    pub keyframes: Keyframes,
    /// 0 (unrated) to 5.
    pub rating: u8,
    /// Marked as favourite.
    pub favorite: bool,
    /// Times watched to the end (or past 90 %).
    pub play_count: u32,
    /// Unix seconds of the last playback.
    pub last_played: Option<i64>,
    /// Seconds to resume from; 0 when not started or finished.
    pub resume_position: f64,
    /// Unix seconds when first indexed.
    pub added_at: i64,
    /// The file was not found by the last scan of its folder.
    pub missing: bool,
    /// Generated thumbnail JPEG.
    pub thumbnail: Option<PathBuf>,
    /// Generated scrub preview sprite sheet.
    pub preview_strip: Option<PreviewStrip>,
    /// Thumbnail offered by a remote source (DeoVR feed, DLNA).
    pub thumbnail_url: Option<String>,
    /// The metadata worker has processed this row.
    pub probed: bool,
    /// Why the metadata worker failed on this row, if it did.
    pub probe_error: Option<String>,
    /// Haptic scripts: the main `stem.funscript` first, then axis scripts.
    pub scripts: Vec<String>,
    /// Subtitle files.
    pub subtitles: Vec<String>,
    /// Tag names, sorted case-insensitively.
    pub tags: Vec<String>,
}

impl MediaRecord {
    /// The format to play with: the user's override when set, else the
    /// detected one (the precedence of [`fp_core::format::resolve`]).
    pub fn effective_format(&self) -> DetectedFormat {
        match self.user_format {
            Some(format) => DetectedFormat {
                format,
                evidence: Evidence::User,
            },
            None => self.detected,
        }
    }

    /// Last path segment of the location.
    pub fn file_name(&self) -> &str {
        file_name_of(&self.location)
    }

    /// Fraction watched (0..=1) from the resume position, when the duration
    /// is known.
    pub fn progress(&self) -> Option<f64> {
        self.duration
            .filter(|d| *d > 0.0)
            .map(|d| (self.resume_position / d).clamp(0.0, 1.0))
    }
}

/// Last segment of a path or URL, without a query string.
pub(crate) fn file_name_of(location: &str) -> &str {
    let no_query = location.split(['?', '#']).next().unwrap_or(location);
    let trimmed = no_query.trim_end_matches(['/', '\\']);
    trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed)
}

/// File name without its extension.
pub(crate) fn stem_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// A timestamped bookmark inside a video.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    /// Row id.
    pub id: MarkerId,
    /// Video it belongs to.
    pub media_id: MediaId,
    /// Seconds from the start.
    pub time: f64,
    /// Label.
    pub name: String,
    /// Provided by the source (chapters); replaced on re-index, never
    /// exported. User markers have this `false`.
    pub from_source: bool,
}

/// One playback session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Session id.
    pub id: SessionId,
    /// Video watched.
    pub media_id: MediaId,
    /// Unix seconds when playback started.
    pub started_at: i64,
    /// Unix seconds of the last progress report.
    pub updated_at: i64,
    /// Last reported position, seconds.
    pub position: f64,
    /// The session counted as a full watch.
    pub completed: bool,
}

/// A tag and how many videos carry it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagCount {
    /// Tag name as first written.
    pub name: String,
    /// Number of videos.
    pub count: usize,
}

/// Columns read by [`media_from_row`], for a `media` table aliased `m`.
pub(crate) const MEDIA_COLUMNS: &str = "m.id, m.location, m.source_id, m.title, m.size, m.mtime,
    m.duration, m.width, m.height, m.video_codec, m.detected_format, m.detected_evidence,
    m.user_format, m.view_settings, m.keyframes, m.rating, m.favorite, m.play_count,
    m.last_played, m.resume_position, m.added_at, m.missing, m.thumbnail, m.preview_strip,
    m.thumbnail_url, m.probed, m.probe_error, m.scripts, m.subtitles,
    (SELECT json_group_array(t.name ORDER BY t.name COLLATE NOCASE)
       FROM media_tags mt JOIN tags t ON t.id = mt.tag_id WHERE mt.media_id = m.id)";

fn json_col<T: DeserializeOwned>(row: &Row<'_>, idx: usize) -> rusqlite::Result<T> {
    let text: String = row.get(idx)?;
    serde_json::from_str(&text)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e)))
}

fn json_opt<T: DeserializeOwned>(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<T>> {
    match row.get::<_, Option<String>>(idx)? {
        None => Ok(None),
        Some(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e))),
    }
}

/// The format the viewer chose. One saved before masks existed (no
/// `alpha_packed`) takes the mask from the file name, as detection does, so
/// an `_alpha` video uses its mask; one saved since keeps its own choice.
fn user_format_col(
    row: &Row<'_>,
    idx: usize,
    location: &str,
) -> rusqlite::Result<Option<VideoFormat>> {
    let Some(text) = row.get::<_, Option<String>>(idx)? else {
        return Ok(None);
    };
    let mut f: VideoFormat = serde_json::from_str(&text)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e)))?;
    if !text.contains("alpha_packed") {
        f.alpha_packed = fp_core::format::detect_from_name(file_name_of(location))
            .is_some_and(|d| d.alpha_packed);
    }
    Ok(Some(f))
}

fn u32_opt(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<u32>> {
    Ok(row
        .get::<_, Option<i64>>(idx)?
        .map(|v| v.clamp(0, u32::MAX as i64) as u32))
}

/// Builds a record from a row selected with [`MEDIA_COLUMNS`].
pub(crate) fn media_from_row(row: &Row<'_>) -> rusqlite::Result<MediaRecord> {
    let evidence: String = row.get(11)?;
    let location: String = row.get(1)?;
    Ok(MediaRecord {
        id: MediaId(row.get(0)?),
        user_format: user_format_col(row, 12, &location)?,
        location,
        source_id: row.get(2)?,
        title: row.get(3)?,
        size: row.get::<_, Option<i64>>(4)?.map(|v| v.max(0) as u64),
        mtime: row.get(5)?,
        duration: row.get(6)?,
        width: u32_opt(row, 7)?,
        height: u32_opt(row, 8)?,
        video_codec: row.get(9)?,
        detected: DetectedFormat {
            format: json_col(row, 10)?,
            evidence: parse_evidence(&evidence),
        },
        view_settings: json_opt(row, 13)?,
        keyframes: json_col(row, 14)?,
        rating: row.get::<_, i64>(15)?.clamp(0, 5) as u8,
        favorite: row.get(16)?,
        play_count: row.get::<_, i64>(17)?.clamp(0, u32::MAX as i64) as u32,
        last_played: row.get(18)?,
        resume_position: row.get(19)?,
        added_at: row.get(20)?,
        missing: row.get(21)?,
        thumbnail: row.get::<_, Option<String>>(22)?.map(PathBuf::from),
        preview_strip: json_opt(row, 23)?,
        thumbnail_url: row.get(24)?,
        probed: row.get(25)?,
        probe_error: row.get(26)?,
        scripts: json_col(row, 27)?,
        subtitles: json_col(row, 28)?,
        tags: json_col(row, 29)?,
    })
}

/// Current time in Unix seconds.
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_kinds() {
        assert_eq!(
            ProjectionKind::of(&Projection::EQUIRECT_180),
            ProjectionKind::Equirect180
        );
        assert_eq!(
            ProjectionKind::of(&Projection::EQUIRECT_360),
            ProjectionKind::Equirect360
        );
        assert_eq!(
            ProjectionKind::of(&Projection::fisheye(200.0)),
            ProjectionKind::Fisheye
        );
        assert_eq!(
            ProjectionKind::of(&Projection::Eac { h_fov: 360.0 }),
            ProjectionKind::Eac
        );
        assert_eq!(ProjectionKind::of(&Projection::Flat), ProjectionKind::Flat);
    }

    #[test]
    fn evidence_round_trips() {
        for e in [
            Evidence::User,
            Evidence::Metadata,
            Evidence::FileName,
            Evidence::AspectRatio,
            Evidence::Default,
        ] {
            assert_eq!(parse_evidence(evidence_str(e)), e);
        }
    }

    #[test]
    fn names() {
        assert_eq!(file_name_of("/a/b/c.mp4"), "c.mp4");
        assert_eq!(file_name_of("http://h/x/y.mp4?token=1"), "y.mp4");
        assert_eq!(file_name_of("smb://h/share/dir/"), "dir");
        assert_eq!(stem_of("a.b.mp4"), "a.b");
        assert_eq!(stem_of(".hidden"), ".hidden");
    }
}
