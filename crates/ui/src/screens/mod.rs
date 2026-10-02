//! App screens built from widgets.
//!
//! Each screen is a small struct holding UI-local state (search text, open
//! menus, keyboard state) with a `show(&mut self, ui, &view) -> Vec<UiAction>`
//! method. Views are plain data filled by the app; actions are mapped by the
//! app to player/library/config commands. No other fp crate is referenced.

pub mod library;
pub mod picture;
pub mod player;
pub mod settings;

pub use library::{LibraryItem, LibraryScreen, LibraryView, SortKey, SourceEntry, SourceKind};
pub use picture::{PictureAdjustScreen, PictureView};
pub use player::{PlayerControls, PlayerView, ScriptStatus, TrackEntry};
pub use settings::{
    GeneralSettings, HapticsDevice, HapticsSettings, PlaybackSettings, RemoteSettings,
    SettingsModel, SettingsScreen, SettingsTab, UpdateSettings,
};

use crate::geom::Rect;
use crate::ui::Ui;
use fp_core::{Codec, Corrections, MediaTime, Projection, StereoMode};
use std::ops::Range;

/// Everything a screen can ask the app to do.
#[derive(Debug, Clone, PartialEq)]
pub enum UiAction {
    // Navigation
    Back,
    OpenSettings,
    OpenLibrary,
    OpenPictureAdjust,
    ClosePictureAdjust,

    // Library
    SelectSource(u64),
    AddSource,
    RefreshSource(u64),
    Search(String),
    SetSort {
        key: SortKey,
        descending: bool,
    },
    ToggleTag(String),
    ClearTags,
    SetFavouritesOnly(bool),
    OpenItem(u64),
    ToggleFavourite(u64),
    /// Items currently built by the virtualized grid; load their thumbnails.
    VisibleItems(Range<usize>),

    // Player
    TogglePlay,
    /// Committed seek.
    Seek(MediaTime),
    /// Live drag position (fast keyframe seek / preview).
    Scrub(MediaTime),
    SeekRelative(f64),
    SetSpeed(f32),
    SetProjection(Projection),
    SetStereo(StereoMode),
    SetSwapEyes(bool),
    SelectSubtitle(Option<u32>),
    SelectAudio(u32),
    SetLoopA(MediaTime),
    SetLoopB(MediaTime),
    ClearLoop,
    Recenter,
    /// Hovered timeline time; the app should provide a preview sprite.
    RequestPreview(MediaTime),
    SetScriptEnabled(bool),

    // Picture adjust
    SetCorrections(Corrections),
    AddKeyframe,
    ClearKeyframes,
    ResetCorrections,

    // Settings
    SettingsChanged(SettingsModel),
    ScanHapticsDevices,
    ConnectHapticsDevice(String),
    DisconnectHapticsDevice(String),
    RegenerateApiToken,
    CheckForUpdates,
    InstallUpdate,
}

/// Fills the whole panel with the themed background card.
pub fn panel_background(ui: &mut Ui) -> Rect {
    let r = ui.screen_rect();
    let (bg, rad) = (ui.theme.panel_bg, ui.theme.corner_radius * 2.0);
    ui.painter().rect_rounded(r, rad, bg);
    r
}

/// "8K" / "6K" / "5K" / "4K" / "1080p" style label from frame size.
pub fn resolution_label(width: u32, height: u32) -> String {
    let long = width.max(height);
    match long {
        7680.. => "8K".into(),
        5760.. => "6K".into(),
        5120.. => "5K".into(),
        3840.. => "4K".into(),
        2560.. => "2.5K".into(),
        1920.. => "1080p".into(),
        0 => String::new(),
        _ => format!("{}p", width.min(height)),
    }
}

pub fn codec_label(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
        Codec::Vp9 => "VP9",
        Codec::Av1 => "AV1",
        Codec::Other => "Other",
    }
}

/// "180° SBS", "Flat", "MKX200 SBS"…
pub fn projection_label(p: &Projection, s: StereoMode) -> String {
    match s {
        StereoMode::Mono => p.label(),
        _ => format!("{} {}", p.label(), s.label()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(resolution_label(7680, 3840), "8K");
        assert_eq!(resolution_label(5760, 2880), "6K");
        assert_eq!(resolution_label(3840, 2160), "4K");
        assert_eq!(resolution_label(1920, 1080), "1080p");
        assert_eq!(resolution_label(1280, 720), "720p");
        assert_eq!(resolution_label(0, 0), "");
        assert_eq!(codec_label(Codec::Hevc), "HEVC");
        assert_eq!(
            projection_label(&Projection::EQUIRECT_180, StereoMode::Sbs),
            "180° SBS"
        );
        assert_eq!(
            projection_label(&Projection::FLAT_DEFAULT, StereoMode::Mono),
            "Flat"
        );
    }
}
