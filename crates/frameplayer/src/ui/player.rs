//! Playback UI: the control bar, the adjustments panel and subtitles.

use super::{Action, View, big_button, fmt_time, slider_row, theme};
use egui::{Align, Color32, Layout, RichText, Sense, Vec2};
use fp_core::format::{Projection, StereoLayout, VideoFormat};
use fp_core::view::ViewSettings;
use fp_media::PlayerState;

/// Format presets offered for overrides.
pub fn format_choices() -> Vec<(&'static str, VideoFormat)> {
    use StereoLayout::*;
    let f = |p, s| VideoFormat::new(p, s);
    vec![
        ("Flat 2D", f(Projection::Flat, Mono)),
        ("Flat 3D SBS", f(Projection::Flat, SideBySide)),
        ("Flat 3D TB", f(Projection::Flat, TopBottom)),
        ("180° SBS", f(Projection::EQUIRECT_180, SideBySide)),
        ("180° TB", f(Projection::EQUIRECT_180, TopBottom)),
        ("180° 2D", f(Projection::EQUIRECT_180, Mono)),
        ("360° 2D", f(Projection::EQUIRECT_360, Mono)),
        ("360° TB", f(Projection::EQUIRECT_360, TopBottom)),
        ("360° SBS", f(Projection::EQUIRECT_360, SideBySide)),
        (
            "Fisheye 190° SBS",
            f(Projection::fisheye(190.0), SideBySide),
        ),
        (
            "Fisheye 200° SBS",
            f(Projection::fisheye(200.0), SideBySide),
        ),
        (
            "Fisheye 220° SBS",
            f(Projection::fisheye(220.0), SideBySide),
        ),
        (
            "EAC 360° (YouTube)",
            f(Projection::Eac { h_fov: 360.0 }, Mono),
        ),
        (
            "EAC 360° TB",
            f(Projection::Eac { h_fov: 360.0 }, TopBottom),
        ),
        (
            "EAC 180° SBS",
            f(Projection::Eac { h_fov: 180.0 }, SideBySide),
        ),
    ]
}

/// The floating transport bar.
pub fn control_bar(ctx: &egui::Context, v: &mut View) {
    let frame = egui::Frame::new()
        .fill(theme::PANEL_BG)
        .corner_radius(18.0)
        .inner_margin(14.0);
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        let Some(pb) = v.playback.as_deref_mut() else {
            return;
        };
        let duration = pb.player.duration();
        let position = v.state.scrub.unwrap_or_else(|| pb.player.position());
        let state = pb.player.state();
        ui.horizontal(|ui| {
            ui.label(RichText::new(&pb.title).size(21.0).strong());
            if state == PlayerState::Buffering {
                ui.spinner();
            }
            if let Some(e) = pb.player.error() {
                ui.label(RichText::new(e).color(theme::ERROR));
            } else if let Some(n) = &pb.notice {
                ui.label(RichText::new(n).color(theme::WARN));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{} / {}", fmt_time(position), fmt_time(duration)))
                        .monospace()
                        .size(19.0),
                );
                ui.label(RichText::new(pb.format.label()).color(theme::MUTED));
            });
        });
        seek_bar(ui, v.state, pb, v.actions);
        ui.horizontal(|ui| {
            if big_button(ui, "☰", false)
                .on_hover_text("Library")
                .clicked()
            {
                v.actions.push(Action::ShowBrowser(true));
            }
            ui.add_space(8.0);
            let step = v.settings.seek_step;
            if big_button(ui, &format!("⏪ {}", step as i64), false)
                .on_hover_text("Back")
                .clicked()
            {
                v.actions.push(Action::SeekRelative(-step));
            }
            let icon = if pb.player.is_paused() || state == PlayerState::Ended {
                "▶"
            } else {
                "⏸"
            };
            if ui
                .add(
                    egui::Button::new(RichText::new(icon).size(28.0))
                        .min_size(Vec2::new(72.0, 48.0)),
                )
                .clicked()
            {
                v.actions.push(Action::TogglePause);
            }
            if big_button(ui, &format!("{} ⏩", step as i64), false)
                .on_hover_text("Forward")
                .clicked()
            {
                v.actions.push(Action::SeekRelative(step));
            }
            ui.add_space(8.0);
            let speed = pb.player.speed();
            if ui.button("−").clicked() {
                v.actions
                    .push(Action::SetSpeed(((speed - 0.25) * 4.0).round() / 4.0));
            }
            if ui
                .button(format!("{speed:.2}×"))
                .on_hover_text("Reset speed")
                .clicked()
            {
                v.actions.push(Action::SetSpeed(1.0));
            }
            if ui.button("+").clicked() {
                v.actions
                    .push(Action::SetSpeed(((speed + 0.25) * 4.0).round() / 4.0));
            }
            ui.add_space(8.0);
            let mut vol = pb.player.volume();
            ui.label("🔊");
            ui.spacing_mut().slider_width = 120.0;
            if ui
                .add(egui::Slider::new(&mut vol, 0.0..=1.5).show_value(false))
                .changed()
            {
                v.actions.push(Action::SetVolume(vol));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if big_button(ui, "🗙", false)
                    .on_hover_text("Close video")
                    .clicked()
                {
                    v.actions.push(Action::ClosePlayback);
                }
                if v.passthrough_available
                    && big_button(ui, "👓", v.settings.passthrough)
                        .on_hover_text("Passthrough")
                        .clicked()
                {
                    v.actions.push(Action::TogglePassthrough);
                }
                if big_button(ui, "⌖", false)
                    .on_hover_text("Recenter")
                    .clicked()
                {
                    v.actions.push(Action::Recenter);
                }
                if big_button(ui, "🔖", false)
                    .on_hover_text("Add bookmark")
                    .clicked()
                {
                    v.actions.push(Action::AddBookmark);
                }
                if big_button(ui, "⚙", v.state.adjust_open)
                    .on_hover_text("Format and adjustments")
                    .clicked()
                {
                    v.state.adjust_open = !v.state.adjust_open;
                }
            });
        });
    });
}

fn seek_bar(
    ui: &mut egui::Ui,
    state: &mut super::UiState,
    pb: &mut crate::playback::Playback,
    actions: &mut Vec<Action>,
) {
    let duration = pb.player.duration().max(0.001);
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 46.0), Sense::click_and_drag());
    let painter = ui.painter_at(rect.expand(2.0));
    let track = egui::Rect::from_center_size(rect.center(), Vec2::new(rect.width(), 12.0));
    painter.rect_filled(track, 6.0, Color32::from_rgb(50, 55, 68));
    // Haptic heatmap underneath the progress.
    if let Some(h) = &pb.heatmap {
        let n = h.colors.len().max(1) as f32;
        let w = track.width() / n;
        for (i, c) in h.colors.iter().enumerate() {
            let x = track.left() + i as f32 * w;
            let r = egui::Rect::from_min_max(
                egui::pos2(x, track.top() - 6.0),
                egui::pos2(x + w + 0.5, track.top() - 1.0),
            );
            painter.rect_filled(r, 0.0, Color32::from_rgb(c[0], c[1], c[2]));
        }
    }
    let pos = state.scrub.unwrap_or_else(|| pb.player.position());
    let frac = (pos / duration).clamp(0.0, 1.0) as f32;
    let mut done = track;
    done.set_width(track.width() * frac);
    painter.rect_filled(done, 6.0, theme::ACCENT);
    for (t, _) in &pb.markers {
        let x = track.left() + track.width() * (*t / duration).clamp(0.0, 1.0) as f32;
        painter.line_segment(
            [
                egui::pos2(x, track.top() - 2.0),
                egui::pos2(x, track.bottom() + 8.0),
            ],
            egui::Stroke::new(3.0_f32, theme::WARN),
        );
    }
    let knob = egui::pos2(track.left() + track.width() * frac, track.center().y);
    painter.circle_filled(
        knob,
        if resp.hovered() || resp.dragged() {
            13.0
        } else {
            9.0
        },
        Color32::WHITE,
    );
    let to_time = |x: f32| ((x - track.left()) / track.width()).clamp(0.0, 1.0) as f64 * duration;
    if let Some(p) = resp.hover_pos() {
        let t = to_time(p.x);
        let marker = pb.markers.iter().find(|(m, _)| {
            ((m / duration) as f32 * track.width() - (p.x - track.left())).abs() < 8.0
        });
        let label = match marker {
            Some((_, name)) => format!("{} · {name}", fmt_time(t)),
            None => fmt_time(t),
        };
        let galley =
            painter.layout_no_wrap(label, egui::FontId::proportional(16.0), Color32::WHITE);
        let at = egui::pos2(
            (p.x - galley.size().x / 2.0).clamp(rect.left(), rect.right() - galley.size().x),
            rect.bottom() - galley.size().y,
        );
        painter.rect_filled(
            egui::Rect::from_min_size(
                at - Vec2::new(4.0, 1.0),
                galley.size() + Vec2::new(8.0, 2.0),
            ),
            4.0,
            Color32::from_black_alpha(200),
        );
        painter.galley(at, galley, Color32::WHITE);
    }
    if resp.dragged()
        && let Some(p) = resp.interact_pointer_pos()
    {
        state.scrub = Some(to_time(p.x));
    }
    if resp.drag_stopped() {
        if let Some(t) = state.scrub.take() {
            actions.push(Action::Seek(t));
        }
    } else if resp.clicked()
        && let Some(p) = resp.interact_pointer_pos()
    {
        // Snap to a marker when clicking right on it.
        let t = to_time(p.x);
        let snap = pb
            .markers
            .iter()
            .map(|m| m.0)
            .find(|m| ((m - t) / duration).abs() * (track.width() as f64) < 8.0);
        actions.push(Action::Seek(snap.unwrap_or(t)));
    }
}

const TABS: [&str; 6] = [
    "Format",
    "Position",
    "Stereo",
    "Picture",
    "Audio & text",
    "Haptics",
];

/// Format, view adjustments, tracks and haptics for the open video.
pub fn adjust_panel(ctx: &egui::Context, v: &mut View) {
    let frame = egui::Frame::new()
        .fill(theme::PANEL_BG)
        .corner_radius(18.0)
        .inner_margin(16.0);
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        let Some(pb) = v.playback.as_deref_mut() else {
            return;
        };
        ui.horizontal_wrapped(|ui| {
            for (i, t) in TABS.iter().enumerate() {
                if ui
                    .selectable_label(v.state.adjust_tab == i, RichText::new(*t).size(19.0))
                    .clicked()
                {
                    v.state.adjust_tab = i;
                }
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            let before = pb.settings;
            let s = &mut pb.settings;
            let d = ViewSettings::default();
            match v.state.adjust_tab {
                0 => {
                    ui.label(format!(
                        "Detected: {} ({})",
                        pb.detected.label(),
                        pb.evidence.label()
                    ));
                    ui.add_space(4.0);
                    let user = pb.evidence == fp_core::format::Evidence::User;
                    ui.horizontal_wrapped(|ui| {
                        if ui.selectable_label(!user, "Automatic").clicked() {
                            v.actions.push(Action::SetFormat(None));
                        }
                        for (label, f) in format_choices() {
                            if ui.selectable_label(user && pb.format == f, label).clicked() {
                                v.actions.push(Action::SetFormat(Some(f)));
                            }
                        }
                    });
                    ui.add_space(6.0);
                    ui.label(RichText::new("Fisheye field of view").strong());
                    if let Projection::Fisheye { fov } = pb.format.projection {
                        let mut f = fov;
                        if ui
                            .add(
                                egui::Slider::new(&mut f, 150.0..=240.0)
                                    .suffix("°")
                                    .step_by(1.0),
                            )
                            .changed()
                        {
                            v.actions.push(Action::SetFormat(Some(VideoFormat {
                                projection: Projection::Fisheye { fov: f },
                                ..pb.format
                            })));
                        }
                        slider_row(ui, "Lens k1", &mut s.lens_k1, -0.5..=0.5, 0.0, "");
                        slider_row(ui, "Lens k2", &mut s.lens_k2, -0.5..=0.5, 0.0, "");
                    } else {
                        ui.label(RichText::new("Only for fisheye formats.").color(theme::MUTED));
                    }
                    if pb.format.projection == Projection::Flat {
                        ui.add_space(6.0);
                        ui.label(RichText::new("Screen").strong());
                        slider_row(
                            ui,
                            "Distance",
                            &mut s.screen_distance,
                            1.0..=20.0,
                            d.screen_distance,
                            " m",
                        );
                        slider_row(
                            ui,
                            "Width",
                            &mut s.screen_width,
                            0.5..=30.0,
                            d.screen_width,
                            " m",
                        );
                        slider_row(ui, "Curvature", &mut s.screen_curvature, 0.0..=1.0, 0.0, "");
                    }
                }
                1 => {
                    slider_row(ui, "Yaw", &mut s.yaw, -180.0..=180.0, 0.0, "°");
                    slider_row(ui, "Pitch", &mut s.pitch, -90.0..=90.0, 0.0, "°");
                    slider_row(ui, "Roll", &mut s.roll, -45.0..=45.0, 0.0, "°");
                    slider_row(ui, "Zoom", &mut s.zoom, 0.5..=2.5, 1.0, "×");
                    ui.label(
                        RichText::new(
                            "Tip: hold both grips and twist to rotate, or recenter from the bar.",
                        )
                        .color(theme::MUTED),
                    );
                }
                2 => {
                    slider_row(ui, "IPD / depth", &mut s.ipd_offset, -5.0..=5.0, 0.0, "°");
                    slider_row(
                        ui,
                        "Vertical align",
                        &mut s.vertical_align,
                        -3.0..=3.0,
                        0.0,
                        "°",
                    );
                    slider_row(
                        ui,
                        "Rotation align",
                        &mut s.rotation_align,
                        -3.0..=3.0,
                        0.0,
                        "°",
                    );
                    ui.checkbox(&mut s.swap_eyes, "Swap left and right eyes");
                    slider_row(
                        ui,
                        "Subtitle depth",
                        &mut s.subtitle_distance,
                        0.8..=10.0,
                        d.subtitle_distance,
                        " m",
                    );
                }
                3 => {
                    slider_row(ui, "Brightness", &mut s.brightness, -0.5..=0.5, 0.0, "");
                    slider_row(ui, "Contrast", &mut s.contrast, 0.5..=1.5, 1.0, "");
                    slider_row(ui, "Saturation", &mut s.saturation, 0.0..=2.0, 1.0, "");
                    slider_row(ui, "Gamma", &mut s.gamma, 0.5..=2.0, 1.0, "");
                    slider_row(ui, "Sharpen", &mut s.sharpen, 0.0..=1.0, 0.0, "");
                }
                4 => audio_and_text(ui, pb, v.actions),
                _ => haptics_tab(ui, pb.script_count, v.settings, v.devices),
            }
            if pb.settings != before {
                pb.settings_dirty = true;
            }
            if v.state.adjust_tab <= 3 {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let label = if pb.record_id.is_some() {
                        "Save for this video"
                    } else {
                        "Keep for this session"
                    };
                    if ui
                        .add_enabled(pb.settings_dirty, egui::Button::new(label))
                        .clicked()
                    {
                        v.actions.push(Action::SaveView);
                    }
                    if ui.button("Reset").clicked() {
                        v.actions.push(Action::ResetView);
                    }
                    if ui
                        .button("Keyframe here")
                        .on_hover_text("Settings change smoothly between keyframes")
                        .clicked()
                    {
                        v.actions.push(Action::AddKeyframe);
                    }
                    if !pb.keyframes.frames.is_empty()
                        && ui
                            .button(format!("Clear {} keyframes", pb.keyframes.frames.len()))
                            .clicked()
                    {
                        v.actions.push(Action::ClearKeyframes);
                    }
                });
            }
        });
    });
}

fn audio_and_text(ui: &mut egui::Ui, pb: &crate::playback::Playback, actions: &mut Vec<Action>) {
    let info = pb.player.info();
    ui.label(RichText::new("Audio").strong());
    let current = pb.player.audio_stream();
    let mut any = false;
    for s in info.audio_streams() {
        any = true;
        let mut label = format!("#{} {}", s.index, s.codec);
        if let Some(l) = &s.language {
            label += &format!(" [{l}]");
        }
        if let Some(t) = &s.title {
            label += &format!(" {t}");
        }
        label += &format!(" {} ch", s.channels);
        if s.ambisonic {
            label += " · spatial";
        }
        if ui
            .selectable_label(current == Some(s.index), label)
            .clicked()
        {
            actions.push(Action::SelectAudio(s.index));
        }
    }
    if !any {
        ui.label(RichText::new("No audio").color(theme::MUTED));
    }
    ui.add_space(8.0);
    ui.label(RichText::new("Subtitles").strong());
    let sub_stream = pb.player.subtitle_stream();
    let off = sub_stream.is_none() && pb.active_subtitle_file.is_none();
    if ui.selectable_label(off, "Off").clicked() {
        actions.push(Action::SelectSubtitleFile(None));
        actions.push(Action::SelectSubtitleStream(None));
    }
    for (i, (name, _)) in pb.subtitle_files.iter().enumerate() {
        if ui
            .selectable_label(pb.active_subtitle_file == Some(i), format!("File: {name}"))
            .clicked()
        {
            actions.push(Action::SelectSubtitleFile(Some(i)));
        }
    }
    for s in info.subtitle_streams() {
        let mut label = format!("Track #{} {}", s.index, s.codec);
        if let Some(l) = &s.language {
            label += &format!(" [{l}]");
        }
        if let Some(t) = &s.title {
            label += &format!(" {t}");
        }
        if ui
            .selectable_label(
                sub_stream == Some(s.index) && pb.active_subtitle_file.is_none(),
                label,
            )
            .clicked()
        {
            actions.push(Action::SelectSubtitleFile(None));
            actions.push(Action::SelectSubtitleStream(Some(s.index)));
        }
    }
    if !pb.markers.is_empty() {
        ui.add_space(8.0);
        ui.label(RichText::new("Chapters and bookmarks").strong());
        for (t, name) in &pb.markers {
            if ui.button(format!("{}  {name}", fmt_time(*t))).clicked() {
                actions.push(Action::Seek(*t));
            }
        }
    }
    ui.add_space(8.0);
    let st = pb.player.stats();
    ui.label(
        RichText::new(format!(
            "Video: {}{} · dropped {} of {} · Audio: {} via {}",
            st.video_decoder,
            if st.hardware { " (hardware)" } else { "" },
            st.frames_dropped,
            st.frames_shown + st.frames_dropped,
            st.audio_decoder,
            st.audio_sink
        ))
        .small()
        .color(theme::MUTED),
    );
}

fn haptics_tab(
    ui: &mut egui::Ui,
    scripts: usize,
    settings: &mut crate::settings::Settings,
    devices: &[fp_haptics::DeviceStatus],
) {
    if scripts == 0 {
        ui.label(RichText::new("No haptic script for this video.").color(theme::MUTED));
    } else {
        ui.label(format!("{scripts} script axis(es) loaded."));
    }
    let mut offset = settings.haptics.offset_ms as f32;
    if slider_row(ui, "Timing offset", &mut offset, -500.0..=500.0, 0.0, " ms") {
        settings.haptics.offset_ms = offset.round() as i64;
    }
    let l0 = settings
        .haptics
        .axes
        .entry(fp_haptics::Axis::L0)
        .or_default();
    let (mut lo, mut hi) = (l0.min * 100.0, l0.max * 100.0);
    let a = slider_row(ui, "Stroke bottom", &mut lo, 0.0..=100.0, 0.0, "%");
    let b = slider_row(ui, "Stroke top", &mut hi, 0.0..=100.0, 100.0, "%");
    if a || b {
        l0.min = (lo / 100.0).min(hi / 100.0 - 0.05).max(0.0);
        l0.max = (hi / 100.0).max(l0.min + 0.05).min(1.0);
    }
    ui.checkbox(&mut l0.invert, "Invert stroke");
    ui.add_space(6.0);
    ui.label(RichText::new("Devices").strong());
    if devices.is_empty() {
        ui.label(
            RichText::new("None connected. Add one in Settings › Haptics.").color(theme::MUTED),
        );
    }
    for d in devices {
        let (c, s) = if d.connected {
            (theme::OK, "connected")
        } else {
            (theme::ERROR, "disconnected")
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("⚡").color(c));
            ui.label(format!("{} — {s}", d.name));
        });
        if let Some(e) = &d.last_error {
            ui.label(RichText::new(e).small().color(theme::ERROR));
        }
    }
}

/// Subtitle cues, drawn large with an outline on a transparent panel.
pub fn subtitles(
    ctx: &egui::Context,
    cues: &[fp_media::subtitle::Cue],
    textures: &mut Vec<egui::TextureHandle>,
) {
    let frame = egui::Frame::new().fill(Color32::TRANSPARENT);
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        let text: Vec<&str> = cues
            .iter()
            .map(|c| c.text.as_str())
            .filter(|t| !t.trim().is_empty())
            .collect();
        textures.clear();
        for c in cues {
            for (i, b) in c.bitmaps.iter().enumerate() {
                if b.width == 0 || b.height == 0 || b.rgba.len() < (b.width * b.height * 4) as usize
                {
                    continue;
                }
                let img = egui::ColorImage::from_rgba_unmultiplied(
                    [b.width as usize, b.height as usize],
                    &b.rgba,
                );
                textures.push(ctx.load_texture(
                    format!("sub{i}"),
                    img,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
            for t in textures.iter() {
                let size = t.size_vec2();
                let scale = (ui.available_width() / size.x).min(1.0);
                ui.image((t.id(), size * scale));
            }
            if text.is_empty() {
                return;
            }
            let joined = text.join("\n");
            let font = egui::FontId::proportional(54.0);
            let galley =
                ui.painter()
                    .layout(joined, font, Color32::WHITE, ui.available_width() - 40.0);
            let (rect, _) =
                ui.allocate_exact_size(galley.size() + Vec2::new(36.0, 16.0), Sense::hover());
            ui.painter()
                .rect_filled(rect, 12.0, Color32::from_black_alpha(150));
            ui.painter()
                .galley(rect.min + Vec2::new(18.0, 8.0), galley, Color32::WHITE);
        });
    });
}
