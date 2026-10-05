//! The media currently open: player, format, adjustments, scripts,
//! subtitles and markers, plus progress bookkeeping in the library.

use crate::services::Opener;
use crate::settings::Settings;
use fp_core::format::{self, ContainerHints, Evidence, VideoFormat};
use fp_core::source::Entry;
use fp_core::view::{Keyframes, ViewSettings};
use fp_core::{PlaybackStatus, playback::now_ms};
use fp_haptics::{Axis, Heatmap, Script};
use fp_library::{Library, MediaId, SessionId};
use fp_media::{Player, PlayerConfig, PlayerState};
use std::time::Instant;

/// What to open.
#[derive(Clone, Debug, Default)]
pub struct OpenRequest {
    pub location: String,
    pub source_id: Option<String>,
    /// The browse entry, when opened from a source (carries feed metadata).
    pub entry: Option<Entry>,
    /// Start here instead of the saved resume position.
    pub start_at: Option<f64>,
}

pub struct Playback {
    pub player: Player,
    pub request: OpenRequest,
    pub title: String,
    pub record_id: Option<MediaId>,
    session: Option<SessionId>,
    pub format: VideoFormat,
    pub evidence: Evidence,
    /// Format the file would have without the user's override.
    pub detected: VideoFormat,
    pub settings: ViewSettings,
    pub keyframes: Keyframes,
    /// Adjustments differ from what is saved for this video.
    pub settings_dirty: bool,
    pub heatmap: Option<Heatmap>,
    pub markers: Vec<(f64, String)>,
    pub subtitle_files: Vec<(String, String)>,
    pub active_subtitle_file: Option<usize>,
    pub script_count: usize,
    last_save: Instant,
    pub notice: Option<String>,
}

/// The result of opening on a background thread.
pub struct Opened {
    pub playback: Playback,
    pub scripts: Vec<(Axis, Script)>,
}

fn file_name(location: &str) -> String {
    let trimmed = location.split(['?', '#']).next().unwrap_or(location);
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    percent_decode(last)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn title_from_name(name: &str) -> String {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    stem.replace(['_', '.'], " ").trim().to_string()
}

/// Opens media; blocking (network probing), so run it on a job thread.
pub fn open(
    req: OpenRequest,
    opener: &Opener,
    library: &Library,
    settings: &Settings,
) -> Result<Opened, String> {
    let name = file_name(&req.location);
    // Feeds may need a details call to get the stream URL and metadata.
    let entry = match (
        &req.entry,
        req.source_id.as_deref().and_then(|id| opener.source(id)),
    ) {
        (Some(e), Some(src)) => Some(src.details(e).unwrap_or_else(|_| e.clone())),
        (e, _) => e.clone(),
    };
    let location = entry
        .as_ref()
        .map(|e| e.location.clone())
        .unwrap_or_else(|| req.location.clone());

    // Library record: existing, or created for remote entries.
    let mut record = library.get_by_location(&location).ok().flatten();
    if record.is_none()
        && let (Some(e), Some(sid)) = (&entry, &req.source_id)
        && library.upsert_entry(sid, e).is_ok()
    {
        record = library.get_by_location(&location).ok().flatten();
    }

    let src = opener.open_in(req.source_id.as_deref(), &location)?;
    let start_at = req
        .start_at
        .or_else(|| {
            record
                .as_ref()
                .filter(|_| settings.resume)
                .map(|r| r.resume_position)
                .filter(|p| *p > 1.0)
        })
        .unwrap_or(0.0);
    let cfg = PlayerConfig {
        hw_decode: if settings.hardware_decoding {
            fp_media::HwDecode::Auto
        } else {
            fp_media::HwDecode::Off
        },
        audio: fp_media::audio::output::Backend::Alsa {
            device: settings.audio_device.clone(),
            latency_ms: 80,
        },
        start_at,
        volume: settings.volume,
        ..Default::default()
    };
    let player = Player::open(src, &name, cfg).map_err(|e| format!("{name}: {e}"))?;
    let info = player.info().clone();
    let (w, h) = info
        .video_stream()
        .map(|v| (v.width, v.height))
        .unwrap_or((0, 0));

    // Format: user override > container metadata > declared by the source >
    // file name > aspect ratio.
    let user = record.as_ref().and_then(|r| r.user_format);
    let hints = if info.hints.projection.is_some() || info.hints.stereo.is_some() {
        info.hints
    } else if let Some(f) = entry.as_ref().and_then(|e| e.format) {
        ContainerHints {
            projection: Some(f.projection),
            stereo: Some(f.stereo),
        }
    } else {
        info.hints
    };
    let resolved = format::resolve(user, hints, &name, w, h);
    let detected = format::resolve(None, hints, &name, w, h).format;

    let settings_v = record
        .as_ref()
        .and_then(|r| r.view_settings)
        .unwrap_or_else(|| settings.new_video_view());
    let keyframes = record
        .as_ref()
        .map(|r| r.keyframes.clone())
        .unwrap_or_default();
    let title = record
        .as_ref()
        .map(|r| r.title.clone())
        .filter(|t| !t.is_empty())
        .or_else(|| info.title.clone())
        .or_else(|| entry.as_ref().map(|e| e.name.clone()))
        .unwrap_or_else(|| title_from_name(&name));

    // Scripts and subtitles: library record, entry, then the source's sidecars.
    let mut script_locs: Vec<String> = record
        .as_ref()
        .map(|r| r.scripts.clone())
        .unwrap_or_default();
    let mut subs: Vec<(String, String)> = record
        .as_ref()
        .map(|r| {
            r.subtitles
                .iter()
                .map(|s| (file_name(s), s.clone()))
                .collect()
        })
        .unwrap_or_default();
    if let Some(e) = &entry {
        script_locs.extend(e.scripts.iter().cloned());
        subs.extend(e.subtitles.iter().map(|s| (file_name(s), s.clone())));
        if let Some(src) = req.source_id.as_deref().and_then(|id| opener.source(id))
            && let Ok(side) = src.sidecars(e)
        {
            script_locs.extend(side.scripts.into_iter().map(|s| s.location));
            subs.extend(side.subtitles.into_iter().map(|s| (s.name, s.location)));
        }
    }
    if script_locs.is_empty() && location.starts_with('/') {
        let dirs = vec![fp_core::dirs::interactive_dir()];
        script_locs = fp_haptics::funscript::find_scripts(std::path::Path::new(&location), &dirs)
            .into_iter()
            .map(|(_, p)| p.display().to_string())
            .collect();
    }
    script_locs.dedup();
    subs.dedup_by(|a, b| a.1 == b.1);

    let mut scripts = Vec::new();
    for loc in &script_locs {
        match opener.fetch(loc, 64 << 20).and_then(|bytes| {
            fp_haptics::funscript::parse_script_file(&file_name(loc), &bytes)
                .map_err(|e| e.to_string())
        }) {
            Ok(mut s) => scripts.append(&mut s),
            Err(e) => log::warn!("script {loc}: {e}"),
        }
    }
    let duration_ms = (info.duration * 1000.0) as i64;
    let heatmap = scripts
        .iter()
        .find(|(a, _)| *a == Axis::L0)
        .filter(|_| duration_ms > 0)
        .map(|(_, s)| fp_haptics::heatmap_range(s, 0, duration_ms, 240));

    let mut active_subtitle_file = None;
    if let Some((i, (n, loc))) = subs.iter().enumerate().next() {
        match opener
            .open(loc)
            .map_err(|e| e.to_string())
            .and_then(|s| player.load_subtitles(s, n).map_err(|e| e.to_string()))
        {
            Ok(_) => active_subtitle_file = Some(i),
            Err(e) => log::warn!("subtitles {loc}: {e}"),
        }
    }

    let mut markers: Vec<(f64, String)> = info
        .chapters
        .iter()
        .map(|c| (c.start, c.title.clone()))
        .collect();
    if let Some(e) = &entry {
        markers.extend(e.markers.iter().cloned());
    }
    if let Some(r) = &record
        && let Ok(m) = library.markers(r.id)
    {
        markers.extend(m.into_iter().map(|m| (m.time, m.name)));
    }
    markers.sort_by(|a, b| a.0.total_cmp(&b.0));
    markers.dedup_by(|a, b| (a.0 - b.0).abs() < 0.5 && a.1 == b.1);

    let record_id = record.as_ref().map(|r| r.id);
    let session = record_id.and_then(|id| library.start_playback(id).ok());
    let script_count = scripts.len();
    Ok(Opened {
        playback: Playback {
            player,
            request: OpenRequest { location, ..req },
            title,
            record_id,
            session,
            format: resolved.format,
            evidence: resolved.evidence,
            detected,
            settings: settings_v,
            keyframes,
            settings_dirty: false,
            heatmap,
            markers,
            subtitle_files: subs,
            active_subtitle_file,
            script_count,
            last_save: Instant::now(),
            notice: None,
        },
        scripts,
    })
}

impl Playback {
    pub fn status(&self) -> PlaybackStatus {
        let p = &self.player;
        PlaybackStatus {
            location: self.request.location.clone(),
            title: self.title.clone(),
            duration: p.duration(),
            position: p.position(),
            speed: p.speed(),
            playing: matches!(p.state(), PlayerState::Playing),
            sampled_at_ms: now_ms(),
        }
    }

    /// View settings at the current position (keyframes applied).
    pub fn current_settings(&self) -> ViewSettings {
        self.keyframes.at(self.player.position(), &self.settings)
    }

    /// What is shown now: [`Self::current_settings`] with the global chroma
    /// key unless this video has its own.
    pub fn shown_settings(&self, global: &ViewSettings) -> ViewSettings {
        self.current_settings().with_global_key(global)
    }

    /// Saves progress every few seconds and at the end.
    pub fn save_progress(&mut self, library: &Library, force: bool) {
        if !force && self.last_save.elapsed().as_secs_f64() < 5.0 {
            return;
        }
        self.last_save = Instant::now();
        let finished = self.player.state() == PlayerState::Ended;
        if let Some(s) = self.session
            && let Err(e) = library.update_playback(s, self.player.position(), finished)
        {
            log::warn!("saving progress: {e}");
        }
    }

    pub fn set_format(&mut self, library: &Library, format: Option<VideoFormat>) {
        self.format = format.unwrap_or(self.detected);
        self.evidence = if format.is_some() {
            Evidence::User
        } else {
            Evidence::FileName
        };
        if let Some(id) = self.record_id
            && let Err(e) = library.set_user_format(id, format)
        {
            log::warn!("saving format: {e}");
        }
    }

    pub fn save_settings(&mut self, library: &Library) {
        if let Some(id) = self.record_id {
            let r = library
                .set_view_settings(id, Some(&self.settings))
                .and_then(|_| library.set_keyframes(id, &self.keyframes));
            match r {
                Ok(()) => self.settings_dirty = false,
                Err(e) => log::warn!("saving adjustments: {e}"),
            }
        } else {
            self.notice = Some(
                "Adjustments apply until you close this video (it is not in the library).".into(),
            );
        }
    }

    pub fn add_bookmark(&mut self, library: &Library) {
        let t = self.player.position();
        let name = format!("Bookmark {}", crate::ui::fmt_time(t));
        if let Some(id) = self.record_id {
            let _ = library.add_marker(id, t, &name);
        }
        self.markers.push((t, name));
        self.markers.sort_by(|a, b| a.0.total_cmp(&b.0));
    }

    pub fn close(mut self, library: &Library) {
        self.save_progress(library, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_titles() {
        assert_eq!(
            file_name("http://h/a%20b/Scene_180_LR.mp4?token=1"),
            "Scene_180_LR.mp4"
        );
        assert_eq!(file_name("/media/x/My.Video_2024.mkv"), "My.Video_2024.mkv");
        assert_eq!(title_from_name("My.Video_2024.mkv"), "My Video 2024");
        assert_eq!(percent_decode("a%2Fb%zz"), "a/b%zz");
    }
}
