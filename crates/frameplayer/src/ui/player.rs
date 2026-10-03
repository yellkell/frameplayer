//! Playback UI: the control bar, the adjustments panel and subtitles.

use super::theme::Weight;
use super::{Action, View, fmt_time, icons, slider_row, theme, widgets};
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

/// The floating transport bar: title and format, the seek bar, and three
/// groups of controls (browse and sound left, transport centred, view
/// right).
pub fn control_bar(ctx: &egui::Context, v: &mut View) {
    let frame = egui::Frame::new()
        .fill(theme::BG)
        .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
        .corner_radius(28)
        .inner_margin(egui::Margin::symmetric(28, 18));
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        let Some(pb) = v.playback.as_deref_mut() else {
            return;
        };
        let duration = pb.player.duration();
        let position = v.state.scrub.unwrap_or_else(|| pb.player.position());
        let state = pb.player.state();

        // Title, status, format.
        ui.horizontal(|ui| {
            ui.set_height(30.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                format_pill(ui, &widgets::format_short(&pb.format));
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(widgets::display_title(&pb.title))
                                .font(theme::font(Weight::SemiBold, 21.0))
                                .color(theme::TEXT),
                        )
                        .truncate(),
                    );
                    if state == PlayerState::Buffering {
                        ui.spinner();
                    }
                    if let Some(e) = pb.player.error() {
                        ui.label(RichText::new(e).color(theme::ERROR));
                    } else if let Some(n) = &pb.notice {
                        ui.label(RichText::new(n).color(theme::WARN));
                    }
                });
            });
        });
        ui.add_space(6.0);

        // Times either side of the seek bar.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            let time = |ui: &mut egui::Ui, t: f64, align: Align| {
                ui.allocate_ui_with_layout(
                    Vec2::new(68.0, 36.0),
                    Layout::left_to_right(Align::Center).with_main_align(align),
                    |ui| {
                        ui.label(
                            RichText::new(fmt_time(t))
                                .font(theme::font(Weight::Medium, 16.0))
                                .color(theme::TEXT_2),
                        )
                    },
                );
            };
            time(ui, position, Align::Min);
            let w = ui.available_width() - 68.0 - 14.0;
            seek_bar(ui, w, v.state, pb, v.actions);
            time(ui, duration, Align::Max);
        });
        ui.add_space(8.0);

        // Controls: three groups on one row, transport always centred.
        let (row, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 72.0), Sense::hover());
        let paused = pb.player.is_paused() || state == PlayerState::Ended;
        let step = v.settings.seek_step;

        let mut left = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(row)
                .layout(Layout::left_to_right(Align::Center)),
        );
        left.spacing_mut().item_spacing.x = 8.0;
        if widgets::icon_button(&mut left, icons::SQUARES_FOUR, 52.0, false)
            .on_hover_text("Library")
            .clicked()
        {
            v.actions.push(Action::ShowBrowser(true));
        }
        let speed = pb.player.speed();
        if widgets::chip(&mut left, &fmt_speed(speed), (speed - 1.0).abs() > 1e-3)
            .on_hover_text("Playback speed")
            .clicked()
        {
            v.actions.push(Action::SetSpeed(next_speed(speed)));
        }
        left.add_space(4.0);
        let vol = pb.player.volume();
        let speaker = if vol <= 0.001 {
            icons::SPEAKER_X
        } else if vol < 0.5 {
            icons::SPEAKER_LOW
        } else {
            icons::SPEAKER_HIGH
        };
        if widgets::icon_button(&mut left, speaker, 48.0, false)
            .on_hover_text(if vol <= 0.001 { "Unmute" } else { "Mute" })
            .clicked()
        {
            if vol <= 0.001 {
                v.actions.push(Action::SetVolume(
                    v.state.unmute_volume.take().unwrap_or(1.0),
                ));
            } else {
                v.state.unmute_volume = Some(vol);
                v.actions.push(Action::SetVolume(0.0));
            }
        }
        let mut vol_edit = vol;
        left.spacing_mut().slider_width = 120.0;
        if left
            .add(egui::Slider::new(&mut vol_edit, 0.0..=1.5).show_value(false))
            .changed()
        {
            v.actions.push(Action::SetVolume(vol_edit));
        }

        let centre_w = 56.0 + 16.0 + 72.0 + 16.0 + 56.0;
        let centre = egui::Rect::from_center_size(row.center(), Vec2::new(centre_w, row.height()));
        let mut mid = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(centre)
                .layout(Layout::left_to_right(Align::Center)),
        );
        mid.spacing_mut().item_spacing.x = 16.0;
        if skip_button(&mut mid, -step).on_hover_text("Back").clicked() {
            v.actions.push(Action::SeekRelative(-step));
        }
        if play_button(&mut mid, paused)
            .on_hover_text(if paused { "Play" } else { "Pause" })
            .clicked()
        {
            v.actions.push(Action::TogglePause);
        }
        if skip_button(&mut mid, step)
            .on_hover_text("Forward")
            .clicked()
        {
            v.actions.push(Action::SeekRelative(step));
        }

        let mut right = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(row)
                .layout(Layout::right_to_left(Align::Center)),
        );
        right.spacing_mut().item_spacing.x = 8.0;
        if widgets::icon_button(&mut right, icons::X, 52.0, false)
            .on_hover_text("Close video")
            .clicked()
        {
            v.actions.push(Action::ClosePlayback);
        }
        if widgets::icon_button(&mut right, icons::EYE_SLASH, 52.0, false)
            .on_hover_text("Hide controls (pull the trigger to bring them back)")
            .clicked()
        {
            v.actions.push(Action::HideControls);
        }
        if v.passthrough_available
            && widgets::icon_button(&mut right, icons::EYEGLASSES, 52.0, v.settings.passthrough)
                .on_hover_text("Passthrough")
                .clicked()
        {
            v.actions.push(Action::TogglePassthrough);
        }
        if widgets::icon_button(&mut right, icons::CROSSHAIR, 52.0, false)
            .on_hover_text("Recenter")
            .clicked()
        {
            v.actions.push(Action::Recenter);
        }
        if widgets::icon_button(&mut right, icons::BOOKMARK_SIMPLE, 52.0, false)
            .on_hover_text("Add bookmark")
            .clicked()
        {
            v.actions.push(Action::AddBookmark);
        }
        if widgets::icon_button(
            &mut right,
            icons::SLIDERS_HORIZONTAL,
            52.0,
            v.state.adjust_open,
        )
        .on_hover_text("Format and adjustments")
        .clicked()
        {
            v.state.adjust_open = !v.state.adjust_open;
        }
    });
}

const SPEEDS: [f64; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];

/// The next speed in [`SPEEDS`], wrapping round.
fn next_speed(speed: f64) -> f64 {
    SPEEDS
        .iter()
        .copied()
        .find(|s| *s > speed + 1e-3)
        .unwrap_or(SPEEDS[0])
}

fn fmt_speed(speed: f64) -> String {
    let s = format!("{speed:.2}");
    format!("{}×", s.trim_end_matches('0').trim_end_matches('.'))
}

/// A quiet pill naming the video's format.
fn format_pill(ui: &mut egui::Ui, text: &str) {
    let g = ui.painter().layout_no_wrap(
        text.to_string(),
        theme::font(Weight::SemiBold, 14.0),
        theme::TEXT_2,
    );
    let (rect, _) = ui.allocate_exact_size(g.size() + Vec2::new(20.0, 10.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(8), theme::SURFACE_2);
    ui.painter()
        .galley(rect.center() - g.size() / 2.0, g, theme::TEXT_2);
}

/// The big round play / pause button.
fn play_button(ui: &mut egui::Ui, paused: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(72.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let t = ui
            .ctx()
            .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
        let down = resp.is_pointer_button_down_on();
        let p = ui.painter();
        let r = 36.0 + t * 2.0 - if down { 2.0 } else { 0.0 };
        widgets::shadow(
            p,
            egui::Rect::from_center_size(rect.center(), Vec2::splat(r * 2.0)),
            (r) as u8,
            0.6,
        );
        p.circle_filled(
            rect.center(),
            r,
            theme::TEXT.lerp_to_gamma(Color32::WHITE, t),
        );
        let icon = if paused { icons::PLAY } else { icons::PAUSE };
        // The play triangle's weight sits left of its box: nudge it right.
        let nudge = if paused {
            Vec2::new(2.5, 0.0)
        } else {
            Vec2::ZERO
        };
        p.text(
            rect.center() + nudge,
            egui::Align2::CENTER_CENTER,
            icon,
            theme::icon_fill(32.0),
            theme::BG,
        );
    }
    resp
}

/// Seek back or forward by `secs`: a circular arrow with the seconds in it.
fn skip_button(ui: &mut egui::Ui, secs: f64) -> egui::Response {
    let icon = if secs < 0.0 {
        icons::ARROW_COUNTER_CLOCKWISE
    } else {
        icons::ARROW_CLOCKWISE
    };
    let resp = widgets::icon_button(ui, "", 56.0, false);
    let c = resp.rect.center();
    let t = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let fg = theme::TEXT.lerp_to_gamma(Color32::WHITE, t);
    let p = ui.painter();
    p.text(c, egui::Align2::CENTER_CENTER, icon, theme::icon(36.0), fg);
    p.text(
        c + Vec2::new(0.0, 1.0),
        egui::Align2::CENTER_CENTER,
        format!("{}", secs.abs().round() as i64),
        theme::font(Weight::Bold, 11.0),
        fg,
    );
    resp
}

fn seek_bar(
    ui: &mut egui::Ui,
    width: f32,
    state: &mut super::UiState,
    pb: &mut crate::playback::Playback,
    actions: &mut Vec<Action>,
) {
    let duration = pb.player.duration().max(0.001);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 36.0), Sense::click_and_drag());
    let active = resp.hovered() || resp.dragged();
    let t = ui.ctx().animate_bool_with_time(resp.id, active, 0.12);
    let painter = ui.painter();
    let h = 6.0 + t * 4.0;
    let track = egui::Rect::from_center_size(rect.center(), Vec2::new(rect.width(), h));
    let round = egui::CornerRadius::same((h / 2.0) as u8);
    painter.rect_filled(track, round, theme::SURFACE_3);
    // Haptic heatmap just above the track.
    if let Some(hm) = &pb.heatmap {
        let n = hm.colors.len().max(1) as f32;
        let w = track.width() / n;
        for (i, c) in hm.colors.iter().enumerate() {
            let x = track.left() + i as f32 * w;
            let r = egui::Rect::from_min_max(
                egui::pos2(x, track.top() - 7.0),
                egui::pos2(x + w + 0.5, track.top() - 3.0),
            );
            painter.rect_filled(r, 0.0, Color32::from_rgb(c[0], c[1], c[2]));
        }
    }
    let pos = state.scrub.unwrap_or_else(|| pb.player.position());
    let frac = (pos / duration).clamp(0.0, 1.0) as f32;
    let to_x = |f: f32| track.left() + track.width() * f;
    // Where the pointer would seek to, previewed in a lighter tone.
    if let Some(p) = resp.hover_pos()
        && !resp.dragged()
    {
        let hx = p.x.clamp(track.left(), track.right());
        if hx > to_x(frac) {
            let ahead = egui::Rect::from_min_max(
                egui::pos2(to_x(frac), track.top()),
                egui::pos2(hx, track.bottom()),
            );
            painter.rect_filled(ahead, round, Color32::from_white_alpha(40));
        }
    }
    let done = egui::Rect::from_min_max(track.min, egui::pos2(to_x(frac), track.bottom()));
    painter.rect_filled(done, round, theme::ACCENT);
    for (m, _) in &pb.markers {
        let x = to_x((*m / duration).clamp(0.0, 1.0) as f32);
        painter.circle_filled(egui::pos2(x, track.center().y), 3.0, theme::WARN);
    }
    let knob = egui::pos2(to_x(frac), track.center().y);
    let r = 7.0 + t * 4.0;
    painter.circle_filled(knob + Vec2::new(0.0, 1.5), r, Color32::from_black_alpha(90));
    painter.circle_filled(knob, r, Color32::WHITE);

    let to_time = |x: f32| ((x - track.left()) / track.width()).clamp(0.0, 1.0) as f64 * duration;
    if let Some(p) = resp.hover_pos().or(resp.interact_pointer_pos()) {
        let tm = to_time(p.x);
        let marker = pb.markers.iter().find(|(m, _)| {
            ((m / duration) as f32 * track.width() - (p.x - track.left())).abs() < 8.0
        });
        let label = match marker {
            Some((_, name)) => format!("{} · {name}", fmt_time(tm)),
            None => fmt_time(tm),
        };
        // A bubble above the track, on the panel's top layer.
        let g = ui
            .painter()
            .layout_no_wrap(label, theme::font(Weight::SemiBold, 15.0), theme::BG);
        let size = g.size() + Vec2::new(16.0, 8.0);
        let x = (p.x - size.x / 2.0).clamp(ui.max_rect().left(), ui.max_rect().right() - size.x);
        let bubble = egui::Rect::from_min_size(egui::pos2(x, track.top() - size.y - 10.0), size);
        let top = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Tooltip,
            egui::Id::new("seek-time"),
        ));
        top.rect_filled(bubble, egui::CornerRadius::same(8), theme::TEXT);
        top.galley(bubble.min + Vec2::new(8.0, 4.0), g, theme::BG);
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
