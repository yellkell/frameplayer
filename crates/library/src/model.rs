//! Rows as the rest of the app sees them.

use fp_core::{Codec, MediaTime, Projection, StereoMode};
use serde::{Deserialize, Serialize};

/// Coarse projection class used for filtering and badges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionKind {
    Flat,
    Equirect180,
    Equirect360,
    Fisheye,
    Eac,
    Mesh,
}

impl ProjectionKind {
    pub fn of(p: &Projection) -> ProjectionKind {
        match p {
            Projection::Flat { .. } => ProjectionKind::Flat,
            Projection::Equirect { h_fov_deg } if *h_fov_deg > 270.0 => ProjectionKind::Equirect360,
            Projection::Equirect { .. } => ProjectionKind::Equirect180,
            Projection::Fisheye { .. } => ProjectionKind::Fisheye,
            Projection::Eac => ProjectionKind::Eac,
            Projection::CustomMesh { .. } => ProjectionKind::Mesh,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ProjectionKind::Flat => "flat",
            ProjectionKind::Equirect180 => "equirect180",
            ProjectionKind::Equirect360 => "equirect360",
            ProjectionKind::Fisheye => "fisheye",
            ProjectionKind::Eac => "eac",
            ProjectionKind::Mesh => "mesh",
        }
    }

    pub fn is_immersive(self) -> bool {
        self != ProjectionKind::Flat
    }
}

/// Where an item's projection/stereo came from (higher wins on rescans).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectSource {
    Default,
    Filename,
    Feed,
    Container,
    User,
}

impl DetectSource {
    pub fn as_str(self) -> &'static str {
        match self {
            DetectSource::Default => "default",
            DetectSource::Filename => "filename",
            DetectSource::Feed => "feed",
            DetectSource::Container => "container",
            DetectSource::User => "user",
        }
    }

    pub fn parse(s: &str) -> DetectSource {
        match s {
            "filename" => DetectSource::Filename,
            "feed" => DetectSource::Feed,
            "container" => DetectSource::Container,
            "user" => DetectSource::User,
            _ => DetectSource::Default,
        }
    }
}

pub(crate) fn stereo_str(s: StereoMode) -> &'static str {
    match s {
        StereoMode::Mono => "mono",
        StereoMode::Sbs => "sbs",
        StereoMode::Ou => "ou",
    }
}

pub(crate) fn parse_stereo(s: &str) -> Option<StereoMode> {
    Some(match s {
        "mono" => StereoMode::Mono,
        "sbs" => StereoMode::Sbs,
        "ou" => StereoMode::Ou,
        _ => return None,
    })
}

pub(crate) fn codec_str(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
        Codec::Vp9 => "vp9",
        Codec::Av1 => "av1",
        Codec::Other => "other",
    }
}

pub(crate) fn parse_codec(s: &str) -> Option<Codec> {
    Some(match s {
        "h264" => Codec::H264,
        "hevc" => Codec::Hevc,
        "vp9" => Codec::Vp9,
        "av1" => Codec::Av1,
        "other" => Codec::Other,
        _ => return None,
    })
}

/// What the indexer knows about a file before probing it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NewItem {
    pub source_id: Option<String>,
    pub uri: String,
    /// Display path relative to the source root.
    pub path: String,
    pub title: String,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
    pub content_hash: Option<String>,
    pub duration: Option<MediaTime>,
    pub remote_thumbnail: Option<String>,
}

/// A library item with its user data joined in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: i64,
    pub source_id: Option<String>,
    pub uri: String,
    pub path: String,
    pub title: String,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
    pub content_hash: Option<String>,
    pub duration: Option<MediaTime>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codec: Option<Codec>,
    pub fps: Option<f64>,
    pub hdr: bool,
    pub projection: Option<Projection>,
    pub projection_kind: Option<ProjectionKind>,
    pub stereo: Option<StereoMode>,
    pub swap_eyes: bool,
    pub detect_source: DetectSource,
    pub thumbnail_path: Option<String>,
    pub sprite_path: Option<String>,
    pub remote_thumbnail: Option<String>,
    pub added_at: i64,
    pub probed: bool,
    // User data.
    pub rating: Option<f32>,
    pub favourite: bool,
    pub resume: Option<MediaTime>,
    pub last_watched: Option<i64>,
    pub watch_count: u32,
    pub tags: Vec<String>,
    pub has_script: bool,
}

impl Item {
    /// "8K", "5.7K", "4K", "1080p" style badge from the frame size.
    pub fn resolution_label(&self) -> Option<String> {
        let w = self.width?;
        let h = self.height?;
        let long = w.max(h);
        Some(match long {
            l if l >= 7600 => "8K".into(),
            l if l >= 6000 => "6K".into(),
            l if l >= 5600 => "5.7K".into(),
            l if l >= 3800 => "4K".into(),
            l if l >= 2500 => "2.7K".into(),
            _ => format!("{}p", w.min(h)),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bookmark {
    pub id: i64,
    pub item_id: i64,
    pub position: MediaTime,
    pub name: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub item_id: i64,
    pub watched_at: i64,
    pub position: Option<MediaTime>,
    pub completed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    /// Present for smart playlists.
    pub query: Option<crate::query::ItemQuery>,
    pub created_at: i64,
}
