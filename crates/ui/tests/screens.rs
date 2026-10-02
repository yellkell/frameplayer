//! Screen-level tests: snapshot-style draw list checks and simulated input
//! producing the expected `UiAction`s.

use fp_core::media::Chapter;
use fp_core::{Codec, Corrections, MediaTime, Projection, StereoMode};
use fp_ui::input::{FrameInput, NavInput, PointerInput, PointerSource, TextEvent};
use fp_ui::painter::validate;
use fp_ui::screens::*;
use fp_ui::{Hand, Ui, UiOutput, Vec2};

const DT: f32 = 1.0 / 72.0;

fn laser(pos: Vec2, pressed: bool) -> FrameInput {
    FrameInput {
        dt: DT,
        pointers: vec![PointerInput::new(
            PointerSource::Laser(Hand::Right),
            Some(pos),
            pressed,
        )],
        ..Default::default()
    }
}

fn idle() -> FrameInput {
    FrameInput {
        dt: DT,
        ..Default::default()
    }
}

fn run<R>(ui: &mut Ui, inp: FrameInput, f: impl FnOnce(&mut Ui) -> R) -> (R, UiOutput) {
    ui.begin_frame(inp);
    let r = f(ui);
    let out = ui.end_frame();
    (r, out)
}

/// Clicks the recorded widget `label` (from the last frame) and returns all
/// actions produced during the press/release frames.
fn click<S>(
    ui: &mut Ui,
    state: &mut S,
    label: &str,
    mut show: impl FnMut(&mut S, &mut Ui) -> Vec<UiAction>,
) -> Vec<UiAction> {
    let at = ui
        .find_widget(label)
        .unwrap_or_else(|| {
            panic!(
                "widget {label:?} not found; have {:?}",
                ui.widgets().iter().map(|w| &w.label).collect::<Vec<_>>()
            )
        })
        .rect
        .center();
    let mut actions = run(ui, laser(at, false), |ui| show(state, ui)).0;
    actions.extend(run(ui, laser(at, true), |ui| show(state, ui)).0);
    actions.extend(run(ui, laser(at, false), |ui| show(state, ui)).0);
    actions
}

fn assert_snapshot(out: &UiOutput, ui: &Ui) {
    validate(&out.draw_list).unwrap();
    assert!(
        out.draw_list.vertices.len() > 100,
        "draw list suspiciously small: {}",
        out.draw_list.vertices.len()
    );
    assert!(!out.draw_list.cmds.is_empty());
    let (w, h) = (ui.size().x, ui.size().y);
    for c in &out.draw_list.cmds {
        assert!(
            c.clip[0] >= 0.0
                && c.clip[1] >= 0.0
                && c.clip[0] + c.clip[2] <= w + 0.5
                && c.clip[1] + c.clip[3] <= h + 0.5,
            "clip {:?}",
            c.clip
        );
    }
    assert!(
        out.draw_list
            .cmds
            .iter()
            .any(|c| c.texture == fp_core::draw::TextureId::FontAtlas),
        "text drawn"
    );
    let atlas = ui.atlas();
    assert_eq!(atlas.pixels.len(), (atlas.width * atlas.height) as usize);
    assert!(atlas.pixels.iter().any(|&p| p > 0));
}

fn library_view() -> LibraryView {
    let items = (0..200)
        .map(|i| LibraryItem {
            id: i,
            title: format!("Video number {i} with a fairly long title"),
            duration: Some(MediaTime::from_millis(600_000 + i as i64 * 1000)),
            width: 7680,
            height: 3840,
            codec: Some(Codec::Hevc),
            projection: Some(Projection::EQUIRECT_180),
            stereo: StereoMode::Sbs,
            hdr: i % 3 == 0,
            thumbnail: if i % 2 == 0 { Some(i) } else { None },
            favourite: i % 5 == 0,
            resume: if i % 4 == 0 { Some(0.4) } else { None },
            tags: vec!["outdoor".into(), "8k".into()],
            has_script: i % 7 == 0,
        })
        .collect();
    LibraryView {
        sources: vec![
            SourceEntry {
                id: 1,
                name: "Internal".into(),
                kind: SourceKind::Local,
                online: true,
                item_count: Some(200),
            },
            SourceEntry {
                id: 2,
                name: "NAS (SMB)".into(),
                kind: SourceKind::Smb,
                online: false,
                item_count: None,
            },
            SourceEntry {
                id: 3,
                name: "XBVR".into(),
                kind: SourceKind::DeoVrFeed,
                online: true,
                item_count: Some(1234),
            },
        ],
        selected_source: Some(1),
        items,
        all_tags: vec!["outdoor".into(), "8k".into(), "fisheye".into()],
        active_tags: vec!["8k".into()],
        ..Default::default()
    }
}

#[test]
fn library_snapshot_and_actions() {
    let mut ui = Ui::new(Vec2::new(2000.0, 1200.0), 1000.0);
    ui.set_record_widgets(true);
    let view = library_view();
    let mut screen = LibraryScreen::new();
    let show = |s: &mut LibraryScreen, ui: &mut Ui| s.show(ui, &view);
    let (actions, out) = run(&mut ui, idle(), |ui| show(&mut screen, ui));
    assert_snapshot(&out, &ui);
    // Thumbnails are images; the visible range is reported once.
    assert!(out
        .draw_list
        .cmds
        .iter()
        .any(|c| matches!(c.texture, fp_core::draw::TextureId::Image(_))));
    let vis: Vec<_> = actions
        .iter()
        .filter_map(|a| {
            if let UiAction::VisibleItems(r) = a {
                Some(r.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(vis.len(), 1);
    assert!(
        vis[0].start == 0 && vis[0].end < 200,
        "virtualized: {:?}",
        vis[0]
    );
    let (again, _) = run(&mut ui, idle(), |ui| show(&mut screen, ui));
    assert!(
        !again.iter().any(|a| matches!(a, UiAction::VisibleItems(_))),
        "not re-sent while unchanged"
    );

    // Tile click opens; star click toggles favourite without opening.
    let a = click(
        &mut ui,
        &mut screen,
        "Video number 1 with a fairly long title",
        show,
    );
    assert!(a.contains(&UiAction::OpenItem(1)), "{a:?}");
    let a = click(
        &mut ui,
        &mut screen,
        "favourite Video number 2 with a fairly long title",
        show,
    );
    assert!(a.contains(&UiAction::ToggleFavourite(2)));
    assert!(!a.contains(&UiAction::OpenItem(2)));

    // Sidebar, tags and settings.
    let a = click(&mut ui, &mut screen, "XBVR", show);
    assert!(a.contains(&UiAction::SelectSource(3)));
    let a = click(&mut ui, &mut screen, "tag fisheye", show);
    assert!(a.contains(&UiAction::ToggleTag("fisheye".into())));
    let a = click(&mut ui, &mut screen, "Settings", show);
    assert!(a.contains(&UiAction::OpenSettings));
    let a = click(&mut ui, &mut screen, "StarOutline", show);
    assert!(a.contains(&UiAction::SetFavouritesOnly(true)));

    // Sort dropdown.
    click(&mut ui, &mut screen, "Title", show);
    run(&mut ui, idle(), |ui| show(&mut screen, ui));
    let a = click(&mut ui, &mut screen, "Duration", show);
    assert!(
        a.contains(&UiAction::SetSort {
            key: SortKey::Duration,
            descending: false
        }),
        "{a:?}"
    );

    // Search: focusing shows the keyboard; typing emits Search.
    click(&mut ui, &mut screen, "Search library", show);
    assert!(screen.show_keyboard);
    let mut inp = idle();
    inp.text.push(TextEvent::Text("beach".into()));
    let (a, out) = run(&mut ui, inp, |ui| show(&mut screen, ui));
    assert!(a.contains(&UiAction::Search("beach".into())), "{a:?}");
    assert!(out.wants_text);
    run(&mut ui, idle(), |ui| show(&mut screen, ui));
    let a = click(&mut ui, &mut screen, "s", show);
    run(&mut ui, idle(), |ui| show(&mut screen, ui));
    assert!(
        screen.search == "beachs" || a.contains(&UiAction::Search("beachs".into())),
        "{}",
        screen.search
    );
    // Hide key closes the keyboard; B then goes back.
    click(&mut ui, &mut screen, "hide", show);
    assert!(!screen.show_keyboard);
    let inp = FrameInput {
        dt: DT,
        nav: NavInput {
            back: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let (a, _) = run(&mut ui, inp, |ui| show(&mut screen, ui));
    assert!(a.contains(&UiAction::Back));
}

#[test]
fn library_empty_and_loading_states() {
    let mut ui = Ui::new(Vec2::new(1600.0, 1000.0), 1000.0);
    let mut screen = LibraryScreen::new();
    let view = LibraryView {
        loading: true,
        ..Default::default()
    };
    let (_, out) = run(&mut ui, idle(), |ui| screen.show(ui, &view));
    validate(&out.draw_list).unwrap();
    let view = LibraryView {
        status: Some("Server unreachable".into()),
        ..Default::default()
    };
    let (_, out) = run(&mut ui, idle(), |ui| screen.show(ui, &view));
    validate(&out.draw_list).unwrap();
}

fn player_view() -> PlayerView {
    PlayerView {
        title: "Some 180° SBS video".into(),
        position: MediaTime::from_millis(65_000),
        duration: MediaTime::from_millis(600_000),
        playing: true,
        buffered: vec![(MediaTime::ZERO, MediaTime::from_millis(120_000))],
        chapters: vec![
            Chapter {
                start: MediaTime::ZERO,
                title: "Intro".into(),
            },
            Chapter {
                start: MediaTime::from_millis(200_000),
                title: "Part 2".into(),
            },
        ],
        loop_a: Some(MediaTime::from_millis(30_000)),
        projection: Projection::EQUIRECT_180,
        stereo: StereoMode::Sbs,
        subtitle_tracks: vec![TrackEntry {
            id: 3,
            label: "English".into(),
        }],
        audio_tracks: vec![
            TrackEntry {
                id: 1,
                label: "Stereo".into(),
            },
            TrackEntry {
                id: 2,
                label: "Ambisonic".into(),
            },
        ],
        selected_audio: Some(1),
        script: Some(ScriptStatus {
            name: "video.funscript".into(),
            device: Some("The Handy".into()),
            enabled: true,
            heat: (0..100).map(|i| i as f32 / 100.0).collect(),
        }),
        preview: Some((fp_core::draw::TextureId::Image(99), [0.0, 0.0, 0.1, 0.1])),
        ..Default::default()
    }
}

#[test]
fn player_controls_snapshot_and_actions() {
    let mut ui = Ui::new(Vec2::new(1800.0, 700.0), 1000.0);
    ui.set_record_widgets(true);
    let view = player_view();
    let mut pc = PlayerControls::new();
    let show = |s: &mut PlayerControls, ui: &mut Ui| s.show(ui, &view);
    let (_, out) = run(&mut ui, idle(), |ui| show(&mut pc, ui));
    assert_snapshot(&out, &ui);

    let a = click(&mut ui, &mut pc, "Pause", show);
    assert!(a.contains(&UiAction::TogglePlay), "{a:?}");
    let a = click(&mut ui, &mut pc, "SeekForward", show);
    assert!(a.contains(&UiAction::SeekRelative(10.0)));
    let a = click(&mut ui, &mut pc, "SkipForward", show);
    assert!(a.contains(&UiAction::Seek(MediaTime::from_millis(200_000))));
    let a = click(&mut ui, &mut pc, "B", show);
    assert!(a.contains(&UiAction::SetLoopB(view.position)));
    let a = click(&mut ui, &mut pc, "Recenter", show);
    assert!(a.contains(&UiAction::Recenter));
    let a = click(&mut ui, &mut pc, "Adjust", show);
    assert!(a.contains(&UiAction::OpenPictureAdjust));
    let a = click(&mut ui, &mut pc, "Haptics", show);
    assert!(a.contains(&UiAction::SetScriptEnabled(false)));

    // Speed menu.
    click(&mut ui, &mut pc, "1×", show);
    run(&mut ui, idle(), |ui| show(&mut pc, ui));
    let a = click(&mut ui, &mut pc, "1.5×", show);
    assert!(a.contains(&UiAction::SetSpeed(1.5)), "{a:?}");
    assert!(pc.menu.is_none());
    // Projection menu: preset + stereo.
    click(&mut ui, &mut pc, "180° SBS", show);
    run(&mut ui, idle(), |ui| show(&mut pc, ui));
    let a = click(&mut ui, &mut pc, "OU", show);
    assert!(a.contains(&UiAction::SetStereo(StereoMode::Ou)), "{a:?}");
    let a = click(&mut ui, &mut pc, "360°", show);
    assert!(
        a.contains(&UiAction::SetProjection(Projection::EQUIRECT_360)),
        "{a:?}"
    );
    // Subtitles: Off / English.
    click(&mut ui, &mut pc, "Subtitles", show);
    run(&mut ui, idle(), |ui| show(&mut pc, ui));
    let a = click(&mut ui, &mut pc, "English", show);
    assert!(a.contains(&UiAction::SelectSubtitle(Some(3))), "{a:?}");

    // Timeline: hover requests a preview; drag scrubs; release seeks.
    run(&mut ui, idle(), |ui| show(&mut pc, ui));
    let track = ui.find_widget("timeline").unwrap().rect;
    let y = track.center().y;
    let (a, out) = run(
        &mut ui,
        laser(Vec2::new(track.x + track.w * 0.5, y), false),
        |ui| show(&mut pc, ui),
    );
    assert!(
        a.contains(&UiAction::RequestPreview(MediaTime::from_millis(300_000))),
        "{a:?}"
    );
    validate(&out.draw_list).unwrap();
    let (a, _) = run(
        &mut ui,
        laser(Vec2::new(track.x + track.w * 0.5, y), true),
        |ui| show(&mut pc, ui),
    );
    assert!(a.iter().any(|x| matches!(x, UiAction::Scrub(_))));
    let (a, _) = run(
        &mut ui,
        laser(Vec2::new(track.x + track.w * 0.25, y + 200.0), true),
        |ui| show(&mut pc, ui),
    );
    let scrub = a
        .iter()
        .find_map(|x| {
            if let UiAction::Scrub(t) = x {
                Some(*t)
            } else {
                None
            }
        })
        .unwrap();
    assert!((scrub.as_secs_f64() - 150.0).abs() < 1.0, "{scrub}");
    let (a, _) = run(
        &mut ui,
        laser(Vec2::new(track.x + track.w * 0.25, y + 200.0), false),
        |ui| show(&mut pc, ui),
    );
    let seek = a
        .iter()
        .find_map(|x| {
            if let UiAction::Seek(t) = x {
                Some(*t)
            } else {
                None
            }
        })
        .expect("seek on release");
    assert!((seek.as_secs_f64() - 150.0).abs() < 1.0);

    // B without an open menu goes back.
    let inp = FrameInput {
        dt: DT,
        nav: NavInput {
            back: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let (a, _) = run(&mut ui, inp, |ui| show(&mut pc, ui));
    assert!(a.contains(&UiAction::Back));
}

#[test]
fn picture_adjust_slider_emits_corrections() {
    let mut ui = Ui::new(Vec2::new(1200.0, 900.0), 1000.0);
    ui.set_record_widgets(true);
    let view = PictureView {
        corrections: Corrections::default(),
        keyframe_count: 2,
        position: MediaTime::from_millis(5000),
    };
    let mut screen = PictureAdjustScreen::new();
    let show = |s: &mut PictureAdjustScreen, ui: &mut Ui| s.show(ui, &view);
    let (_, out) = run(&mut ui, idle(), |ui| show(&mut screen, ui));
    assert_snapshot(&out, &ui);
    let ipd = ui.find_widget("IPD").unwrap().rect;
    let at = Vec2::new(ipd.right() - 2.0, ipd.center().y);
    run(&mut ui, laser(at, false), |ui| show(&mut screen, ui));
    let (a, _) = run(&mut ui, laser(at, true), |ui| show(&mut screen, ui));
    let c = a
        .iter()
        .find_map(|x| {
            if let UiAction::SetCorrections(c) = x {
                Some(*c)
            } else {
                None
            }
        })
        .expect("corrections changed");
    assert!(c.ipd_deg > 4.5, "{}", c.ipd_deg);
    run(&mut ui, laser(at, false), |ui| show(&mut screen, ui));
    // Tabs switch the slider set.
    click(&mut ui, &mut screen, "Colour", show);
    run(&mut ui, idle(), |ui| show(&mut screen, ui));
    assert!(ui.find_widget("Exposure").is_some());
    let a = click(&mut ui, &mut screen, "Clear keyframes", show);
    assert!(a.contains(&UiAction::ClearKeyframes));
    let a = click(&mut ui, &mut screen, "Reset", show);
    assert!(a.contains(&UiAction::ResetCorrections));
    let a = click(&mut ui, &mut screen, "Keyframe @ 0:05 (2)", show);
    assert!(a.contains(&UiAction::AddKeyframe));
}

#[test]
fn settings_toggles_and_qr() {
    let mut ui = Ui::new(Vec2::new(1400.0, 1000.0), 1000.0);
    ui.set_record_widgets(true);
    let mut model = SettingsModel::default();
    model.general.available_refresh_rates = vec![72.0, 90.0, 120.0];
    model.remote = RemoteSettings {
        api_enabled: true,
        api_port: 8642,
        deovr_port: 23554,
        pairing_url: Some("http://192.168.1.5:8642/pair#t=abc".into()),
        ..Default::default()
    };
    model.haptics.devices = vec![HapticsDevice {
        id: "h1".into(),
        name: "The Handy".into(),
        backend: "handy".into(),
        connected: false,
    }];
    model.updates = UpdateSettings {
        current_version: "0.1.0".into(),
        available: Some("0.2.0".into()),
        ..Default::default()
    };
    let mut screen = SettingsScreen::new();

    let m = model.clone();
    let show = move |s: &mut SettingsScreen, ui: &mut Ui| s.show(ui, &m);
    let (_, out) = run(&mut ui, idle(), |ui| show(&mut screen, ui));
    assert_snapshot(&out, &ui);
    let a = click(&mut ui, &mut screen, "Lying-down mode", show.clone());
    let changed = a
        .iter()
        .find_map(|x| {
            if let UiAction::SettingsChanged(m) = x {
                Some(m.clone())
            } else {
                None
            }
        })
        .expect("changed");
    assert!(changed.general.lying_down);

    // Remote tab draws a QR code: many black quads.
    click(&mut ui, &mut screen, "Remote", show.clone());
    let (_, out) = run(&mut ui, idle(), |ui| show(&mut screen, ui));
    let black = out
        .draw_list
        .vertices
        .iter()
        .filter(|v| v.color == [0.0, 0.0, 0.0, 1.0])
        .count();
    assert!(black > 4 * 50, "QR modules drawn: {black}");
    let a = click(&mut ui, &mut screen, "New token", show.clone());
    assert!(a.contains(&UiAction::RegenerateApiToken));

    click(&mut ui, &mut screen, "Haptics", show.clone());
    run(&mut ui, idle(), |ui| show(&mut screen, ui));
    let a = click(&mut ui, &mut screen, "Connect", show.clone());
    assert!(a.contains(&UiAction::ConnectHapticsDevice("h1".into())));
    let a = click(&mut ui, &mut screen, "Scan", show.clone());
    assert!(a.contains(&UiAction::ScanHapticsDevices));

    click(&mut ui, &mut screen, "Updates", show.clone());
    run(&mut ui, idle(), |ui| show(&mut screen, ui));
    let a = click(&mut ui, &mut screen, "Install", show.clone());
    assert!(a.contains(&UiAction::InstallUpdate));
}

#[test]
fn panels_at_other_densities_stay_valid() {
    // Smaller / denser panels must still produce valid geometry.
    for (size, ppm) in [
        (Vec2::new(800.0, 450.0), 600.0),
        (Vec2::new(3000.0, 1600.0), 1600.0),
    ] {
        let mut ui = Ui::new(size, ppm);
        let view = library_view();
        let mut s = LibraryScreen::new();
        let (_, out) = run(&mut ui, idle(), |ui| s.show(ui, &view));
        validate(&out.draw_list).unwrap();
        let pv = player_view();
        let mut pc = PlayerControls::new();
        let (_, out) = run(&mut ui, idle(), |ui| pc.show(ui, &pv));
        validate(&out.draw_list).unwrap();
    }
}
