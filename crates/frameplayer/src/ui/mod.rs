//! The in-headset interface. Each panel's UI is a function of the app
//! state; anything that changes more than view-local state is returned as
//! an [`Action`] for the app to apply.

pub mod icons;
pub mod keyboard;
pub mod library;
pub mod player;
pub mod settings;
pub mod sources;
pub mod theme;
pub mod thumbs;
pub mod widgets;

use crate::playback::OpenRequest;
use crate::settings::HapticDeviceConfig;
use fp_core::format::VideoFormat;
use fp_core::source::Entry;
use fp_library::{MediaId, Query};
use fp_sources::SourceConfig;
use std::path::PathBuf;

/// `h:mm:ss` or `m:ss`.
pub fn fmt_time(t: f64) -> String {
    let t = if t.is_finite() { t.max(0.0) as u64 } else { 0 };
    let (h, m, s) = (t / 3600, (t / 60) % 60, t % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn fmt_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else {
        format!("{:.0} kB", b / 1e3)
    }
}

/// Something the UI asks the app to do.
#[derive(Clone, Debug)]
pub enum Action {
    Open(OpenRequest),
    /// Open item `.1` of a list; grip + thumbstick moves through the list.
    OpenList(Vec<OpenRequest>, usize),
    TogglePause,
    Seek(f64),
    SeekRelative(f64),
    SetSpeed(f64),
    SetVolume(f32),
    ClosePlayback,
    /// Override the format of the playing video (`None` = detected).
    SetFormat(Option<VideoFormat>),
    SaveView,
    ResetView,
    AddKeyframe,
    ClearKeyframes,
    AddBookmark,
    SelectAudio(usize),
    SelectSubtitleStream(Option<usize>),
    SelectSubtitleFile(Option<usize>),
    SetFavorite(MediaId, bool),
    SetRating(MediaId, u8),
    SetUserFormat(MediaId, Option<VideoFormat>),
    RemoveMedia(MediaId),
    Rescan {
        force: bool,
    },
    RetryFailed,
    ClearHistory,
    AddFolder(PathBuf),
    RemoveFolder(usize),
    AddSource(SourceConfig),
    RemoveSource(String),
    Browse {
        source: String,
        location: Option<String>,
    },
    /// Index every video in a source folder into the library.
    ImportFolder {
        source: String,
        location: Option<String>,
    },
    DiscoverDlna,
    AddDevice(HapticDeviceConfig),
    RemoveDevice(usize),
    ReconnectDevices,
    RegenerateToken,
    CheckUpdates,
    InstallUpdate,
    Recenter,
    TogglePassthrough,
    /// Show the library/browser panel (during playback).
    ShowBrowser(bool),
    /// Hide the player controls (the trigger on empty space brings them
    /// back).
    HideControls,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Home,
    Library,
    Sources,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsTab {
    Playback,
    Library,
    Haptics,
    Remote,
    Updates,
    About,
}

/// Format filter chips in the library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatFilter {
    All,
    Flat,
    Vr180,
    Vr360,
    Fisheye,
}

/// State of a source listing.
#[derive(Default)]
pub struct Browse {
    pub source: String,
    pub location: Option<String>,
    /// Parent locations, for the back button.
    pub stack: Vec<Option<String>>,
    pub entries: Vec<Entry>,
    pub loading: bool,
    pub error: Option<String>,
}

/// Add-source form.
pub struct SourceForm {
    pub kind: fp_sources::SourceKind,
    pub name: String,
    pub address: String,
    pub username: String,
    pub password: String,
    pub insecure_tls: bool,
}

impl Default for SourceForm {
    fn default() -> Self {
        SourceForm {
            kind: fp_sources::SourceKind::DeoVr,
            name: String::new(),
            address: String::new(),
            username: String::new(),
            password: String::new(),
            insecure_tls: false,
        }
    }
}

/// Status of the update check, shown in settings.
#[derive(Default, Clone, Debug)]
pub enum UpdateStatus {
    #[default]
    Unknown,
    Checking,
    UpToDate,
    Available {
        version: String,
        notes: String,
    },
    Downloading {
        done: u64,
        total: u64,
    },
    Installed {
        version: String,
    },
    Failed(String),
}

/// View-local UI state.
pub struct UiState {
    pub screen: Screen,
    pub settings_tab: SettingsTab,
    pub search: String,
    pub sort: fp_library::Sort,
    pub favorites_only: bool,
    pub format_filter: FormatFilter,
    pub stereo_only: bool,
    pub page_size: usize,
    /// Library results; `None` when they must be re-queried.
    pub results: Option<Vec<fp_library::MediaRecord>>,
    pub total: usize,
    pub home: Option<HomeRows>,
    pub details: Option<MediaId>,
    pub browse: Option<Browse>,
    pub source_form: Option<SourceForm>,
    pub dlna_found: Vec<fp_sources::DlnaDevice>,
    pub dlna_searching: bool,
    pub new_folder: String,
    pub new_device: String,
    pub new_device_kind: usize,
    pub scan_status: Option<String>,
    pub worker_status: Option<String>,
    pub update: UpdateStatus,
    pub adjust_open: bool,
    pub adjust_tab: usize,
    pub open_error: Option<String>,
    pub opening: Option<String>,
    pub toast: Option<(String, std::time::Instant)>,
    /// Hide the seek-preview while scrubbing has not moved.
    pub scrub: Option<f64>,
    /// The volume before muting from the control bar.
    pub unmute_volume: Option<f32>,
    pub keyboard_shift: bool,
    /// Facts for the About tab (runtime, GPU, decoder...).
    pub about: Vec<(String, String)>,
}

pub struct HomeRows {
    pub continue_watching: Vec<fp_library::MediaRecord>,
    pub recent: Vec<fp_library::MediaRecord>,
    pub favorites: Vec<fp_library::MediaRecord>,
}

impl Default for UiState {
    fn default() -> Self {
        UiState {
            screen: Screen::Home,
            settings_tab: SettingsTab::Playback,
            search: String::new(),
            sort: fp_library::Sort::Added,
            favorites_only: false,
            format_filter: FormatFilter::All,
            stereo_only: false,
            page_size: 48,
            results: None,
            total: 0,
            home: None,
            details: None,
            browse: None,
            source_form: None,
            dlna_found: Vec::new(),
            dlna_searching: false,
            new_folder: String::new(),
            new_device: String::new(),
            new_device_kind: 0,
            scan_status: None,
            worker_status: None,
            update: UpdateStatus::Unknown,
            adjust_open: false,
            adjust_tab: 0,
            open_error: None,
            opening: None,
            toast: None,
            scrub: None,
            unmute_volume: None,
            keyboard_shift: false,
            about: Vec::new(),
        }
    }
}

impl UiState {
    /// Library changed: re-query on the next paint.
    pub fn invalidate(&mut self) {
        self.results = None;
        self.home = None;
    }

    pub fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), std::time::Instant::now()));
    }

    pub fn query(&self) -> Query {
        use fp_library::ProjectionKind as P;
        let projections = match self.format_filter {
            FormatFilter::All => vec![],
            FormatFilter::Flat => vec![P::Flat],
            FormatFilter::Vr180 => vec![P::Equirect180],
            FormatFilter::Vr360 => vec![P::Equirect360, P::Eac],
            FormatFilter::Fisheye => vec![P::Fisheye],
        };
        Query {
            text: self.search.trim().to_string(),
            favorites_only: self.favorites_only,
            projections,
            stereo: self.stereo_only.then_some(fp_library::StereoFilter::Stereo),
            sort: if self.search.trim().is_empty() {
                self.sort
            } else {
                fp_library::Sort::Relevance
            },
            limit: Some(self.page_size),
            ..Default::default()
        }
    }
}

/// Everything a panel's UI may read or change.
pub struct View<'a> {
    pub state: &'a mut UiState,
    pub settings: &'a mut crate::settings::Settings,
    pub services: &'a crate::services::Services,
    pub thumbs: &'a mut thumbs::Thumbs,
    pub playback: Option<&'a mut crate::playback::Playback>,
    pub actions: &'a mut Vec<Action>,
    pub passthrough_available: bool,
    pub devices: &'a [fp_haptics::DeviceStatus],
}

/// A big square-ish button used in the navigation and transport rows.
pub fn big_button(ui: &mut egui::Ui, text: &str, selected: bool) -> egui::Response {
    let b = egui::Button::new(egui::RichText::new(text).size(21.0))
        .selected(selected)
        .min_size(egui::vec2(56.0, 48.0));
    ui.add(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(fmt_time(0.0), "0:00");
        assert_eq!(fmt_time(65.4), "1:05");
        assert_eq!(fmt_time(3600.0 + 61.0), "1:01:01");
        assert_eq!(fmt_time(f64::NAN), "0:00");
        assert_eq!(fmt_size(1_500_000_000), "1.5 GB");
    }
}
