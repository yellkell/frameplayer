//! Build the fp-ui screen view structs from controller / library / player
//! state. Pure functions of [`Controller`], so the frame loop just calls
//! them each frame and the mapping is unit-tested.

use crate::controller::{source_key, Controller, ThumbState, CONTINUE_SOURCE};
use fp_core::draw::TextureId;
use fp_core::MediaTime;
use fp_library::Item;
use fp_sources::SourceKind as SrcKind;
use fp_ui::screens::{
    GeneralSettings, HapticsSettings, LibraryItem, LibraryView, PictureView, PlaybackSettings,
    PlayerView, RemoteSettings, ScriptStatus, SettingsModel, SourceEntry, SourceKind, TrackEntry,
    UpdateSettings,
};
use fp_video::PlaybackState;

fn ui_source_kind(id: &str, k: SrcKind) -> SourceKind {
    match k {
        SrcKind::Local if id.starts_with("mount:") => SourceKind::Removable,
        SrcKind::Local => SourceKind::Local,
        SrcKind::Smb => SourceKind::Smb,
        SrcKind::WebDav => SourceKind::WebDav,
        SrcKind::Http => SourceKind::Http,
        SrcKind::Sftp => SourceKind::Sftp,
        SrcKind::Dlna => SourceKind::Dlna,
        SrcKind::DeoVr => SourceKind::DeoVrFeed,
    }
}

/// One grid tile.
pub fn library_item(it: &Item, thumb: Option<&ThumbState>) -> LibraryItem {
    LibraryItem {
        id: it.id as u64,
        title: it.title.clone(),
        duration: it.duration,
        width: it.width.unwrap_or(0),
        height: it.height.unwrap_or(0),
        codec: it.codec,
        projection: it.projection.clone(),
        stereo: it.stereo.unwrap_or_default(),
        hdr: it.hdr,
        thumbnail: match thumb {
            Some(ThumbState::Ready(key)) => Some(*key),
            _ => None,
        },
        favourite: it.favourite,
        resume: match (it.resume, it.duration) {
            (Some(r), Some(d)) if d.0 > 0 => Some((r.0 as f64 / d.0 as f64).clamp(0.0, 1.0) as f32),
            _ => None,
        },
        tags: it.tags.clone(),
        has_script: it.has_script,
    }
}

pub fn library_view(c: &Controller) -> LibraryView {
    let l = &c.library;
    let shelf = SourceEntry {
        id: source_key(CONTINUE_SOURCE),
        name: "Continue watching".into(),
        kind: SourceKind::Local,
        online: true,
        item_count: None,
    };
    LibraryView {
        sources: std::iter::once(shelf)
            .chain(l.sources.iter().map(|s| SourceEntry {
                id: source_key(&s.id),
                name: s.name.clone(),
                kind: ui_source_kind(&s.id, s.kind),
                online: l.online.get(&s.id).copied().unwrap_or(true),
                item_count: l.counts.get(&s.id).copied(),
            }))
            .collect(),
        selected_source: l.selected_source.as_deref().map(source_key),
        items: l
            .items
            .iter()
            .map(|it| library_item(it, l.thumbs.get(&it.id)))
            .collect(),
        all_tags: l.all_tags.clone(),
        active_tags: l.active_tags.clone(),
        sort: l.sort,
        sort_descending: l.descending,
        favourites_only: l.favourites_only,
        loading: l.loading,
        status: l.status.clone(),
    }
}

fn track_label(lang: &Option<String>, title: &Option<String>, fallback: String) -> String {
    match (lang, title) {
        (Some(l), Some(t)) => format!("{t} ({l})"),
        (Some(l), None) => l.clone(),
        (None, Some(t)) => t.clone(),
        (None, None) => fallback,
    }
}

pub fn player_view(c: &Controller) -> PlayerView {
    let mut v = PlayerView {
        seek_step: MediaTime::from_secs_f64(c.config.playback.seek_step_s as f64),
        ..Default::default()
    };
    let Some(s) = &c.session else {
        return v;
    };
    let st = &c.player;
    v.title = s.title.clone();
    v.position = st.position;
    v.duration = st
        .duration
        .or_else(|| s.duration())
        .unwrap_or(MediaTime::ZERO);
    v.playing = st.state == PlaybackState::Playing;
    v.buffering =
        s.resolving || matches!(st.state, PlaybackState::Opening | PlaybackState::Seeking);
    if st.buffered > MediaTime::ZERO {
        v.buffered = vec![(MediaTime::ZERO, st.buffered)];
    }
    v.loop_a = s.loop_a;
    v.loop_b = s.loop_b;
    v.speed = st.speed as f32;
    v.projection = s.view.projection.clone();
    v.stereo = s.view.stereo;
    v.swap_eyes = s.view.swap_eyes;
    if let Some(m) = &s.media {
        v.chapters = m.chapters.clone();
        v.subtitle_tracks = m
            .subtitles
            .iter()
            .map(|t| TrackEntry {
                id: t.index,
                label: track_label(
                    &t.language,
                    &t.title,
                    format!("Track {} ({})", t.index, t.format),
                ),
            })
            .collect();
        v.audio_tracks = m
            .audio
            .iter()
            .map(|t| TrackEntry {
                id: t.index,
                label: track_label(
                    &t.language,
                    &t.title,
                    format!("Track {} · {} ch", t.index, t.channels),
                ),
            })
            .collect();
    }
    v.selected_subtitle = st.subtitle_track;
    v.selected_audio = st.audio_track;
    v.script = s.script.as_ref().map(|sc| ScriptStatus {
        name: sc.name.clone(),
        device: c.runtime.haptics_device.clone(),
        enabled: sc.enabled,
        heat: sc.heat.clone(),
    });
    if let (Some(sp), Some(t)) = (&s.sprite, s.preview_time) {
        let (_, [x, y, w, h]) = sp.info.tile_for(t);
        let (sw, sh) = (sp.sheet.0.max(1) as f32, sp.sheet.1.max(1) as f32);
        v.preview = Some((
            TextureId::Image(sp.key),
            [
                x as f32 / sw,
                y as f32 / sh,
                (x + w) as f32 / sw,
                (y + h) as f32 / sh,
            ],
        ));
        if h > 0 {
            v.preview_aspect = w as f32 / h as f32;
        }
    }
    v
}

pub fn picture_view(c: &Controller) -> PictureView {
    match &c.session {
        Some(s) => PictureView {
            corrections: s.view.corrections_at(c.player.position),
            keyframe_count: s.view.keyframes.len(),
            position: c.player.position,
        },
        None => PictureView::default(),
    }
}

pub fn settings_model(c: &Controller) -> SettingsModel {
    let cfg = &c.config;
    let rt = &c.runtime;
    SettingsModel {
        general: GeneralSettings {
            refresh_rate_hz: cfg.general.refresh_rate_hz,
            available_refresh_rates: rt.refresh_rates.clone(),
            gaze_dimming: cfg.comfort.gaze_dimming,
            passthrough_background: cfg.comfort.passthrough_background,
            head_locked_screen: cfg.comfort.head_locked_screen,
            lying_down: cfg.comfort.lying_down,
            environment_dim: cfg.comfort.environment_dim,
        },
        playback: PlaybackSettings {
            default_speed: cfg.playback.default_speed,
            resume_playback: cfg.playback.resume_playback,
            seek_step_s: cfg.playback.seek_step_s,
            subtitle_depth_m: cfg.playback.subtitle_depth_m,
            av_offset_ms: cfg.playback.av_offset_ms,
        },
        remote: RemoteSettings {
            api_enabled: cfg.remote.api_enabled,
            api_port: cfg.remote.api_port,
            deovr_enabled: cfg.remote.deovr_enabled,
            deovr_port: cfg.remote.deovr_port,
            pairing_url: rt.pairing_url.clone(),
        },
        haptics: HapticsSettings {
            enabled: cfg.haptics.enabled,
            offset_ms: cfg.haptics.offset_ms,
            devices: rt
                .haptics_devices
                .iter()
                .map(|d| fp_ui::screens::HapticsDevice {
                    connected: cfg.haptics.enabled
                        && cfg.haptics.backend == d.id
                        && rt.haptics_device.is_some(),
                    ..d.clone()
                })
                .collect(),
            scanning: rt.haptics_scanning,
        },
        updates: UpdateSettings {
            current_version: rt.version.clone(),
            beta_channel: cfg.updates.channel.eq_ignore_ascii_case("beta"),
            check_on_start: cfg.updates.check_on_start,
            available: rt.update_available.clone(),
            progress: rt.update_progress,
            status: rt.update_status.clone(),
        },
    }
}

/// Remote-API view of the player (seconds).
pub fn remote_status(c: &Controller) -> fp_remote::PlayerStatus {
    match &c.session {
        None => fp_remote::PlayerStatus::default(),
        Some(s) => fp_remote::PlayerStatus {
            path: Some(s.uri.clone()),
            title: Some(s.title.clone()),
            item_id: s.item.as_ref().map(|i| i.id.to_string()),
            duration: c
                .player
                .duration
                .or_else(|| s.duration())
                .map(|d| d.as_secs_f64()),
            position: c.player.position.as_secs_f64(),
            speed: c.player.speed,
            playing: c.player.state == PlaybackState::Playing,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::controller::tests::item;
    use crate::controller::{OpenTarget, OpenedMeta, ScriptInfo, SpriteState, SPRITE_KEY_BASE};
    use fp_core::{AudioTrackInfo, MediaInfo, Projection, StereoMode, SubtitleTrackInfo};
    use fp_library::SpriteInfo;
    use fp_video::PlayerStatus;

    #[test]
    fn library_tiles() {
        let mut it = item(7, "file:///v/a_180_LR.mp4");
        it.resume = Some(MediaTime::from_secs_f64(150.0));
        it.projection = Some(Projection::EQUIRECT_180);
        it.stereo = Some(StereoMode::Sbs);
        let t = library_item(&it, Some(&ThumbState::Ready(42)));
        assert_eq!(t.id, 7);
        assert_eq!(t.thumbnail, Some(42));
        assert_eq!(t.resume, Some(0.25));
        assert_eq!((t.width, t.height), (3840, 1920));
        assert_eq!(t.stereo, StereoMode::Sbs);
        assert_eq!(
            library_item(&it, Some(&ThumbState::Requested)).thumbnail,
            None
        );
    }

    #[test]
    fn library_view_maps_sources() {
        let mut c = Controller::new(Config::default());
        c.library.sources = vec![
            fp_sources::SourceConfig {
                id: "mount:/run/media/deck/SD".into(),
                name: "SD".into(),
                kind: SrcKind::Local,
                uri: "file:///run/media/deck/SD".into(),
                pinned_host_key: None,
            },
            fp_sources::SourceConfig {
                id: "xbvr".into(),
                name: "XBVR".into(),
                kind: SrcKind::DeoVr,
                uri: "https://xbvr:9999".into(),
                pinned_host_key: None,
            },
        ];
        c.library.online.insert("xbvr".into(), false);
        c.library.selected_source = Some("xbvr".into());
        c.library.items = vec![item(1, "file:///a.mp4")];
        let v = library_view(&c);
        assert_eq!(v.sources[0].name, "Continue watching");
        assert_eq!(v.sources[1].kind, SourceKind::Removable);
        assert_eq!(v.sources[2].kind, SourceKind::DeoVrFeed);
        assert!(!v.sources[2].online);
        assert_eq!(v.selected_source, Some(source_key("xbvr")));
        assert_eq!(v.items.len(), 1);
    }

    #[test]
    fn player_view_from_session() {
        let mut c = Controller::new(Config::default());
        c.library.items = vec![item(1, "file:///v/a.mp4")];
        let fx = c.open(OpenTarget::Item(1), None);
        let crate::controller::Effect::Open { token, .. } = fx[0] else {
            panic!()
        };
        c.on_media_opened(OpenedMeta {
            token,
            uri: "file:///v/a.mp4".into(),
            item: Some(c.library.items[0].clone()),
            ..Default::default()
        });
        let v = player_view(&c);
        assert_eq!(v.title, "a");
        assert_eq!(
            v.duration,
            MediaTime::from_secs_f64(600.0),
            "from library until probed"
        );
        c.on_player_status(PlayerStatus {
            state: PlaybackState::Playing,
            position: MediaTime::from_secs_f64(30.0),
            duration: Some(MediaTime::from_secs_f64(601.0)),
            buffered: MediaTime::from_secs_f64(40.0),
            speed: 1.5,
            audio_track: Some(2),
            ..Default::default()
        });
        c.on_player_event(&fp_video::PlayerEvent::Opened(Box::new(MediaInfo {
            audio: vec![AudioTrackInfo {
                index: 2,
                channels: 6,
                sample_rate: 48000,
                language: Some("en".into()),
                title: None,
                ambisonic_order: None,
            }],
            subtitles: vec![SubtitleTrackInfo {
                index: 3,
                language: None,
                title: None,
                format: "srt".into(),
            }],
            ..Default::default()
        })));
        let s = c.session.as_mut().unwrap();
        s.script = Some(ScriptInfo {
            name: "a.funscript".into(),
            heat: vec![0.5; 4],
            enabled: true,
        });
        s.sprite = Some(SpriteState {
            info: SpriteInfo {
                columns: 2,
                rows: 2,
                tile_width: 100,
                tile_height: 50,
                interval_secs: 10.0,
            },
            key: SPRITE_KEY_BASE | 1,
            sheet: (200, 100),
        });
        s.preview_time = Some(MediaTime::from_secs_f64(15.0));
        c.runtime.haptics_device = Some("Handy".into());
        let v = player_view(&c);
        assert!(v.playing && !v.buffering);
        assert_eq!(v.position, MediaTime::from_secs_f64(30.0));
        assert_eq!(v.duration, MediaTime::from_secs_f64(601.0));
        assert_eq!(v.speed, 1.5);
        assert_eq!(
            v.buffered,
            vec![(MediaTime::ZERO, MediaTime::from_secs_f64(40.0))]
        );
        assert_eq!(v.audio_tracks[0].label, "en");
        assert_eq!(v.selected_audio, Some(2));
        assert_eq!(v.subtitle_tracks[0].label, "Track 3 (srt)");
        assert_eq!(v.script.as_ref().unwrap().device.as_deref(), Some("Handy"));
        // Tile 1 of a 2x2 sheet: top-right quarter.
        assert_eq!(
            v.preview,
            Some((TextureId::Image(SPRITE_KEY_BASE | 1), [0.5, 0.0, 1.0, 0.5]))
        );
        assert_eq!(v.preview_aspect, 2.0);

        let r = remote_status(&c);
        assert_eq!(r.item_id.as_deref(), Some("1"));
        assert_eq!(r.position, 30.0);
        assert!(r.playing);
        assert_eq!(r.duration, Some(601.0));
    }

    #[test]
    fn no_session_views() {
        let c = Controller::new(Config::default());
        assert_eq!(player_view(&c).title, "");
        assert_eq!(picture_view(&c), PictureView::default());
        assert_eq!(remote_status(&c), fp_remote::PlayerStatus::default());
    }

    #[test]
    fn settings_reflect_config_and_runtime() {
        let mut c = Controller::new(Config::default());
        c.config.haptics.enabled = true;
        c.config.haptics.backend = "buttplug".into();
        c.runtime.haptics_device = Some("Intiface".into());
        c.runtime.haptics_devices = vec![fp_ui::screens::HapticsDevice {
            id: "buttplug".into(),
            name: "Intiface".into(),
            backend: "buttplug".into(),
            connected: false,
        }];
        c.runtime.refresh_rates = vec![72.0, 90.0];
        let m = settings_model(&c);
        assert!(m.haptics.devices[0].connected);
        assert_eq!(m.general.available_refresh_rates, vec![72.0, 90.0]);
        assert_eq!(m.playback.seek_step_s, 10.0);
        assert!(!m.updates.beta_channel);
    }
}
