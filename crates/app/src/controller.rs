//! The application brain, kept free of I/O so it can be tested exhaustively.
//!
//! Inputs: UI actions ([`UiAction`]), remote commands ([`RemoteCommand`]),
//! controller shortcuts ([`InputAction`]), player status/events and service
//! results. Output: a list of [`Effect`]s the glue layer (`app.rs`) carries
//! out against the player, the services runtime and the XR session.
//!
//! Policies that live here:
//! * view settings for the open video: per-file user override > container
//!   signalling (st3d/sv3d) > library detection (DeoVR feed) > filename
//!   tokens > flat mono ([`resolve_view_settings`], built on
//!   [`fp_core::detect::merge`]);
//! * resume points: where to start, when to save, when a video counts as
//!   watched ([`resume_start`], [`is_near_end`]);
//! * per-file override persistence: picture/projection edits mark the
//!   session dirty and are saved after a short debounce and on close.

use crate::config::Config;
use crate::input_map::InputAction;
use crate::state::{AppState, Nav, Screen};
use fp_core::detect::{self, Detected};
use fp_core::projection::CorrectionKeyframe;
use fp_core::{Corrections, MediaInfo, MediaTime, Projection, StereoMode, ViewSettings};
use fp_library::{DetectSource, Item, ItemQuery, Sort, SpriteInfo};
use fp_remote::RemoteCommand;
use fp_sources::SourceConfig;
use fp_ui::screens::{HapticsDevice, SettingsModel, SortKey};
use fp_ui::{ToastKind, UiAction};
use fp_video::{PlaybackState, PlayerEvent, PlayerStatus, SeekMode};
use std::collections::HashMap;

/// Saved positions closer than this to the start are not worth resuming.
pub const MIN_RESUME: MediaTime = MediaTime(10_000_000);
/// How often the resume point is saved while playing (seconds).
pub const RESUME_SAVE_INTERVAL_S: f64 = 15.0;
/// User view edits are persisted after this long without further edits.
pub const VIEW_SAVE_DEBOUNCE_S: f64 = 2.0;
/// Speed ladder used by controller shortcuts.
pub const SPEEDS: [f64; 9] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];
/// Image-cache key spaces (`TextureId::Image`).
pub const THUMB_KEY_BASE: u64 = 1 << 60;
pub const SPRITE_KEY_BASE: u64 = 2 << 60;
pub const SUBTITLE_KEY_BASE: u64 = 3 << 60;
/// Pseudo-source id of the "Continue watching" shelf (items with a resume point).
pub const CONTINUE_SOURCE: &str = "__continue__";

// ------------------------------------------------------------------ policy

/// What the view-settings resolver knows about a video.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ViewInputs {
    /// Persisted per-file override (wins outright).
    pub user_override: Option<ViewSettings>,
    /// Library detection for the item and where it came from.
    pub library: Option<(Projection, StereoMode, bool, DetectSource)>,
    /// Container signalling from the demuxer (None until the file is open).
    pub container: (Option<Projection>, Option<StereoMode>),
    /// File name or URI for filename-token detection.
    pub name: String,
}

/// Resolve the view settings to play with. Precedence: user override >
/// container metadata > library detection (feed / earlier probe) >
/// filename tokens > flat mono.
pub fn resolve_view_settings(inp: &ViewInputs) -> ViewSettings {
    if let Some(vs) = &inp.user_override {
        return vs.clone();
    }
    if let Some((p, s, swap, DetectSource::User)) = &inp.library {
        return ViewSettings {
            projection: p.clone(),
            stereo: *s,
            swap_eyes: *swap,
            ..Default::default()
        };
    }
    let mut guess: Detected = detect::from_filename(&inp.name);
    if let Some((p, s, swap, src)) = &inp.library {
        if matches!(src, DetectSource::Feed | DetectSource::Container) {
            guess = Detected {
                projection: Some(p.clone()),
                stereo: Some(*s),
                swap_eyes: *swap,
            };
        }
    }
    let (projection, stereo, swap_eyes) = detect::merge(None, inp.container.clone(), &guess);
    ViewSettings {
        projection,
        stereo,
        swap_eyes,
        ..Default::default()
    }
}

/// Within this much of the end a video counts as finished (the same rule
/// `fp_library::Library::set_resume` applies, so the two never disagree).
pub const FINISHED_MARGIN: MediaTime = MediaTime(30_000_000);

/// True when `pos` is close enough to the end to count as finished.
pub fn is_near_end(pos: MediaTime, duration: Option<MediaTime>) -> bool {
    match duration {
        Some(d) if d > MediaTime::ZERO => pos.0 >= d.0 - FINISHED_MARGIN.0,
        _ => false,
    }
}

/// Where to start a video given a saved resume point.
pub fn resume_start(
    enabled: bool,
    saved: Option<MediaTime>,
    duration: Option<MediaTime>,
) -> Option<MediaTime> {
    let pos = saved.filter(|_| enabled)?;
    (pos >= MIN_RESUME && !is_near_end(pos, duration)).then_some(pos)
}

/// Next speed on the ladder in direction `dir` (±1).
pub fn step_speed(current: f64, dir: i32) -> f64 {
    let idx = SPEEDS
        .iter()
        .enumerate()
        .min_by(|a, b| (a.1 - current).abs().total_cmp(&(b.1 - current).abs()))
        .map(|(i, _)| i as i32)
        .unwrap_or(3);
    SPEEDS[(idx + dir).clamp(0, SPEEDS.len() as i32 - 1) as usize]
}

/// Library sort for a UI sort key (`None` = sort client-side).
pub fn sort_for(key: SortKey, searching: bool) -> Option<Sort> {
    if searching {
        return Some(Sort::Relevance);
    }
    match key {
        SortKey::Title => Some(Sort::Name),
        SortKey::Added => Some(Sort::DateAdded),
        SortKey::Duration => Some(Sort::Duration),
        SortKey::LastWatched => Some(Sort::LastWatched),
        SortKey::Resolution => None,
    }
}

/// Stable u64 id for a source id string (FNV-1a), as the UI wants numbers.
pub fn source_key(id: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// File name (without directories / query) of a URI or path, for titles.
pub fn title_from_uri(uri: &str) -> String {
    let no_query = uri.split(['?', '#']).next().unwrap_or(uri);
    let file = no_query
        .trim_end_matches('/')
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(no_query);
    let decoded = percent_decode(file);
    match decoded.rfind('.') {
        Some(i) if i > 0 => decoded[..i].to_string(),
        _ => decoded,
    }
}

/// Decode `%XX` escapes (invalid escapes are kept literally).
pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && s.is_char_boundary(i + 3) {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ----------------------------------------------------------------- effects

#[derive(Debug, Clone, PartialEq)]
pub enum OpenTarget {
    Item(i64),
    Uri(String),
}

/// Player commands (a cloneable mirror of the parts of
/// [`fp_video::PlayerCommand`] the controller issues).
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerOp {
    Play,
    Pause,
    TogglePause,
    Seek(MediaTime, SeekMode),
    SetSpeed(f64),
    Step(i32),
    SetLoop(Option<(MediaTime, MediaTime)>),
    SelectAudio(Option<u32>),
    SelectSubtitle(Option<u32>),
    SetAvOffset(MediaTime),
    NextChapter,
    PrevChapter,
    Stop,
}

/// Where a view override is stored.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewTarget {
    pub item_id: Option<i64>,
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ThumbSource {
    File(String),
    Url(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LibraryOp {
    Query {
        serial: u64,
        query: ItemQuery,
    },
    /// Items with a resume point, most recent first.
    ContinueWatching {
        serial: u64,
    },
    /// Rescan one source (`None` = all).
    Refresh(Option<String>),
    AddDefaultSources,
    SetFavourite(i64, bool),
    SaveResume(i64, MediaTime),
    ClearResume(i64),
    RecordWatch {
        item: i64,
        position: Option<MediaTime>,
        completed: bool,
    },
    SaveView {
        target: ViewTarget,
        settings: ViewSettings,
    },
    LoadThumbnails(Vec<(i64, ThumbSource)>),
    LoadSprite {
        item: i64,
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum HapticsOp {
    ClearScripts,
    SetScriptEnabled(bool),
    Scan,
}

/// Something for the glue layer to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Player(PlayerOp),
    /// Resolve and open media (services), then hand it to the player.
    Open {
        token: u64,
        target: OpenTarget,
        start: Option<MediaTime>,
    },
    Library(LibraryOp),
    Haptics(HapticsOp),
    /// Persist the config and restart whatever the change affects.
    ConfigChanged {
        old: Box<Config>,
        new: Box<Config>,
    },
    Recenter,
    RequestRefreshRate(f32),
    Toast(String, ToastKind),
    /// Replace the library screen's search field text (remote keyboard).
    SetSearchText(String),
    CheckUpdates,
    InstallUpdate,
}

// ------------------------------------------------------------------- state

#[derive(Debug, Clone, PartialEq)]
pub enum ThumbState {
    Requested,
    Ready(u64),
    Failed,
}

/// Library browsing state (the query the user built plus its results).
#[derive(Debug, Clone, Default)]
pub struct LibraryState {
    pub sources: Vec<SourceConfig>,
    pub online: HashMap<String, bool>,
    pub counts: HashMap<String, usize>,
    pub selected_source: Option<String>,
    pub search: String,
    pub sort: SortKey,
    pub descending: bool,
    pub favourites_only: bool,
    pub active_tags: Vec<String>,
    pub items: Vec<Item>,
    pub all_tags: Vec<String>,
    pub loading: bool,
    pub status: Option<String>,
    pub serial: u64,
    pub thumbs: HashMap<i64, ThumbState>,
}

impl LibraryState {
    pub fn query(&self) -> ItemQuery {
        let searching = !self.search.trim().is_empty();
        ItemQuery {
            text: searching.then(|| self.search.trim().to_string()),
            source_id: self.selected_source.clone(),
            tags: self.active_tags.clone(),
            favourite: self.favourites_only.then_some(true),
            sort: sort_for(self.sort, searching).unwrap_or(Sort::Name),
            descending: self.descending,
            limit: Some(2000),
            ..Default::default()
        }
    }

    /// Apply results; client-side sorting for keys the DB can't sort by.
    pub fn set_items(&mut self, mut items: Vec<Item>, tags: Vec<String>) {
        if sort_for(self.sort, !self.search.trim().is_empty()).is_none() {
            items.sort_by_key(|i| i.height.unwrap_or(0).max(i.width.unwrap_or(0)));
            if self.descending {
                items.reverse();
            }
        }
        self.items = items;
        self.all_tags = tags;
        self.loading = false;
        self.status = if self.items.is_empty() {
            Some(if self.sources.is_empty() {
                "No sources yet. Use + to add your video folders.".into()
            } else if self.search.trim().is_empty() {
                "Nothing here yet.".into()
            } else {
                format!("No results for \"{}\"", self.search.trim())
            })
        } else {
            None
        };
    }

    pub fn source_by_key(&self, key: u64) -> Option<&SourceConfig> {
        self.sources.iter().find(|s| source_key(&s.id) == key)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScriptInfo {
    pub name: String,
    pub heat: Vec<f32>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpriteState {
    pub info: SpriteInfo,
    pub key: u64,
    pub sheet: (u32, u32),
}

/// The open video.
#[derive(Debug, Clone)]
pub struct Session {
    pub token: u64,
    pub uri: String,
    pub title: String,
    pub item: Option<Item>,
    pub inputs: ViewInputs,
    pub view: ViewSettings,
    /// The user edited the view this session (persist on close).
    pub view_edited: bool,
    view_dirty_since: Option<f64>,
    pub media: Option<MediaInfo>,
    pub loop_a: Option<MediaTime>,
    pub loop_b: Option<MediaTime>,
    pub script: Option<ScriptInfo>,
    pub preview_time: Option<MediaTime>,
    pub sprite: Option<SpriteState>,
    /// Waiting for services to resolve the media.
    pub resolving: bool,
    last_resume_save: f64,
    ended: bool,
}

impl Session {
    pub fn target(&self) -> ViewTarget {
        ViewTarget {
            item_id: self.item.as_ref().map(|i| i.id),
            uri: self.uri.clone(),
        }
    }
    pub fn duration(&self) -> Option<MediaTime> {
        self.media
            .as_ref()
            .and_then(|m| m.duration)
            .or_else(|| self.item.as_ref().and_then(|i| i.duration))
    }
}

/// Runtime status shown in Settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuntimeStatus {
    pub pairing_url: Option<String>,
    pub refresh_rates: Vec<f32>,
    pub haptics_devices: Vec<HapticsDevice>,
    pub haptics_scanning: bool,
    pub haptics_device: Option<String>,
    pub update_available: Option<String>,
    pub update_progress: Option<f32>,
    pub update_status: Option<String>,
    pub version: String,
}

/// Media resolved by the services, minus the byte stream itself.
#[derive(Debug, Clone, Default)]
pub struct OpenedMeta {
    pub token: u64,
    pub uri: String,
    pub item: Option<Item>,
    pub user_override: Option<ViewSettings>,
    pub explicit_start: Option<MediaTime>,
}

/// How to hand resolved media to the player.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenDecision {
    pub name: String,
    pub start: Option<MediaTime>,
    pub effects: Vec<Effect>,
}

pub struct Controller {
    pub config: Config,
    pub nav: AppState,
    pub library: LibraryState,
    pub session: Option<Session>,
    pub runtime: RuntimeStatus,
    pub player: PlayerStatus,
    next_token: u64,
    idle_s: f32,
    now: f64,
}

impl Controller {
    pub fn new(config: Config) -> Controller {
        let library = LibraryState {
            loading: true,
            ..Default::default()
        };
        Controller {
            config,
            nav: AppState::default(),
            library,
            session: None,
            runtime: RuntimeStatus {
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
            player: PlayerStatus::default(),
            next_token: 1,
            idle_s: 0.0,
            now: 0.0,
        }
    }

    pub fn screen(&self) -> Screen {
        self.nav.screen()
    }

    // ---------------------------------------------------------- library

    /// Build the query effect for the current library state.
    pub fn requery(&mut self) -> Effect {
        self.library.serial += 1;
        self.library.loading = true;
        if self.library.selected_source.as_deref() == Some(CONTINUE_SOURCE) {
            return Effect::Library(LibraryOp::ContinueWatching {
                serial: self.library.serial,
            });
        }
        Effect::Library(LibraryOp::Query {
            serial: self.library.serial,
            query: self.library.query(),
        })
    }

    pub fn on_library_items(&mut self, serial: u64, items: Vec<Item>, tags: Vec<String>) {
        if serial != self.library.serial {
            return;
        }
        self.library.set_items(items, tags);
    }

    pub fn on_sources(&mut self, sources: Vec<SourceConfig>) -> Vec<Effect> {
        let changed = sources != self.library.sources;
        self.library.sources = sources;
        if let Some(sel) = &self.library.selected_source {
            if sel != CONTINUE_SOURCE && !self.library.sources.iter().any(|s| &s.id == sel) {
                self.library.selected_source = None;
            }
        }
        if changed {
            vec![self.requery()]
        } else {
            Vec::new()
        }
    }

    pub fn on_thumbnail(&mut self, item: i64, ok: bool) {
        self.library.thumbs.insert(
            item,
            if ok {
                ThumbState::Ready(THUMB_KEY_BASE | item as u64)
            } else {
                ThumbState::Failed
            },
        );
    }

    /// The renderer evicted a thumbnail; request it again when visible.
    pub fn forget_thumbnail(&mut self, item: i64) {
        self.library.thumbs.remove(&item);
    }

    fn load_thumbnails(&mut self, range: std::ops::Range<usize>) -> Vec<Effect> {
        let mut jobs = Vec::new();
        let end = range.end.min(self.library.items.len());
        for it in &self.library.items[range.start.min(end)..end] {
            if self.library.thumbs.contains_key(&it.id) {
                continue;
            }
            let src = match (&it.thumbnail_path, &it.remote_thumbnail) {
                (Some(p), _) => ThumbSource::File(p.clone()),
                (None, Some(u)) => ThumbSource::Url(u.clone()),
                _ => continue,
            };
            jobs.push((it.id, src));
        }
        for (id, _) in &jobs {
            self.library.thumbs.insert(*id, ThumbState::Requested);
        }
        if jobs.is_empty() {
            Vec::new()
        } else {
            vec![Effect::Library(LibraryOp::LoadThumbnails(jobs))]
        }
    }

    // ------------------------------------------------------------ open

    /// Start opening `target`; the player screen shows while services resolve it.
    pub fn open(&mut self, target: OpenTarget, start: Option<MediaTime>) -> Vec<Effect> {
        let mut fx = self.close_session(false);
        let token = self.next_token;
        self.next_token += 1;
        let (uri, title, item) = match &target {
            OpenTarget::Item(id) => {
                let item = self.library.items.iter().find(|i| i.id == *id).cloned();
                let uri = item.as_ref().map(|i| i.uri.clone()).unwrap_or_default();
                let title = item
                    .as_ref()
                    .map(|i| i.title.clone())
                    .unwrap_or_else(|| "Loading…".into());
                (uri, title, item)
            }
            OpenTarget::Uri(u) => (u.clone(), title_from_uri(u), None),
        };
        self.session = Some(Session {
            token,
            uri: uri.clone(),
            title,
            inputs: ViewInputs {
                name: uri,
                ..Default::default()
            },
            item,
            view: ViewSettings::default(),
            view_edited: false,
            view_dirty_since: None,
            media: None,
            loop_a: None,
            loop_b: None,
            script: None,
            preview_time: None,
            sprite: None,
            resolving: true,
            last_resume_save: self.now,
            ended: false,
        });
        self.player = PlayerStatus {
            state: PlaybackState::Opening,
            ..Default::default()
        };
        self.nav.handle(Nav::OpenItem);
        self.idle_s = 0.0;
        fx.push(Effect::Open {
            token,
            target,
            start,
        });
        fx
    }

    /// Services resolved the media. Returns `None` if the request is stale
    /// (the caller then drops the input).
    pub fn on_media_opened(&mut self, meta: OpenedMeta) -> Option<OpenDecision> {
        let resume_enabled = self.config.playback.resume_playback;
        let default_speed = self.config.playback.default_speed as f64;
        let av_offset = self.config.playback.av_offset_ms;
        let s = self.session.as_mut().filter(|s| s.token == meta.token)?;
        s.resolving = false;
        s.uri = meta.uri.clone();
        if let Some(item) = &meta.item {
            s.title = item.title.clone();
        }
        s.item = meta.item.clone();
        s.inputs = ViewInputs {
            user_override: meta.user_override.clone(),
            library: meta.item.as_ref().and_then(|i| {
                i.projection.clone().map(|p| {
                    (
                        p,
                        i.stereo.unwrap_or_default(),
                        i.swap_eyes,
                        i.detect_source,
                    )
                })
            }),
            container: (None, None),
            name: meta
                .item
                .as_ref()
                .map(|i| i.path.clone())
                .unwrap_or_else(|| meta.uri.clone()),
        };
        s.view = resolve_view_settings(&s.inputs);
        let start = meta.explicit_start.or_else(|| {
            resume_start(
                resume_enabled,
                meta.item.as_ref().and_then(|i| i.resume),
                meta.item.as_ref().and_then(|i| i.duration),
            )
        });
        let mut effects = Vec::new();
        if (default_speed - 1.0).abs() > 1e-3 {
            effects.push(Effect::Player(PlayerOp::SetSpeed(default_speed)));
        }
        if av_offset != 0 {
            effects.push(Effect::Player(PlayerOp::SetAvOffset(
                MediaTime::from_millis(av_offset as i64),
            )));
        }
        if let Some(item) = &meta.item {
            if let (Some(path), Some(_)) = (&item.sprite_path, &item.thumbnail_path) {
                effects.push(Effect::Library(LibraryOp::LoadSprite {
                    item: item.id,
                    path: path.clone(),
                }));
            }
        }
        if let Some(t) = start.filter(|t| *t > MediaTime::ZERO) {
            effects.push(Effect::Toast(
                format!("Resuming at {}", fp_ui::widgets::timeline::format_time(t)),
                ToastKind::Info,
            ));
        }
        Some(OpenDecision {
            name: s.title.clone(),
            start,
            effects,
        })
    }

    pub fn on_open_failed(&mut self, token: u64, error: &str) -> Vec<Effect> {
        if self.session.as_ref().is_none_or(|s| s.token != token) {
            return Vec::new();
        }
        self.session = None;
        self.player = PlayerStatus::default();
        self.nav.handle(Nav::PlaybackEnded);
        vec![Effect::Toast(
            format!("Could not open: {error}"),
            ToastKind::Error,
        )]
    }

    pub fn on_scripts_loaded(&mut self, token: u64, name: String, heat: Vec<f32>) {
        if let Some(s) = self.session.as_mut().filter(|s| s.token == token) {
            s.script = Some(ScriptInfo {
                name,
                heat,
                enabled: true,
            });
        }
    }

    pub fn on_sprite_loaded(&mut self, item: i64, info: SpriteInfo, sheet: (u32, u32)) {
        if let Some(s) = self
            .session
            .as_mut()
            .filter(|s| s.item.as_ref().is_some_and(|i| i.id == item))
        {
            s.sprite = Some(SpriteState {
                info,
                key: SPRITE_KEY_BASE | item as u64,
                sheet,
            });
        }
    }

    // ---------------------------------------------------------- player

    /// Fresh status snapshot from the player (every frame).
    pub fn on_player_status(&mut self, st: PlayerStatus) -> Vec<Effect> {
        let mut fx = Vec::new();
        let Some(s) = self.session.as_mut() else {
            self.player = st;
            return fx;
        };
        if s.resolving {
            // The player still reports the previous file.
            return fx;
        }
        let opening = s.media.is_none() && st.state == PlaybackState::Idle;
        self.player = st;
        if opening {
            // The engine has not picked up the open request yet.
            self.player.state = PlaybackState::Opening;
        }
        if self.player.state == PlaybackState::Ended && !s.ended && !self.player.loop_file {
            s.ended = true;
            fx.extend(self.close_session(true));
            self.nav.handle(Nav::PlaybackEnded);
        }
        fx
    }

    pub fn on_player_event(&mut self, ev: &PlayerEvent) -> Vec<Effect> {
        let mut fx = Vec::new();
        match ev {
            PlayerEvent::Opened(info) => {
                if let Some(s) = self.session.as_mut().filter(|s| !s.resolving) {
                    s.media = Some((**info).clone());
                    if let Some(v) = info.primary_video() {
                        s.inputs.container = (v.signalled_projection.clone(), v.signalled_stereo);
                        if !s.view_edited {
                            let corrections = s.view.corrections;
                            let keyframes = s.view.keyframes.clone();
                            s.view = resolve_view_settings(&s.inputs);
                            if s.inputs.user_override.is_none() {
                                s.view.corrections = corrections;
                                s.view.keyframes = keyframes;
                            }
                        }
                        let target = self.config.general.refresh_rate_hz;
                        if let Some(hz) = fp_xr::choose_refresh_rate(
                            &self.runtime.refresh_rates,
                            Some(v.fps as f32),
                            if target > 0.0 { target } else { 72.0 },
                            if target > 0.0 { target } else { 144.0 },
                        ) {
                            fx.push(Effect::RequestRefreshRate(hz));
                        }
                    }
                }
            }
            PlayerEvent::Error(e) => {
                if self.session.as_ref().is_some_and(|s| !s.resolving) {
                    fx.push(Effect::Toast(
                        format!("Playback error: {e}"),
                        ToastKind::Error,
                    ));
                    fx.extend(self.close_session(false));
                    self.nav.handle(Nav::PlaybackEnded);
                }
            }
            PlayerEvent::Warning(w) => fx.push(Effect::Toast(w.clone(), ToastKind::Warning)),
            PlayerEvent::DecoderSelected { warnings, .. } => {
                for w in warnings {
                    fx.push(Effect::Toast(w.clone(), ToastKind::Warning));
                }
            }
            _ => {}
        }
        fx
    }

    /// Resume / history bookkeeping when a session ends.
    fn finish_session(&mut self, completed_naturally: bool) -> Vec<Effect> {
        let mut fx = Vec::new();
        let Some(s) = self.session.as_mut() else {
            return fx;
        };
        if s.view_edited || s.view_dirty_since.is_some() {
            fx.push(Effect::Library(LibraryOp::SaveView {
                target: s.target(),
                settings: s.view.clone(),
            }));
            s.view_dirty_since = None;
        }
        if let Some(item) = &s.item {
            let pos = self.player.position;
            let done = completed_naturally || is_near_end(pos, s.duration());
            if done {
                fx.push(Effect::Library(LibraryOp::ClearResume(item.id)));
            } else if pos >= MIN_RESUME {
                fx.push(Effect::Library(LibraryOp::SaveResume(item.id, pos)));
            }
            if done || pos > MediaTime::ZERO {
                fx.push(Effect::Library(LibraryOp::RecordWatch {
                    item: item.id,
                    position: (!done).then_some(pos),
                    completed: done,
                }));
            }
        }
        fx.push(Effect::Haptics(HapticsOp::ClearScripts));
        fx
    }

    /// Stop the player and forget the session (if any).
    fn close_session(&mut self, natural_end: bool) -> Vec<Effect> {
        if self.session.is_none() {
            return Vec::new();
        }
        let mut fx = self.finish_session(natural_end);
        fx.push(Effect::Player(PlayerOp::Stop));
        self.session = None;
        if !natural_end {
            // After a natural end the final (Ended) status stays visible.
            self.player = PlayerStatus::default();
        }
        fx
    }

    fn apply_nav(&mut self, nav: Nav) -> Vec<Effect> {
        let had_video = self.nav.playing;
        self.nav.handle(nav);
        if had_video && !self.nav.playing {
            let fx = self.close_session(false);
            if self.library.items.is_empty() || self.library.loading {
                return fx;
            }
            let mut fx = fx;
            // Resume points / watch counts changed: refresh the grid.
            fx.push(self.requery());
            return fx;
        }
        Vec::new()
    }

    fn seek_to(&self, t: MediaTime, mode: SeekMode) -> Effect {
        let d = self.session.as_ref().and_then(|s| s.duration());
        let t = match d {
            Some(d) => t.clamp_to(MediaTime::ZERO, d),
            None => MediaTime(t.0.max(0)),
        };
        Effect::Player(PlayerOp::Seek(t, mode))
    }

    fn seek_relative(&self, secs: f64) -> Effect {
        self.seek_to(
            self.player.position + MediaTime::from_secs_f64(secs),
            SeekMode::Precise,
        )
    }

    fn mark_view_dirty(&mut self) {
        let now = self.now;
        if let Some(s) = self.session.as_mut() {
            s.view_edited = true;
            s.view_dirty_since = Some(now);
        }
    }

    fn edit_view(&mut self, f: impl FnOnce(&mut ViewSettings, MediaTime)) {
        let pos = self.player.position;
        if let Some(s) = self.session.as_mut() {
            f(&mut s.view, pos);
        }
        self.mark_view_dirty();
    }

    // ------------------------------------------------------------ tick

    /// Per-frame housekeeping: auto-hide, debounced saves, resume points.
    pub fn tick(&mut self, now: f64, dt: f32) -> Vec<Effect> {
        self.now = now;
        let mut fx = Vec::new();
        if self.player.state == PlaybackState::Playing {
            self.idle_s += dt;
            self.nav.handle(Nav::Idle(self.idle_s));
        }
        if let Some(s) = self.session.as_mut() {
            if let Some(t) = s.view_dirty_since {
                if now - t >= VIEW_SAVE_DEBOUNCE_S {
                    s.view_dirty_since = None;
                    fx.push(Effect::Library(LibraryOp::SaveView {
                        target: s.target(),
                        settings: s.view.clone(),
                    }));
                }
            }
            if let Some(item) = &s.item {
                if self.player.state == PlaybackState::Playing
                    && now - s.last_resume_save >= RESUME_SAVE_INTERVAL_S
                {
                    s.last_resume_save = now;
                    if self.player.position >= MIN_RESUME
                        && !is_near_end(self.player.position, s.duration())
                    {
                        fx.push(Effect::Library(LibraryOp::SaveResume(
                            item.id,
                            self.player.position,
                        )));
                    }
                }
            }
        }
        fx
    }

    /// Any user interaction (resets the controls auto-hide timer).
    pub fn activity(&mut self) {
        self.idle_s = 0.0;
        self.nav.handle(Nav::Activity);
    }

    // ---------------------------------------------------------- inputs

    pub fn handle_ui(&mut self, action: UiAction) -> Vec<Effect> {
        let mut fx = Vec::new();
        match action {
            UiAction::Back => fx.extend(self.apply_nav(Nav::Back)),
            UiAction::OpenSettings => fx.extend(self.apply_nav(Nav::OpenSettings)),
            UiAction::OpenLibrary => {
                fx.extend(self.close_session(false));
                self.nav.handle(Nav::PlaybackEnded);
                for _ in 0..16 {
                    if self.nav.screen() == Screen::Library {
                        break;
                    }
                    self.nav.handle(Nav::Back);
                }
                fx.push(self.requery());
            }
            UiAction::OpenPictureAdjust => fx.extend(self.apply_nav(Nav::OpenPictureAdjust)),
            UiAction::ClosePictureAdjust => fx.extend(self.apply_nav(Nav::Back)),

            UiAction::SelectSource(key) => {
                let id = if key == source_key(CONTINUE_SOURCE) {
                    Some(CONTINUE_SOURCE.to_string())
                } else {
                    self.library.source_by_key(key).map(|s| s.id.clone())
                };
                self.library.selected_source = if self.library.selected_source == id {
                    None
                } else {
                    id
                };
                fx.push(self.requery());
            }
            UiAction::AddSource => {
                fx.push(Effect::Library(LibraryOp::AddDefaultSources));
                fx.push(Effect::Toast(
                    "Added the standard video folders. Network sources go in config.toml (general.library_roots).".into(),
                    ToastKind::Info,
                ));
            }
            UiAction::RefreshSource(key) => {
                let id = self.library.source_by_key(key).map(|s| s.id.clone());
                fx.push(Effect::Library(LibraryOp::Refresh(id)));
            }
            UiAction::Search(text) => {
                self.library.search = text;
                fx.push(self.requery());
            }
            UiAction::SetSort { key, descending } => {
                self.library.sort = key;
                self.library.descending = descending;
                fx.push(self.requery());
            }
            UiAction::ToggleTag(tag) => {
                if let Some(i) = self.library.active_tags.iter().position(|t| *t == tag) {
                    self.library.active_tags.remove(i);
                } else {
                    self.library.active_tags.push(tag);
                }
                fx.push(self.requery());
            }
            UiAction::ClearTags => {
                self.library.active_tags.clear();
                fx.push(self.requery());
            }
            UiAction::SetFavouritesOnly(on) => {
                self.library.favourites_only = on;
                fx.push(self.requery());
            }
            UiAction::OpenItem(id) => fx.extend(self.open(OpenTarget::Item(id as i64), None)),
            UiAction::ToggleFavourite(id) => {
                let id = id as i64;
                if let Some(it) = self.library.items.iter_mut().find(|i| i.id == id) {
                    it.favourite = !it.favourite;
                    fx.push(Effect::Library(LibraryOp::SetFavourite(id, it.favourite)));
                }
            }
            UiAction::VisibleItems(range) => fx.extend(self.load_thumbnails(range)),

            UiAction::TogglePlay => fx.push(Effect::Player(PlayerOp::TogglePause)),
            UiAction::Seek(t) => fx.push(self.seek_to(t, SeekMode::Precise)),
            UiAction::Scrub(t) => fx.push(self.seek_to(t, SeekMode::Keyframe)),
            UiAction::SeekRelative(s) => fx.push(self.seek_relative(s)),
            UiAction::SetSpeed(s) => fx.push(Effect::Player(PlayerOp::SetSpeed(s as f64))),
            UiAction::SetProjection(p) => self.edit_view(|v, _| v.projection = p),
            UiAction::SetStereo(st) => self.edit_view(|v, _| v.stereo = st),
            UiAction::SetSwapEyes(sw) => self.edit_view(|v, _| v.swap_eyes = sw),
            UiAction::SelectSubtitle(t) => fx.push(Effect::Player(PlayerOp::SelectSubtitle(t))),
            UiAction::SelectAudio(t) => fx.push(Effect::Player(PlayerOp::SelectAudio(Some(t)))),
            UiAction::SetLoopA(t) => {
                if let Some(s) = self.session.as_mut() {
                    s.loop_a = Some(t);
                    if s.loop_b.is_some_and(|b| b <= t) {
                        s.loop_b = None;
                    }
                    if let (Some(a), Some(b)) = (s.loop_a, s.loop_b) {
                        fx.push(Effect::Player(PlayerOp::SetLoop(Some((a, b)))));
                    }
                }
            }
            UiAction::SetLoopB(t) => {
                if let Some(s) = self.session.as_mut() {
                    if s.loop_a.is_some_and(|a| a < t) {
                        s.loop_b = Some(t);
                        fx.push(Effect::Player(PlayerOp::SetLoop(Some((
                            s.loop_a.unwrap_or(MediaTime::ZERO),
                            t,
                        )))));
                    }
                }
            }
            UiAction::ClearLoop => {
                if let Some(s) = self.session.as_mut() {
                    s.loop_a = None;
                    s.loop_b = None;
                }
                fx.push(Effect::Player(PlayerOp::SetLoop(None)));
            }
            UiAction::Recenter => fx.push(Effect::Recenter),
            UiAction::RequestPreview(t) => {
                if let Some(s) = self.session.as_mut() {
                    s.preview_time = Some(t);
                }
            }
            UiAction::SetScriptEnabled(on) => {
                if let Some(sc) = self.session.as_mut().and_then(|s| s.script.as_mut()) {
                    sc.enabled = on;
                }
                fx.push(Effect::Haptics(HapticsOp::SetScriptEnabled(on)));
            }

            UiAction::SetCorrections(c) => self.edit_view(|v, pos| set_corrections(v, c, pos)),
            UiAction::AddKeyframe => self.edit_view(|v, pos| {
                let c = v.corrections_at(pos);
                upsert_keyframe(v, pos, c);
            }),
            UiAction::ClearKeyframes => self.edit_view(|v, pos| {
                v.corrections = v.corrections_at(pos);
                v.keyframes.clear();
            }),
            UiAction::ResetCorrections => self.edit_view(|v, _| {
                v.corrections = Corrections::default();
                v.keyframes.clear();
            }),

            UiAction::SettingsChanged(m) => fx.extend(self.apply_settings(&m)),
            UiAction::ScanHapticsDevices => {
                self.runtime.haptics_scanning = true;
                fx.push(Effect::Haptics(HapticsOp::Scan));
            }
            UiAction::ConnectHapticsDevice(id) => {
                let old = self.config.clone();
                self.config.haptics.enabled = true;
                self.config.haptics.backend = id;
                fx.push(self.config_changed(old));
            }
            UiAction::DisconnectHapticsDevice(id) => {
                if self.config.haptics.backend == id {
                    let old = self.config.clone();
                    self.config.haptics.backend.clear();
                    fx.push(self.config_changed(old));
                }
            }
            UiAction::RegenerateApiToken => {
                let old = self.config.clone();
                self.config.remote.api_token = None;
                self.runtime.pairing_url = None;
                fx.push(self.config_changed(old));
            }
            UiAction::CheckForUpdates => {
                self.runtime.update_status = Some("Checking…".into());
                fx.push(Effect::CheckUpdates);
            }
            UiAction::InstallUpdate => {
                self.runtime.update_progress = Some(0.0);
                fx.push(Effect::InstallUpdate);
            }
        }
        fx
    }

    fn config_changed(&self, old: Config) -> Effect {
        Effect::ConfigChanged {
            old: Box::new(old),
            new: Box::new(self.config.clone()),
        }
    }

    /// Fold an edited settings model back into the config.
    pub fn apply_settings(&mut self, m: &SettingsModel) -> Vec<Effect> {
        let old = self.config.clone();
        let c = &mut self.config;
        c.general.refresh_rate_hz = m.general.refresh_rate_hz;
        c.comfort.gaze_dimming = m.general.gaze_dimming;
        c.comfort.passthrough_background = m.general.passthrough_background;
        c.comfort.head_locked_screen = m.general.head_locked_screen;
        c.comfort.lying_down = m.general.lying_down;
        c.comfort.environment_dim = m.general.environment_dim;
        c.playback.default_speed = m.playback.default_speed;
        c.playback.resume_playback = m.playback.resume_playback;
        c.playback.seek_step_s = m.playback.seek_step_s;
        c.playback.subtitle_depth_m = m.playback.subtitle_depth_m;
        c.playback.av_offset_ms = m.playback.av_offset_ms;
        c.remote.api_enabled = m.remote.api_enabled;
        c.remote.deovr_enabled = m.remote.deovr_enabled;
        c.haptics.enabled = m.haptics.enabled;
        c.haptics.offset_ms = m.haptics.offset_ms;
        c.updates.check_on_start = m.updates.check_on_start;
        c.updates.channel = if m.updates.beta_channel {
            "beta".into()
        } else {
            "stable".into()
        };
        if *c == old {
            return Vec::new();
        }
        let mut fx = Vec::new();
        if c.playback.av_offset_ms != old.playback.av_offset_ms && self.session.is_some() {
            fx.push(Effect::Player(PlayerOp::SetAvOffset(
                MediaTime::from_millis(c.playback.av_offset_ms as i64),
            )));
        }
        if c.general.refresh_rate_hz != old.general.refresh_rate_hz
            && c.general.refresh_rate_hz > 0.0
        {
            fx.push(Effect::RequestRefreshRate(c.general.refresh_rate_hz));
        }
        if c.remote.api_enabled && !old.remote.api_enabled {
            self.runtime.pairing_url = None;
        }
        fx.push(self.config_changed(old));
        fx
    }

    pub fn handle_remote(&mut self, cmd: RemoteCommand) -> Vec<Effect> {
        match cmd {
            RemoteCommand::Open { uri, start } => {
                self.open(OpenTarget::Uri(uri), start.map(MediaTime::from_secs_f64))
            }
            RemoteCommand::OpenItem { id } => match id.trim().parse::<i64>() {
                Ok(id) => self.open(OpenTarget::Item(id), None),
                Err(_) => vec![Effect::Toast(
                    format!("Remote asked for unknown item {id}"),
                    ToastKind::Warning,
                )],
            },
            RemoteCommand::Play => self.with_session(Effect::Player(PlayerOp::Play)),
            RemoteCommand::Pause => self.with_session(Effect::Player(PlayerOp::Pause)),
            RemoteCommand::TogglePause => self.with_session(Effect::Player(PlayerOp::TogglePause)),
            RemoteCommand::Stop => {
                let mut fx = self.close_session(false);
                self.nav.handle(Nav::PlaybackEnded);
                fx.push(self.requery());
                fx
            }
            RemoteCommand::Seek { seconds } => self
                .with_session(self.seek_to(MediaTime::from_secs_f64(seconds), SeekMode::Precise)),
            RemoteCommand::SeekRelative { seconds } => {
                self.with_session(self.seek_relative(seconds))
            }
            RemoteCommand::SetSpeed { speed } => {
                self.with_session(Effect::Player(PlayerOp::SetSpeed(speed.clamp(0.25, 4.0))))
            }
            RemoteCommand::Text { text, submit: _ } => {
                self.library.search = text.clone();
                vec![Effect::SetSearchText(text), self.requery()]
            }
        }
    }

    fn with_session(&self, e: Effect) -> Vec<Effect> {
        if self.session.is_some() {
            vec![e]
        } else {
            Vec::new()
        }
    }

    pub fn handle_input(&mut self, a: InputAction) -> Vec<Effect> {
        let in_player = matches!(self.nav.screen(), Screen::Player { .. });
        let mut fx = Vec::new();
        match a {
            InputAction::Activity => self.activity(),
            InputAction::Back => fx.extend(self.apply_nav(Nav::Back)),
            InputAction::ToggleControls => {
                self.idle_s = 0.0;
                fx.extend(self.apply_nav(Nav::ToggleControls));
            }
            InputAction::Recenter => fx.push(Effect::Recenter),
            InputAction::OpenSettings => fx.extend(self.apply_nav(Nav::OpenSettings)),
            InputAction::OpenPictureAdjust => fx.extend(self.apply_nav(Nav::OpenPictureAdjust)),
            InputAction::TogglePlay if self.session.is_some() => {
                fx.push(Effect::Player(PlayerOp::TogglePause));
            }
            InputAction::SeekStep(dir) if self.session.is_some() => {
                fx.push(self.seek_relative(dir as f64 * self.config.playback.seek_step_s as f64));
            }
            InputAction::SpeedStep(dir) if self.session.is_some() => {
                let sp = step_speed(self.player.speed, dir);
                fx.push(Effect::Player(PlayerOp::SetSpeed(sp)));
                fx.push(Effect::Toast(format!("Speed {sp}×"), ToastKind::Info));
            }
            InputAction::FrameStep(n) if self.session.is_some() => {
                fx.push(Effect::Player(PlayerOp::Step(n)));
            }
            InputAction::Chapter(dir) if self.session.is_some() => {
                fx.push(Effect::Player(if dir > 0 {
                    PlayerOp::NextChapter
                } else {
                    PlayerOp::PrevChapter
                }))
            }
            InputAction::AdjustScreen { distance, size } if in_player => {
                let flat = self
                    .session
                    .as_ref()
                    .is_some_and(|s| !s.view.projection.is_immersive());
                if flat {
                    self.edit_view(|v, _| {
                        if let Projection::Flat {
                            width_m,
                            distance_m,
                            ..
                        } = &mut v.projection
                        {
                            *distance_m = (*distance_m + distance).clamp(0.8, 25.0);
                            *width_m = (*width_m + size).clamp(0.5, 40.0);
                        }
                    });
                } else if size != 0.0 {
                    // Immersive: zoom instead of resizing a screen.
                    self.edit_view(|v, pos| {
                        let mut c = v.corrections_at(pos);
                        c.zoom = (c.zoom + size * 0.1).clamp(0.5, 3.0);
                        set_corrections(v, c, pos);
                    });
                }
            }
            _ => {}
        }
        fx
    }

    // ------------------------------------------------- runtime status

    pub fn set_pairing(&mut self, url: Option<String>, token: Option<String>) -> Vec<Effect> {
        self.runtime.pairing_url = url;
        match token {
            Some(t) if self.config.remote.api_token.as_deref() != Some(t.as_str()) => {
                let old = self.config.clone();
                self.config.remote.api_token = Some(t);
                vec![self.config_changed(old)]
            }
            _ => Vec::new(),
        }
    }
}

/// Apply edited corrections: without keyframes they replace the base
/// corrections; with keyframes they edit (or insert) the keyframe at `pos`.
pub fn set_corrections(v: &mut ViewSettings, c: Corrections, pos: MediaTime) {
    if v.keyframes.is_empty() {
        v.corrections = c;
    } else {
        upsert_keyframe(v, pos, c);
    }
}

/// Insert a keyframe at `pos`, replacing one within 250 ms; keeps order.
pub fn upsert_keyframe(v: &mut ViewSettings, pos: MediaTime, c: Corrections) {
    const SNAP: i64 = 250_000;
    if let Some(k) = v
        .keyframes
        .iter_mut()
        .find(|k| (k.at.0 - pos.0).abs() <= SNAP)
    {
        k.corrections = c;
        return;
    }
    let i = v.keyframes.partition_point(|k| k.at < pos);
    v.keyframes.insert(
        i,
        CorrectionKeyframe {
            at: pos,
            corrections: c,
        },
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use fp_core::{FisheyeLens, VideoTrackInfo};

    pub fn item(id: i64, uri: &str) -> Item {
        Item {
            id,
            source_id: Some("local".into()),
            uri: uri.into(),
            path: uri.rsplit('/').next().unwrap_or(uri).into(),
            title: title_from_uri(uri),
            size: Some(1000),
            mtime: None,
            content_hash: Some(format!("hash{id}")),
            duration: Some(MediaTime::from_secs_f64(600.0)),
            width: Some(3840),
            height: Some(1920),
            codec: None,
            fps: Some(30.0),
            hdr: false,
            projection: None,
            projection_kind: None,
            stereo: None,
            swap_eyes: false,
            detect_source: DetectSource::Default,
            thumbnail_path: None,
            sprite_path: None,
            remote_thumbnail: None,
            added_at: 0,
            probed: false,
            rating: None,
            favourite: false,
            resume: None,
            last_watched: None,
            watch_count: 0,
            tags: vec![],
            has_script: false,
        }
    }

    fn controller_with_item(it: Item) -> Controller {
        let mut c = Controller::new(Config::default());
        c.library.items = vec![it];
        c.library.loading = false;
        c
    }

    fn open_item(c: &mut Controller, id: i64) -> u64 {
        let fx = c.open(OpenTarget::Item(id), None);
        let token = fx
            .iter()
            .find_map(|e| match e {
                Effect::Open { token, .. } => Some(*token),
                _ => None,
            })
            .unwrap();
        let it = c.library.items.iter().find(|i| i.id == id).cloned();
        c.on_media_opened(OpenedMeta {
            token,
            uri: it.as_ref().map(|i| i.uri.clone()).unwrap_or_default(),
            item: it,
            ..Default::default()
        })
        .unwrap();
        token
    }

    fn status(state: PlaybackState, pos_s: f64) -> PlayerStatus {
        PlayerStatus {
            state,
            position: MediaTime::from_secs_f64(pos_s),
            duration: Some(MediaTime::from_secs_f64(600.0)),
            ..Default::default()
        }
    }

    #[test]
    fn view_settings_precedence() {
        let user = ViewSettings {
            projection: Projection::FLAT_DEFAULT,
            stereo: StereoMode::Ou,
            ..Default::default()
        };
        let mut inp = ViewInputs {
            user_override: Some(user.clone()),
            library: Some((
                Projection::EQUIRECT_360,
                StereoMode::Mono,
                false,
                DetectSource::Feed,
            )),
            container: (Some(Projection::EQUIRECT_180), Some(StereoMode::Sbs)),
            name: "clip_MKX200_LR.mp4".into(),
        };
        // 1. user override wins outright.
        assert_eq!(resolve_view_settings(&inp), user);
        // 2. container beats library feed and filename.
        inp.user_override = None;
        let v = resolve_view_settings(&inp);
        assert_eq!(
            (v.projection, v.stereo),
            (Projection::EQUIRECT_180, StereoMode::Sbs)
        );
        // 3. feed beats filename.
        inp.container = (None, None);
        let v = resolve_view_settings(&inp);
        assert_eq!(
            (v.projection, v.stereo),
            (Projection::EQUIRECT_360, StereoMode::Mono)
        );
        // 4. filename when the library only has a filename guess / nothing.
        inp.library = Some((
            Projection::FLAT_DEFAULT,
            StereoMode::Mono,
            false,
            DetectSource::Filename,
        ));
        let v = resolve_view_settings(&inp);
        assert_eq!(v.projection, Projection::fisheye(FisheyeLens::Mkx200));
        assert_eq!(v.stereo, StereoMode::Sbs);
        // 5. container stereo only: projection still from filename.
        inp.container = (None, Some(StereoMode::Ou));
        assert_eq!(resolve_view_settings(&inp).stereo, StereoMode::Ou);
        // 6. nothing known: flat mono.
        let v = resolve_view_settings(&ViewInputs {
            name: "holiday.mp4".into(),
            ..Default::default()
        });
        assert_eq!(
            (v.projection, v.stereo),
            (Projection::FLAT_DEFAULT, StereoMode::Mono)
        );
        // 7. a user-set detection without stored override still counts as user.
        let v = resolve_view_settings(&ViewInputs {
            library: Some((
                Projection::EQUIRECT_360,
                StereoMode::Ou,
                true,
                DetectSource::User,
            )),
            container: (Some(Projection::EQUIRECT_180), None),
            name: "x_180_LR.mp4".into(),
            ..Default::default()
        });
        assert_eq!(
            (v.projection, v.stereo, v.swap_eyes),
            (Projection::EQUIRECT_360, StereoMode::Ou, true)
        );
    }

    #[test]
    fn resume_rules() {
        let d = Some(MediaTime::from_secs_f64(600.0));
        let s = |x: f64| Some(MediaTime::from_secs_f64(x));
        assert_eq!(resume_start(true, s(120.0), d), s(120.0));
        assert_eq!(resume_start(false, s(120.0), d), None);
        assert_eq!(resume_start(true, s(5.0), d), None, "too close to start");
        assert_eq!(resume_start(true, s(590.0), d), None, "too close to end");
        assert!(is_near_end(MediaTime::from_secs_f64(590.0), d));
        assert!(!is_near_end(MediaTime::from_secs_f64(500.0), d));
        assert!(!is_near_end(MediaTime::from_secs_f64(500.0), None));
        assert!(is_near_end(MediaTime::from_secs_f64(571.0), d));
        assert!(!is_near_end(MediaTime::from_secs_f64(569.0), d));
        // Short clips never resume.
        assert!(is_near_end(MediaTime::from_secs_f64(9.8), s(10.0)));
    }

    #[test]
    fn speed_ladder() {
        assert_eq!(step_speed(1.0, 1), 1.25);
        assert_eq!(step_speed(1.0, -1), 0.75);
        assert_eq!(step_speed(4.0, 1), 4.0);
        assert_eq!(step_speed(0.25, -1), 0.25);
        assert_eq!(step_speed(1.1, 1), 1.25);
    }

    #[test]
    fn titles_and_keys() {
        assert_eq!(
            title_from_uri("file:///v/My%20Clip_180_LR.mp4"),
            "My Clip_180_LR"
        );
        assert_eq!(title_from_uri("https://h/a/b.mkv?x=1"), "b");
        assert_eq!(title_from_uri("/x/noext"), "noext");
        assert_eq!(source_key("a"), source_key("a"));
        assert_ne!(source_key("a"), source_key("b"));
    }

    #[test]
    fn open_uses_resume_and_starts_player_screen() {
        let mut it = item(1, "file:///v/scene_180_LR.mp4");
        it.resume = Some(MediaTime::from_secs_f64(120.0));
        let mut c = controller_with_item(it);
        let fx = c.open(OpenTarget::Item(1), None);
        assert!(matches!(c.screen(), Screen::Player { .. }));
        let token = match &fx[0] {
            Effect::Open {
                token,
                target,
                start,
            } => {
                assert_eq!(target, &OpenTarget::Item(1));
                assert_eq!(*start, None);
                *token
            }
            other => panic!("unexpected {other:?}"),
        };
        let item = c.library.items[0].clone();
        let d = c
            .on_media_opened(OpenedMeta {
                token,
                uri: item.uri.clone(),
                item: Some(item),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(d.start, Some(MediaTime::from_secs_f64(120.0)));
        let s = c.session.as_ref().unwrap();
        assert_eq!(s.view.projection, Projection::EQUIRECT_180);
        assert_eq!(s.view.stereo, StereoMode::Sbs);
        // Stale token is ignored.
        assert!(c
            .on_media_opened(OpenedMeta {
                token: token + 5,
                ..Default::default()
            })
            .is_none());
    }

    #[test]
    fn explicit_start_beats_resume() {
        let mut it = item(1, "file:///v/a.mp4");
        it.resume = Some(MediaTime::from_secs_f64(120.0));
        let mut c = controller_with_item(it.clone());
        let fx = c.open(OpenTarget::Item(1), Some(MediaTime::from_secs_f64(3.0)));
        let token = match &fx[0] {
            Effect::Open { token, .. } => *token,
            _ => unreachable!(),
        };
        let d = c
            .on_media_opened(OpenedMeta {
                token,
                uri: it.uri.clone(),
                item: Some(it),
                explicit_start: Some(MediaTime::from_secs_f64(3.0)),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(d.start, Some(MediaTime::from_secs_f64(3.0)));
    }

    #[test]
    fn container_signalling_updates_view_unless_user_override() {
        let mut c = controller_with_item(item(1, "file:///v/plain.mp4"));
        open_item(&mut c, 1);
        let info = MediaInfo {
            video: vec![VideoTrackInfo {
                index: 0,
                codec: fp_core::Codec::Hevc,
                width: 3840,
                height: 1920,
                fps: 30.0,
                bit_depth: 8,
                transfer: Default::default(),
                signalled_projection: Some(Projection::EQUIRECT_360),
                signalled_stereo: Some(StereoMode::Ou),
            }],
            ..Default::default()
        };
        c.runtime.refresh_rates = vec![72.0, 90.0, 120.0];
        let fx = c.on_player_event(&PlayerEvent::Opened(Box::new(info.clone())));
        assert!(fx.contains(&Effect::RequestRefreshRate(90.0)), "{fx:?}");
        let v = &c.session.as_ref().unwrap().view;
        assert_eq!(
            (v.projection.clone(), v.stereo),
            (Projection::EQUIRECT_360, StereoMode::Ou)
        );

        // With a user override the container is ignored.
        let mut c = controller_with_item(item(2, "file:///v/plain.mp4"));
        let fx = c.open(OpenTarget::Item(2), None);
        let token = match &fx[0] {
            Effect::Open { token, .. } => *token,
            _ => unreachable!(),
        };
        let over = ViewSettings {
            stereo: StereoMode::Sbs,
            ..Default::default()
        };
        c.on_media_opened(OpenedMeta {
            token,
            uri: "file:///v/plain.mp4".into(),
            item: Some(c.library.items[0].clone()),
            user_override: Some(over.clone()),
            ..Default::default()
        });
        c.on_player_event(&PlayerEvent::Opened(Box::new(info)));
        assert_eq!(c.session.as_ref().unwrap().view, over);
    }

    #[test]
    fn view_edits_are_debounced_and_saved_on_close() {
        let mut c = controller_with_item(item(1, "file:///v/a.mp4"));
        open_item(&mut c, 1);
        c.tick(10.0, 0.01);
        assert!(c.handle_ui(UiAction::SetStereo(StereoMode::Ou)).is_empty());
        assert!(c.tick(11.0, 0.01).is_empty(), "debounce not elapsed");
        let fx = c.tick(12.5, 0.01);
        match &fx[..] {
            [Effect::Library(LibraryOp::SaveView { target, settings })] => {
                assert_eq!(target.item_id, Some(1));
                assert_eq!(settings.stereo, StereoMode::Ou);
            }
            other => panic!("unexpected {other:?}"),
        }
        // Edited sessions save their view again when closed.
        let fx = c.handle_ui(UiAction::OpenLibrary);
        assert!(fx
            .iter()
            .any(|e| matches!(e, Effect::Library(LibraryOp::SaveView { .. }))));
        assert!(fx.contains(&Effect::Player(PlayerOp::Stop)));
        assert!(c.session.is_none());
        assert_eq!(c.screen(), Screen::Library);
    }

    #[test]
    fn keyframe_editing() {
        let mut v = ViewSettings::default();
        let c1 = Corrections {
            zoom: 1.5,
            ..Default::default()
        };
        set_corrections(&mut v, c1, MediaTime::ZERO);
        assert_eq!(v.corrections.zoom, 1.5);
        upsert_keyframe(&mut v, MediaTime::from_millis(5000), c1);
        upsert_keyframe(&mut v, MediaTime::from_millis(1000), Corrections::default());
        assert_eq!(v.keyframes.len(), 2);
        assert!(v.keyframes[0].at < v.keyframes[1].at);
        // Within the snap window edits the existing keyframe.
        let c2 = Corrections {
            zoom: 2.0,
            ..Default::default()
        };
        set_corrections(&mut v, c2, MediaTime::from_millis(5100));
        assert_eq!(v.keyframes.len(), 2);
        assert_eq!(v.keyframes[1].corrections.zoom, 2.0);
    }

    #[test]
    fn back_twice_closes_and_saves_resume() {
        let mut c = controller_with_item(item(1, "file:///v/a.mp4"));
        open_item(&mut c, 1);
        c.on_player_status(status(PlaybackState::Playing, 200.0));
        assert!(
            c.handle_ui(UiAction::Back).is_empty(),
            "first back hides controls"
        );
        let fx = c.handle_ui(UiAction::Back);
        assert!(fx.contains(&Effect::Library(LibraryOp::SaveResume(
            1,
            MediaTime::from_secs_f64(200.0)
        ))));
        assert!(fx.contains(&Effect::Library(LibraryOp::RecordWatch {
            item: 1,
            position: Some(MediaTime::from_secs_f64(200.0)),
            completed: false
        })));
        assert!(fx.contains(&Effect::Player(PlayerOp::Stop)));
        assert!(fx.contains(&Effect::Haptics(HapticsOp::ClearScripts)));
        assert_eq!(c.screen(), Screen::Library);
    }

    #[test]
    fn natural_end_marks_watched() {
        let mut c = controller_with_item(item(1, "file:///v/a.mp4"));
        open_item(&mut c, 1);
        c.on_player_status(status(PlaybackState::Playing, 599.0));
        let fx = c.on_player_status(status(PlaybackState::Ended, 600.0));
        assert!(fx.contains(&Effect::Library(LibraryOp::ClearResume(1))));
        assert!(fx.contains(&Effect::Library(LibraryOp::RecordWatch {
            item: 1,
            position: None,
            completed: true
        })));
        assert_eq!(c.screen(), Screen::Library);
        assert!(!c.nav.playing);
    }

    #[test]
    fn periodic_resume_save() {
        let mut c = controller_with_item(item(1, "file:///v/a.mp4"));
        open_item(&mut c, 1);
        c.on_player_status(status(PlaybackState::Playing, 100.0));
        assert!(c.tick(5.0, 0.01).is_empty());
        let fx = c.tick(16.0, 0.01);
        assert_eq!(
            fx,
            vec![Effect::Library(LibraryOp::SaveResume(
                1,
                MediaTime::from_secs_f64(100.0)
            ))]
        );
        assert!(c.tick(17.0, 0.01).is_empty());
    }

    #[test]
    fn controls_autohide_while_playing() {
        let mut c = controller_with_item(item(1, "file:///v/a.mp4"));
        open_item(&mut c, 1);
        c.on_player_status(status(PlaybackState::Playing, 1.0));
        for i in 0..50 {
            c.tick(i as f64 * 0.1, 0.1);
        }
        assert_eq!(
            c.screen(),
            Screen::Player {
                controls_visible: false
            }
        );
        c.activity();
        assert_eq!(
            c.screen(),
            Screen::Player {
                controls_visible: true
            }
        );
    }

    #[test]
    fn ui_player_actions_map_to_commands() {
        let mut c = controller_with_item(item(1, "file:///v/a.mp4"));
        open_item(&mut c, 1);
        c.on_player_status(status(PlaybackState::Playing, 100.0));
        assert_eq!(
            c.handle_ui(UiAction::TogglePlay),
            vec![Effect::Player(PlayerOp::TogglePause)]
        );
        assert_eq!(
            c.handle_ui(UiAction::SeekRelative(-10.0)),
            vec![Effect::Player(PlayerOp::Seek(
                MediaTime::from_secs_f64(90.0),
                SeekMode::Precise
            ))]
        );
        assert_eq!(
            c.handle_ui(UiAction::Scrub(MediaTime::from_secs_f64(9999.0))),
            vec![Effect::Player(PlayerOp::Seek(
                MediaTime::from_secs_f64(600.0),
                SeekMode::Keyframe
            ))],
            "clamped to duration"
        );
        assert!(c
            .handle_ui(UiAction::SetLoopA(MediaTime::from_secs_f64(10.0)))
            .is_empty());
        assert_eq!(
            c.handle_ui(UiAction::SetLoopB(MediaTime::from_secs_f64(20.0))),
            vec![Effect::Player(PlayerOp::SetLoop(Some((
                MediaTime::from_secs_f64(10.0),
                MediaTime::from_secs_f64(20.0)
            ))))]
        );
        assert_eq!(c.handle_ui(UiAction::Recenter), vec![Effect::Recenter]);
        assert_eq!(
            c.handle_ui(UiAction::SelectAudio(2)),
            vec![Effect::Player(PlayerOp::SelectAudio(Some(2)))]
        );
    }

    #[test]
    fn library_actions_build_queries() {
        let mut c = Controller::new(Config::default());
        c.library.sources = vec![SourceConfig {
            id: "nas".into(),
            name: "NAS".into(),
            kind: fp_sources::SourceKind::Smb,
            uri: "smb://nas/v".into(),
            pinned_host_key: None,
        }];
        let fx = c.handle_ui(UiAction::SelectSource(source_key("nas")));
        match &fx[..] {
            [Effect::Library(LibraryOp::Query { query, serial })] => {
                assert_eq!(query.source_id.as_deref(), Some("nas"));
                assert_eq!(*serial, c.library.serial);
            }
            other => panic!("{other:?}"),
        }
        let fx = c.handle_ui(UiAction::Search("beach".into()));
        let Effect::Library(LibraryOp::Query { query, serial }) = &fx[0] else {
            panic!()
        };
        assert_eq!(query.text.as_deref(), Some("beach"));
        assert_eq!(query.sort, Sort::Relevance);
        // Stale results are ignored, current ones applied.
        c.on_library_items(serial - 1, vec![item(9, "file:///x.mp4")], vec![]);
        assert!(c.library.items.is_empty());
        c.on_library_items(*serial, vec![item(9, "file:///x.mp4")], vec!["vr".into()]);
        assert_eq!(c.library.items.len(), 1);
        c.handle_ui(UiAction::ToggleTag("vr".into()));
        assert_eq!(c.library.query().tags, vec!["vr".to_string()]);
        // The "Continue watching" shelf is a pseudo-source.
        let fx = c.handle_ui(UiAction::SelectSource(source_key(CONTINUE_SOURCE)));
        assert!(matches!(
            fx[0],
            Effect::Library(LibraryOp::ContinueWatching { .. })
        ));
        let fx = c.handle_ui(UiAction::SelectSource(source_key(CONTINUE_SOURCE)));
        assert!(
            matches!(fx[0], Effect::Library(LibraryOp::Query { .. })),
            "toggles off"
        );
        c.handle_ui(UiAction::SetFavouritesOnly(true));
        assert_eq!(c.library.query().favourite, Some(true));
        let fx = c.handle_ui(UiAction::ToggleFavourite(9));
        assert_eq!(fx, vec![Effect::Library(LibraryOp::SetFavourite(9, true))]);
    }

    #[test]
    fn thumbnails_requested_once() {
        let mut it = item(1, "file:///a.mp4");
        it.thumbnail_path = Some("/c/1.jpg".into());
        let mut it2 = item(2, "deovr+https://h/2");
        it2.remote_thumbnail = Some("https://h/2.jpg".into());
        let mut c = Controller::new(Config::default());
        c.library.items = vec![it, it2, item(3, "file:///c.mp4")];
        let fx = c.handle_ui(UiAction::VisibleItems(0..10));
        assert_eq!(
            fx,
            vec![Effect::Library(LibraryOp::LoadThumbnails(vec![
                (1, ThumbSource::File("/c/1.jpg".into())),
                (2, ThumbSource::Url("https://h/2.jpg".into()))
            ]))]
        );
        assert!(c.handle_ui(UiAction::VisibleItems(0..10)).is_empty());
        c.on_thumbnail(1, true);
        assert_eq!(c.library.thumbs[&1], ThumbState::Ready(THUMB_KEY_BASE | 1));
    }

    #[test]
    fn remote_commands() {
        let mut c = controller_with_item(item(4, "file:///v/a.mp4"));
        assert!(
            c.handle_remote(RemoteCommand::Play).is_empty(),
            "nothing open"
        );
        let fx = c.handle_remote(RemoteCommand::OpenItem { id: "4".into() });
        assert!(matches!(
            &fx[0],
            Effect::Open {
                target: OpenTarget::Item(4),
                ..
            }
        ));
        let fx = c.handle_remote(RemoteCommand::Open {
            uri: "https://h/v.mp4".into(),
            start: Some(30.0),
        });
        assert!(fx.iter().any(|e| matches!(e,
            Effect::Open { target: OpenTarget::Uri(u), start: Some(s), .. }
                if u == "https://h/v.mp4" && *s == MediaTime::from_secs_f64(30.0))));
        assert_eq!(
            c.handle_remote(RemoteCommand::Pause),
            vec![Effect::Player(PlayerOp::Pause)]
        );
        assert_eq!(
            c.handle_remote(RemoteCommand::SetSpeed { speed: 9.0 }),
            vec![Effect::Player(PlayerOp::SetSpeed(4.0))]
        );
        assert!(matches!(
            &c.handle_remote(RemoteCommand::OpenItem { id: "x".into() })[0],
            Effect::Toast(_, ToastKind::Warning)
        ));
        let fx = c.handle_remote(RemoteCommand::Text {
            text: "abc".into(),
            submit: true,
        });
        assert_eq!(fx[0], Effect::SetSearchText("abc".into()));
        let fx = c.handle_remote(RemoteCommand::Stop);
        assert!(fx.contains(&Effect::Player(PlayerOp::Stop)));
        assert!(c.session.is_none());
    }

    #[test]
    fn settings_round_trip_into_config() {
        let mut c = Controller::new(Config::default());
        let mut m = crate::view_models::settings_model(&c);
        m.playback.seek_step_s = 30.0;
        m.remote.api_enabled = true;
        m.updates.beta_channel = true;
        let fx = c.handle_ui(UiAction::SettingsChanged(m.clone()));
        assert!(matches!(fx.last(), Some(Effect::ConfigChanged { .. })));
        assert_eq!(c.config.playback.seek_step_s, 30.0);
        assert!(c.config.remote.api_enabled);
        assert_eq!(c.config.updates.channel, "beta");
        // Same model again: nothing to do.
        assert!(c.handle_ui(UiAction::SettingsChanged(m)).is_empty());
        // Generated token is persisted once.
        let fx = c.set_pairing(Some("http://x".into()), Some("tok".into()));
        assert_eq!(fx.len(), 1);
        assert_eq!(c.config.remote.api_token.as_deref(), Some("tok"));
        assert!(c
            .set_pairing(Some("http://x".into()), Some("tok".into()))
            .is_empty());
        let fx = c.handle_ui(UiAction::ConnectHapticsDevice("buttplug".into()));
        assert!(matches!(fx[0], Effect::ConfigChanged { .. }));
        assert_eq!(c.config.haptics.backend, "buttplug");
        assert!(c.config.haptics.enabled);
    }

    #[test]
    fn input_actions() {
        let mut c = controller_with_item(item(1, "file:///v/a.mp4"));
        assert!(
            c.handle_input(InputAction::TogglePlay).is_empty(),
            "no session"
        );
        open_item(&mut c, 1);
        c.on_player_status(status(PlaybackState::Playing, 100.0));
        assert_eq!(
            c.handle_input(InputAction::SeekStep(1)),
            vec![Effect::Player(PlayerOp::Seek(
                MediaTime::from_secs_f64(110.0),
                SeekMode::Precise
            ))]
        );
        let fx = c.handle_input(InputAction::SpeedStep(1));
        assert_eq!(fx[0], Effect::Player(PlayerOp::SetSpeed(1.25)));
        c.handle_input(InputAction::AdjustScreen {
            distance: 1.0,
            size: 0.5,
        });
        match &c.session.as_ref().unwrap().view.projection {
            Projection::Flat {
                width_m,
                distance_m,
                ..
            } => {
                assert_eq!((*width_m, *distance_m), (4.5, 4.5));
            }
            p => panic!("{p:?}"),
        }
        assert_eq!(
            c.handle_input(InputAction::Recenter),
            vec![Effect::Recenter]
        );
    }

    #[test]
    fn open_failure_returns_to_library() {
        let mut c = Controller::new(Config::default());
        let fx = c.open(OpenTarget::Uri("smb://x/y.mp4".into()), None);
        let Effect::Open { token, .. } = fx[0] else {
            panic!()
        };
        let fx = c.on_open_failed(token, "boom");
        assert!(matches!(&fx[0], Effect::Toast(t, ToastKind::Error) if t.contains("boom")));
        assert_eq!(c.screen(), Screen::Library);
    }
}
