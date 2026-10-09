//! Playback UI: the control bar, the adjustments panel and subtitles.

use super::theme::Weight;
use super::widgets::{Kind, Tip};
use super::{Action, View, depth, fmt_time, icons, premium, theme, widgets};
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
    // The bar floats in its panel: the outer margin leaves room for its
    // shadow on the video.
    let inner = egui::Margin::symmetric(28, 18);
    let frame = egui::Frame::new()
        .outer_margin(BAR_SHADOW_ROOM)
        .inner_margin(inner);
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        let Some(pb) = v.playback.as_deref_mut() else {
            return;
        };
        bar_body(ui.painter(), ui.max_rect() + inner);
        let duration = pb.player.duration();
        let position = v.state.scrub.unwrap_or_else(|| pb.player.position());
        let state = pb.player.state();

        // Title, status, format.
        ui.horizontal(|ui| {
            ui.set_height(30.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // The format opens the adjust panel's Format tab (again
                // closes it).
                if format_pill(ui, &widgets::format_short(&pb.format))
                    .tip("Change the format")
                    .clicked()
                {
                    let showing = v.state.adjust_open && v.state.adjust_tab == 0;
                    v.state.adjust_open = !showing;
                    v.state.adjust_tab = 0;
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let message = match pb.player.error() {
                        Some(e) => Some((e, theme::ERROR)),
                        None => pb.notice.clone().map(|n| (n, theme::WARN)),
                    };
                    // With a message, the title leaves it most of the row;
                    // both truncate rather than run under the format pill.
                    let title_width = if message.is_some() {
                        ui.available_width() * 0.35
                    } else {
                        ui.available_width()
                    };
                    ui.scope(|ui| {
                        ui.set_max_width(title_width);
                        ui.add(
                            egui::Label::new(
                                RichText::new(widgets::display_title(&pb.title))
                                    .font(theme::font(Weight::SemiBold, 21.0))
                                    .color(theme::TEXT),
                            )
                            .truncate(),
                        );
                    });
                    if state == PlayerState::Buffering {
                        ui.spinner();
                    }
                    if let Some((text, color)) = message {
                        ui.add(
                            egui::Label::new(
                                RichText::new(text)
                                    .font(theme::font(Weight::Regular, 15.0))
                                    .color(color),
                            )
                            .truncate(),
                        );
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
        widgets::well(&mut left, |ui| {
            if widgets::key(ui, icons::SQUARES_FOUR, 52.0, false)
                .tip("Library")
                .clicked()
            {
                v.actions.push(Action::ShowBrowser(true));
            }
            let speed = pb.player.speed();
            if widgets::pill_key(ui, &fmt_speed(speed), (speed - 1.0).abs() > 1e-3)
                .tip("Speed")
                .clicked()
            {
                v.actions.push(Action::SetSpeed(next_speed(speed)));
            }
            let vol = pb.player.volume();
            let speaker = if vol <= 0.001 {
                icons::SPEAKER_X
            } else if vol < 0.5 {
                icons::SPEAKER_LOW
            } else {
                icons::SPEAKER_HIGH
            };
            if widgets::key(ui, speaker, 52.0, false)
                .tip(if vol <= 0.001 { "Unmute" } else { "Mute" })
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
            if widgets::slider(ui, &mut vol_edit, 0.0..=1.5, 130.0).changed() {
                v.actions.push(Action::SetVolume(vol_edit));
            }
            ui.add_space(6.0);
        });

        let centre_w = 56.0 + 16.0 + 72.0 + 16.0 + 56.0;
        let centre = egui::Rect::from_center_size(row.center(), Vec2::new(centre_w, row.height()));
        let mut mid = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(centre)
                .layout(Layout::left_to_right(Align::Center)),
        );
        mid.spacing_mut().item_spacing.x = 16.0;
        if skip_button(&mut mid, -step).tip("Rewind").clicked() {
            v.actions.push(Action::SeekRelative(-step));
        }
        if play_button(&mut mid, paused)
            .tip(if paused { "Play" } else { "Pause" })
            .clicked()
        {
            v.actions.push(Action::TogglePause);
        }
        if skip_button(&mut mid, step).tip("Forward").clicked() {
            v.actions.push(Action::SeekRelative(step));
        }

        let mut right = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(row)
                .layout(Layout::right_to_left(Align::Center)),
        );
        widgets::well(&mut right, |ui| {
            if widgets::key(ui, icons::X, 52.0, false)
                .tip("Close")
                .clicked()
            {
                v.actions.push(Action::ClosePlayback);
            }
            if widgets::key(ui, icons::EYE_SLASH, 52.0, false)
                .tip("Hide")
                .clicked()
            {
                v.actions.push(Action::HideControls);
            }
            // Your room only shows around flat videos.
            if v.passthrough_available
                && pb.format.projection == Projection::Flat
                && widgets::key(ui, icons::ARMCHAIR, 52.0, v.settings.passthrough)
                    .tip("Room")
                    .clicked()
            {
                v.actions.push(Action::TogglePassthrough);
            }
            if widgets::key(ui, icons::CROSSHAIR, 52.0, false)
                .tip("Recenter")
                .clicked()
            {
                v.actions.push(Action::Recenter);
            }
            if widgets::key(ui, icons::BOOKMARK_SIMPLE, 52.0, false)
                .tip("Bookmark")
                .clicked()
            {
                v.actions.push(Action::AddBookmark);
            }
            if widgets::key(ui, icons::SLIDERS_HORIZONTAL, 52.0, v.state.adjust_open)
                .tip("Adjust")
                .clicked()
            {
                v.state.adjust_open = !v.state.adjust_open;
            }
        });
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

/// Room round the bar in its panel for the shadow it casts.
const BAR_SHADOW_ROOM: egui::Margin = egui::Margin {
    left: 20,
    right: 20,
    top: 8,
    bottom: 28,
};

/// The bar itself: a raised slab, lit along its top edge, casting a soft
/// shadow on the video.
fn bar_body(p: &egui::Painter, rect: egui::Rect) {
    let radius = 28.0;
    p.add(
        egui::epaint::Shadow {
            offset: [0, 12],
            blur: 30,
            spread: 0,
            color: Color32::from_black_alpha(140),
        }
        .as_shape(rect, egui::CornerRadius::same(radius as u8)),
    );
    depth::raised(
        p,
        rect,
        radius,
        depth::Raised {
            top: Color32::from_rgb(31, 38, 48),
            bottom: Color32::from_rgb(15, 19, 25),
            light: Color32::from_white_alpha(30),
            lift: 0.6,
            glow: None,
        },
    );
    depth::edge(
        p,
        rect,
        radius,
        2.0,
        Color32::from_black_alpha(140),
        depth::Side::Bottom,
        false,
    );
}

/// A small raised pill naming the video's format.
fn format_pill(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let g = ui.painter().layout_no_wrap(
        text.to_string(),
        theme::font(Weight::SemiBold, 14.0),
        theme::TEXT_2,
    );
    let (rect, resp) = ui.allocate_exact_size(g.size() + Vec2::new(20.0, 10.0), Sense::click());
    let t = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    depth::raised(
        ui.painter(),
        rect,
        8.0,
        depth::Raised {
            top: Color32::from_rgb(46, 54, 65).lerp_to_gamma(Color32::from_rgb(62, 72, 86), t),
            bottom: Color32::from_rgb(33, 40, 49).lerp_to_gamma(Color32::from_rgb(44, 52, 63), t),
            light: Color32::from_white_alpha(26),
            lift: 0.3,
            glow: None,
        },
    );
    let fg = theme::TEXT_2.lerp_to_gamma(theme::TEXT, t);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    resp
}

/// The big round play / pause button: a glossy accent dome that glows on
/// the bar.
fn play_button(ui: &mut egui::Ui, paused: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(72.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let t = ui
            .ctx()
            .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
        let down = resp.is_pointer_button_down_on();
        let r = 34.0 + t * 2.0 - if down { 2.0 } else { 0.0 };
        let c = rect.center() + Vec2::new(0.0, if down { 0.5 } else { -1.0 * t });
        let dome = egui::Rect::from_center_size(c, Vec2::splat(r * 2.0));
        let lit = |a: Color32, b: Color32| a.lerp_to_gamma(b, t);
        depth::raised(
            ui.painter(),
            dome,
            r,
            depth::Raised {
                top: lit(Color32::from_rgb(104, 198, 255), Color32::from_rgb(140, 214, 255)),
                bottom: lit(Color32::from_rgb(14, 106, 190), Color32::from_rgb(22, 128, 222)),
                light: Color32::from_white_alpha(150),
                lift: if down { 0.3 } else { 0.9 },
                glow: Some(theme::ACCENT.gamma_multiply(0.45 + 0.25 * t)),
            },
        );
        if paused {
            widgets::play_mark(ui.painter(), c, 28.0, Color32::WHITE);
        } else {
            widgets::pause_mark(ui.painter(), c, 26.0, Color32::WHITE);
        }
    }
    resp
}

/// Seek back or forward by `secs`: a raised key with a circular arrow and
/// the seconds in it.
fn skip_button(ui: &mut egui::Ui, secs: f64) -> egui::Response {
    let icon = if secs < 0.0 {
        icons::ARROW_COUNTER_CLOCKWISE
    } else {
        icons::ARROW_CLOCKWISE
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(56.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let (face, fg) = widgets::key_face(ui, &resp, 56.0, false);
        let c = face.center();
        let p = ui.painter();
        p.text(c, egui::Align2::CENTER_CENTER, icon, theme::icon(32.0), fg);
        p.text(
            c + Vec2::new(0.0, 1.0),
            egui::Align2::CENTER_CENTER,
            format!("{}", secs.abs().round() as i64),
            theme::font(Weight::Bold, 10.0),
            fg,
        );
    }
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
    let h = 8.0 + t * 4.0;
    let track = egui::Rect::from_center_size(rect.center(), Vec2::new(rect.width(), h));
    let round = egui::CornerRadius::same((h / 2.0) as u8);
    depth::sunken(painter, track, h / 2.0, widgets::WELL);
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
    widgets::rail_fill(painter, track, done);
    for (m, _) in &pb.markers {
        let x = to_x((*m / duration).clamp(0.0, 1.0) as f32);
        painter.circle_filled(egui::pos2(x, track.center().y), 3.0, theme::WARN);
    }
    let knob = egui::pos2(to_x(frac), track.center().y);
    widgets::knob(painter, knob, 8.0 + t * 4.0);

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

const TABS: [(&str, &str); 7] = [
    (icons::FRAME_CORNERS, "Format"),
    (icons::ARROWS_OUT_CARDINAL, "Position"),
    (icons::CUBE, "Stereo"),
    (icons::SUN, "Picture"),
    (icons::EYEGLASSES, "Passthrough"),
    (icons::SUBTITLES, "Audio & subs"),
    (icons::VIBRATE, "Haptics"),
];

/// Format, view adjustments, tracks and haptics for the open video.
pub fn adjust_panel(ctx: &egui::Context, v: &mut View) {
    let frame = egui::Frame::new()
        .fill(theme::BG)
        .stroke(egui::Stroke::new(1.0_f32, theme::STROKE))
        .corner_radius(28)
        .inner_margin(egui::Margin::same(22));
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        let Some(pb) = v.playback.as_deref_mut() else {
            return;
        };
        ui.label(
            RichText::new("Adjust")
                .font(theme::font(Weight::Bold, 26.0))
                .color(theme::TEXT),
        );
        ui.add_space(4.0);
        if let Some(i) = widgets::segmented(ui, &TABS, v.state.adjust_tab) {
            v.state.adjust_tab = i;
        }
        ui.add_space(10.0);
        // Save and reset stay at the bottom while the page scrolls.
        let footer = v.state.adjust_tab <= 3 || (v.state.adjust_tab == 4 && v.unlocked);
        let footer_h = if footer { 64.0 } else { 0.0 };
        let page = ui.available_height() - footer_h;
        egui::ScrollArea::vertical()
            .max_height(page)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 10.0;
                let before = pb.settings;
                let s = &mut pb.settings;
                let d = ViewSettings::default();
                match v.state.adjust_tab {
                    0 => {
                        let user = pb.evidence == fp_core::format::Evidence::User;
                        widgets::section_label(ui, "Format");
                        ui.label(
                            RichText::new(format!(
                                "Detected {} ({})",
                                pb.detected.label(),
                                pb.evidence.label()
                            ))
                            .color(theme::TEXT_2),
                        );
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                            if widgets::chip_icon(ui, Some(icons::MAGIC_WAND), "Automatic", !user)
                                .clicked()
                            {
                                v.actions.push(Action::SetFormat(None));
                            }
                            for (label, f) in format_choices() {
                                // The chips set the layout; a packed mask stays.
                                if widgets::chip(ui, label, user && pb.format.same_layout(&f))
                                    .clicked()
                                {
                                    v.actions.push(Action::SetFormat(Some(VideoFormat {
                                        alpha_packed: pb.format.alpha_packed,
                                        ..f
                                    })));
                                }
                            }
                        });
                        if let Projection::Fisheye { fov } = pb.format.projection {
                            ui.add_space(6.0);
                            widgets::section_label(ui, "Fisheye lens");
                            widgets::rows(ui, |r| {
                                let mut f = fov;
                                if r.slider(
                                    "Field of view",
                                    None,
                                    &mut f,
                                    150.0..=240.0,
                                    fov,
                                    "°",
                                    0,
                                ) {
                                    v.actions.push(Action::SetFormat(Some(VideoFormat {
                                        projection: Projection::Fisheye { fov: f.round() },
                                        ..pb.format
                                    })));
                                }
                                r.slider("Lens k1", None, &mut s.lens_k1, -0.5..=0.5, 0.0, "", 2);
                                r.slider("Lens k2", None, &mut s.lens_k2, -0.5..=0.5, 0.0, "", 2);
                            });
                        }
                        if pb.format.projection == Projection::Flat {
                            ui.add_space(6.0);
                            widgets::section_label(ui, "Screen");
                            widgets::rows(ui, |r| {
                                r.slider(
                                    "Distance",
                                    None,
                                    &mut s.screen_distance,
                                    1.0..=20.0,
                                    d.screen_distance,
                                    " m",
                                    1,
                                );
                                r.slider(
                                    "Width",
                                    None,
                                    &mut s.screen_width,
                                    0.5..=30.0,
                                    d.screen_width,
                                    " m",
                                    1,
                                );
                                r.slider(
                                    "Curvature",
                                    None,
                                    &mut s.screen_curvature,
                                    0.0..=1.0,
                                    0.0,
                                    "",
                                    2,
                                );
                            });
                        }
                    }
                    1 => {
                        widgets::rows(ui, |r| {
                            r.slider("Yaw", None, &mut s.yaw, -180.0..=180.0, 0.0, "°", 0);
                            r.slider("Pitch", None, &mut s.pitch, -90.0..=90.0, 0.0, "°", 0);
                            r.slider("Roll", None, &mut s.roll, -45.0..=45.0, 0.0, "°", 0);
                            r.slider("Zoom", None, &mut s.zoom, 0.5..=2.5, 1.0, "×", 2);
                            r.slider(
                                "Height",
                                Some("Raise or lower yourself in the scene."),
                                &mut s.height,
                                -1.0..=1.0,
                                d.height,
                                " m",
                                2,
                            );
                            r.slider(
                                "Forward",
                                Some("Move closer to or back from the scene."),
                                &mut s.forward,
                                -1.0..=1.0,
                                d.forward,
                                " m",
                                2,
                            );
                        });
                        ui.label(
                            RichText::new(
                                "Or hold a grip and the trigger and drag the picture; \
                                 press the thumbstick to undo. Right grip and thumbstick \
                                 up / down changes your height.",
                            )
                            .color(theme::TEXT_3),
                        );
                    }
                    2 => {
                        widgets::rows(ui, |r| {
                            r.slider(
                                "Depth",
                                Some("Eye separation; lower if it's hard to fuse."),
                                &mut s.ipd_offset,
                                -5.0..=5.0,
                                0.0,
                                "°",
                                1,
                            );
                            r.slider(
                                "Vertical align",
                                None,
                                &mut s.vertical_align,
                                -3.0..=3.0,
                                0.0,
                                "°",
                                1,
                            );
                            r.slider(
                                "Rotation align",
                                None,
                                &mut s.rotation_align,
                                -3.0..=3.0,
                                0.0,
                                "°",
                                1,
                            );
                            r.switch(
                                "Swap eyes",
                                Some("For left/right swapped files."),
                                &mut s.swap_eyes,
                            );
                            r.slider(
                                "Subtitle depth",
                                None,
                                &mut s.subtitle_distance,
                                0.8..=10.0,
                                d.subtitle_distance,
                                " m",
                                1,
                            );
                        });
                    }
                    3 => {
                        widgets::rows(ui, |r| {
                            r.slider(
                                "Brightness",
                                None,
                                &mut s.brightness,
                                -0.5..=0.5,
                                0.0,
                                "",
                                2,
                            );
                            r.slider("Contrast", None, &mut s.contrast, 0.5..=1.5, 1.0, "", 2);
                            r.slider("Saturation", None, &mut s.saturation, 0.0..=2.0, 1.0, "", 2);
                            r.slider("Gamma", None, &mut s.gamma, 0.5..=2.0, 1.0, "", 2);
                            r.slider("Sharpen", None, &mut s.sharpen, 0.0..=1.0, 0.0, "", 2);
                        });
                    }
                    4 if !v.unlocked => unlock_card(ui, v.purchase, v.actions, true),
                    4 => {
                        unlocked_header(
                            ui,
                            "This video's settings, saved with its adjustments.",
                            true,
                            v.state,
                        );
                        chroma_tab(
                            ui,
                            s,
                            pb.format,
                            &v.settings.default_view,
                            v.passthrough_available,
                            v.actions,
                        )
                    }
                    5 => audio_and_text(ui, pb, v.actions),
                    _ => haptics_tab(ui, pb.script_count, v.settings, v.devices),
                }
                if pb.settings != before {
                    pb.settings_dirty = true;
                }
            });
        if footer {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let label = if pb.record_id.is_some() {
                    "Save for this video"
                } else {
                    "Keep for this session"
                };
                if ui
                    .add_enabled_ui(pb.settings_dirty, |ui| {
                        widgets::button(ui, Some(icons::CHECK), label, Kind::Primary)
                    })
                    .inner
                    .clicked()
                {
                    v.actions.push(Action::SaveView);
                }
                if widgets::button(
                    ui,
                    Some(icons::ARROW_COUNTER_CLOCKWISE),
                    "Reset",
                    Kind::Secondary,
                )
                .clicked()
                {
                    v.actions.push(Action::ResetView);
                }
                if widgets::icon_button(ui, icons::TIMER, 48.0, false)
                    .tip("Keyframe")
                    .clicked()
                {
                    v.actions.push(Action::AddKeyframe);
                }
                if !pb.keyframes.frames.is_empty()
                    && widgets::button(
                        ui,
                        None,
                        &format!("Clear {} keyframes", pb.keyframes.frames.len()),
                        Kind::Ghost,
                    )
                    .clicked()
                {
                    v.actions.push(Action::ClearKeyframes);
                }
            });
        }
    });
}

/// Chroma key colours offered as one tap.
const KEY_COLORS: [(&str, [f32; 3]); 3] = [
    ("Green screen", [0.0, 1.0, 0.0]),
    ("Blue screen", [0.08, 0.04, 1.0]),
    ("Magenta", [1.0, 0.0, 1.0]),
];

/// Passthrough tab: the video's own packed mask (`_alpha` videos) or a
/// chroma key, so the room shows where the video's background was. The key
/// follows Settings > Passthrough until it is changed here for this video.
fn chroma_tab(
    ui: &mut egui::Ui,
    s: &mut ViewSettings,
    format: VideoFormat,
    global: &ViewSettings,
    passthrough_available: bool,
    actions: &mut Vec<Action>,
) {
    widgets::rows(ui, |r| {
        let mut packed = format.alpha_packed;
        let desc = if packed && format.alpha_pack_scale().is_none() {
            "Only side-by-side videos can carry a mask: set the format to a side-by-side one."
        } else {
            "For videos with a built-in see-through mask. On by itself for _alpha files."
        };
        if r.switch("Use the video's mask", Some(desc), &mut packed) {
            actions.push(Action::SetFormat(Some(VideoFormat {
                alpha_packed: packed,
                ..format
            })));
        }
        let mut own = s.own_key();
        if r.switch(
            "Own settings for this video",
            Some(if own {
                "Saved with this video. Turn off to use the global settings."
            } else {
                "Using the global settings. Change anything below to adjust this video."
            }),
            &mut own,
        ) {
            if own {
                s.set_key(global);
            }
            s.key_own = Some(own);
            actions.push(Action::SaveKey);
        }
    });
    let mut shown = s.with_global_key(global);
    if chroma_controls(ui, &mut shown, passthrough_available) {
        s.set_key(&shown);
        s.key_own = Some(true);
        actions.push(Action::SaveKey);
    }
}

/// Passthrough videos are a one-time purchase (crate::unlock). Until then
/// this stands in for their controls: the pitch and a gradient unlock
/// button, then the code to enter at yellkell.com/unlock while FramePlayer
/// waits for the payment. `compact` for the adjust panel.
pub(super) fn unlock_card(
    ui: &mut egui::Ui,
    purchase: Option<&crate::unlock::Status>,
    actions: &mut Vec<Action>,
    compact: bool,
) {
    use crate::unlock::{PRICE, Status};
    let text = |s: &str, size: f32, color| {
        RichText::new(s)
            .font(theme::font(Weight::Regular, size))
            .color(color)
    };
    ui.spacing_mut().item_spacing.y = 12.0;
    let pitch = "Remove green and blue screen backgrounds, and play passthrough videos with their own masks, so your room shows around the people in them.";
    premium::header(ui, "Passthrough videos", PRICE, pitch, compact, None);
    match purchase {
        Some(Status::Waiting { code, page, price }) => {
            widgets::card(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(egui::Margin::same(22))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 10.0;
                        ui.label(text("On your phone or computer, go to", 17.0, theme::TEXT_2));
                        ui.label(
                            RichText::new(page.as_str())
                                .font(theme::font(Weight::Bold, 26.0))
                                .color(premium::BLUE),
                        );
                        ui.label(text("and enter this code:", 17.0, theme::TEXT_2));
                        ui.label(
                            RichText::new(code.as_str())
                                .font(theme::font(Weight::Bold, 56.0))
                                .extra_letter_spacing(12.0)
                                .color(theme::TEXT),
                        );
                        let after = format!("Pay {price} there and FramePlayer unlocks by itself a few seconds later. Bought it before? Restore it there with your email.");
                        ui.label(text(&after, 16.0, theme::TEXT_2));
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(18.0).color(premium::VIOLET));
                            ui.label(text("Waiting for payment", 16.0, theme::TEXT_3));
                        });
                        if widgets::button(ui, None, "Cancel", Kind::Ghost).clicked() {
                            actions.push(Action::CancelUnlock);
                        }
                    });
            });
        }
        Some(Status::Starting) => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(18.0).color(premium::VIOLET));
                ui.label(text("Getting a code", 16.0, theme::TEXT_2));
            });
        }
        _ => {
            if let Some(Status::Failed(msg)) = purchase
                && msg != "Cancelled"
            {
                ui.label(text(msg, 16.0, theme::ERROR));
            }
            ui.add_space(4.0);
            if premium::button(ui, &format!("Unlock for {PRICE}")).clicked() {
                actions.push(Action::StartUnlock);
            }
            ui.label(text(
                "One-time purchase at yellkell.com/unlock, paid on your phone or computer.",
                14.0,
                theme::TEXT_3,
            ));
        }
    }
}

/// The header over unlocked passthrough controls; it celebrates for a few
/// seconds after the purchase.
pub(super) fn unlocked_header(
    ui: &mut egui::Ui,
    subtitle: &str,
    compact: bool,
    state: &super::UiState,
) {
    let celebrate = state
        .unlocked_at
        .map(|t| t.elapsed().as_secs_f32())
        .filter(|t| *t < premium::CELEBRATE_SECS);
    let thanks;
    let subtitle = if celebrate.is_some() {
        thanks = format!("Thank you for supporting FramePlayer! {subtitle}");
        thanks.as_str()
    } else {
        subtitle
    };
    premium::header(
        ui,
        "Passthrough videos",
        "Unlocked",
        subtitle,
        compact,
        celebrate,
    );
    ui.add_space(6.0);
}

/// "Remove the background" and its colour and fine-tune controls, for one
/// video or the global default. True when anything changed.
pub(super) fn chroma_controls(
    ui: &mut egui::Ui,
    s: &mut ViewSettings,
    passthrough_available: bool,
) -> bool {
    let before = s.key();
    let d = ViewSettings::default();
    widgets::rows(ui, |r| {
        r.switch(
            "Remove the background",
            Some(if passthrough_available {
                "Makes one colour of the video see-through, so your room shows behind it."
            } else {
                "Makes one colour of the video see-through. This headset doesn't share \
                 passthrough with apps, so it turns dark instead."
            }),
            &mut s.chroma_key,
        );
    });
    if s.chroma_key {
        premium::label(ui, "Background colour");
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
            for (label, c) in KEY_COLORS {
                let on = s.key_color.iter().zip(c).all(|(a, b)| (a - b).abs() < 0.02);
                if premium::swatch(ui, label, c, on).clicked() {
                    s.key_color = c;
                }
            }
        });
        widgets::rows(ui, |r| {
            r.row("Colour", None, |ui| {
                let [cr, cg, cb] = s.key_color.map(|c| (c.clamp(0.0, 1.0) * 255.0) as u8);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(64.0, 32.0), egui::Sense::hover());
                ui.painter().rect_filled(
                    rect,
                    egui::CornerRadius::same(8),
                    egui::Color32::from_rgb(cr, cg, cb),
                );
            });
            r.slider(
                "Red",
                None,
                &mut s.key_color[0],
                0.0..=1.0,
                d.key_color[0],
                "",
                2,
            );
            r.slider(
                "Green",
                None,
                &mut s.key_color[1],
                0.0..=1.0,
                d.key_color[1],
                "",
                2,
            );
            r.slider(
                "Blue",
                None,
                &mut s.key_color[2],
                0.0..=1.0,
                d.key_color[2],
                "",
                2,
            );
        });
        premium::label(ui, "Fine-tune");
        widgets::rows(ui, |r| {
            r.slider(
                "Similarity",
                Some("How close to the colour a pixel can be and still disappear."),
                &mut s.key_similarity,
                0.0..=1.0,
                d.key_similarity,
                "",
                2,
            );
            r.slider(
                "Edge softness",
                None,
                &mut s.key_smoothness,
                0.0..=0.5,
                d.key_smoothness,
                "",
                2,
            );
            r.slider(
                "Spill removal",
                Some("Takes the background's tint off hair and edges."),
                &mut s.key_spill,
                0.0..=1.0,
                d.key_spill,
                "",
                2,
            );
        });
    }
    s.key() != before
}

fn audio_and_text(ui: &mut egui::Ui, pb: &crate::playback::Playback, actions: &mut Vec<Action>) {
    let info = pb.player.info();
    widgets::section_label(ui, "Audio");
    let current = pb.player.audio_stream();
    let streams: Vec<_> = info.audio_streams().collect();
    if streams.is_empty() {
        ui.label(RichText::new("No audio").color(theme::TEXT_3));
    } else {
        widgets::rows(ui, |r| {
            for s in &streams {
                let mut title = s
                    .title
                    .clone()
                    .or_else(|| s.language.clone())
                    .unwrap_or_else(|| format!("Track {}", s.index));
                if s.ambisonic {
                    title += " · spatial";
                }
                let desc = format!("{} · {} ch", s.codec, s.channels);
                r.row(&title, Some(desc.as_str()), |ui| {
                    if radio(ui, current == Some(s.index)).clicked() {
                        actions.push(Action::SelectAudio(s.index));
                    }
                });
            }
        });
    }
    widgets::section_label(ui, "Subtitles");
    let sub_stream = pb.player.subtitle_stream();
    let off = sub_stream.is_none() && pb.active_subtitle_file.is_none();
    widgets::rows(ui, |r| {
        r.row("Off", None, |ui| {
            if radio(ui, off).clicked() {
                actions.push(Action::SelectSubtitleFile(None));
                actions.push(Action::SelectSubtitleStream(None));
            }
        });
        for (i, (name, _)) in pb.subtitle_files.iter().enumerate() {
            r.row(name, Some("File"), |ui| {
                if radio(ui, pb.active_subtitle_file == Some(i)).clicked() {
                    actions.push(Action::SelectSubtitleFile(Some(i)));
                }
            });
        }
        for s in info.subtitle_streams() {
            let title = s
                .title
                .clone()
                .or_else(|| s.language.clone())
                .unwrap_or_else(|| format!("Track {}", s.index));
            let desc = format!("Track {} · {}", s.index, s.codec);
            r.row(&title, Some(desc.as_str()), |ui| {
                let on = sub_stream == Some(s.index) && pb.active_subtitle_file.is_none();
                if radio(ui, on).clicked() {
                    actions.push(Action::SelectSubtitleFile(None));
                    actions.push(Action::SelectSubtitleStream(Some(s.index)));
                }
            });
        }
    });
    if !pb.markers.is_empty() {
        widgets::section_label(ui, "Chapters and bookmarks");
        widgets::rows(ui, |r| {
            for (t, name) in &pb.markers {
                r.row(name, Some(fmt_time(*t).as_str()), |ui| {
                    if widgets::icon_button(ui, icons::PLAY, 40.0, false)
                        .tip("Go")
                        .clicked()
                    {
                        actions.push(Action::Seek(*t));
                    }
                });
            }
        });
    }
    let st = pb.player.stats();
    ui.label(
        RichText::new(format!(
            "Video: {}{} · dropped {} of {} · stalled {} · Audio: {} via {}",
            st.video_decoder,
            if st.hardware { " (hardware)" } else { "" },
            st.frames_dropped,
            st.frames_shown + st.frames_dropped,
            st.stalls,
            st.audio_decoder,
            st.audio_sink
        ))
        .font(theme::font(Weight::Regular, 14.0))
        .color(theme::TEXT_3),
    );
}

/// A round radio mark, filled when chosen; the whole row reads as the choice.
fn radio(ui: &mut egui::Ui, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
    let t = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let p = ui.painter();
    p.circle_filled(
        rect.center(),
        18.0,
        Color32::from_white_alpha((t * 18.0) as u8),
    );
    if on {
        p.circle_filled(rect.center(), 11.0, theme::ACCENT);
        p.circle_filled(rect.center(), 4.5, Color32::WHITE);
    } else {
        p.circle_stroke(
            rect.center(),
            10.0,
            egui::Stroke::new(2.0_f32, theme::TEXT_3.lerp_to_gamma(Color32::WHITE, t)),
        );
    }
    resp
}

fn haptics_tab(
    ui: &mut egui::Ui,
    scripts: usize,
    settings: &mut crate::settings::Settings,
    devices: &[fp_haptics::DeviceStatus],
) {
    ui.label(
        RichText::new(if scripts == 0 {
            "No haptic script for this video.".to_string()
        } else {
            format!("{scripts} script axis(es) loaded.")
        })
        .color(theme::TEXT_2),
    );
    let mut offset = settings.haptics.offset_ms as f32;
    let l0 = settings
        .haptics
        .axes
        .entry(fp_haptics::Axis::L0)
        .or_default();
    let (mut lo, mut hi) = (l0.min * 100.0, l0.max * 100.0);
    let (mut a, mut b, mut o) = (false, false, false);
    let mut invert = l0.invert;
    widgets::rows(ui, |r| {
        o = r.slider(
            "Timing offset",
            None,
            &mut offset,
            -500.0..=500.0,
            0.0,
            " ms",
            0,
        );
        a = r.slider("Stroke bottom", None, &mut lo, 0.0..=100.0, 0.0, "%", 0);
        b = r.slider("Stroke top", None, &mut hi, 0.0..=100.0, 100.0, "%", 0);
        r.switch("Invert stroke", None, &mut invert);
    });
    let l0 = settings
        .haptics
        .axes
        .entry(fp_haptics::Axis::L0)
        .or_default();
    l0.invert = invert;
    if a || b {
        l0.min = (lo / 100.0).min(hi / 100.0 - 0.05).max(0.0);
        l0.max = (hi / 100.0).max(l0.min + 0.05).min(1.0);
    }
    if o {
        settings.haptics.offset_ms = offset.round() as i64;
    }
    widgets::section_label(ui, "Devices");
    if devices.is_empty() {
        ui.label(
            RichText::new("None connected. Add one in Settings › Haptics.").color(theme::TEXT_3),
        );
    } else {
        widgets::rows(ui, |r| {
            for d in devices {
                let desc = if d.connected {
                    "Connected".to_string()
                } else {
                    d.last_error
                        .clone()
                        .unwrap_or_else(|| "Not connected".into())
                };
                r.row(&d.name, Some(desc.as_str()), |ui| {
                    let c = if d.connected { theme::OK } else { theme::ERROR };
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 5.0, c);
                });
            }
        });
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
