//! The browser panel: navigation, home, library grid, video details.

use super::theme::{self, Weight};
use super::thumbs::paint_cover_rounded;
use super::widgets::{self, Kind, Tip};
use super::{Action, FormatFilter, HomeRows, Screen, View, fmt_size, fmt_time, icons};
use crate::playback::OpenRequest;
use egui::{Align, Align2, Color32, Layout, RichText, Sense, Vec2};
use fp_library::{MediaRecord, Sort};

/// Thumbnail size of a card.
const CARD: Vec2 = Vec2::new(256.0, 144.0);
/// Space under the thumbnail for the title and facts.
const CARD_TEXT: f32 = 56.0;
const GAP: f32 = 20.0;
/// Page margin left and right of the content.
const MARGIN: i8 = 28;

/// Main browser panel.
pub fn browser(ctx: &egui::Context, v: &mut View) {
    egui::TopBottomPanel::top("nav")
        .exact_height(76.0)
        .frame(
            egui::Frame::new()
                .fill(theme::BG)
                .inner_margin(egui::Margin::symmetric(MARGIN - 6, 0)),
        )
        .show_separator_line(false)
        .show(ctx, |ui| nav(ui, v));
    status_toast(ctx, v);
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::BG)
                .inner_margin(egui::Margin {
                    left: MARGIN,
                    right: MARGIN,
                    top: 8,
                    bottom: 0,
                }),
        )
        .show(ctx, |ui| {
            // A hairline under the navigation.
            let r = ui.max_rect();
            ui.painter().hline(
                (r.left() - MARGIN as f32)..=(r.right() + MARGIN as f32),
                r.top() - 8.0,
                egui::Stroke::new(1.0_f32, theme::STROKE),
            );
            if let Some(id) = v.state.details {
                details(ui, v, id);
                return;
            }
            match v.state.screen {
                Screen::Home => home(ui, v),
                Screen::Library => library(ui, v),
                Screen::Sources => super::sources::sources(ui, v),
                Screen::Settings => super::settings::settings(ui, v),
            }
        });
}

fn nav(ui: &mut egui::Ui, v: &mut View) {
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        // Brand.
        ui.add_space(6.0);
        ui.label(
            RichText::new(icons::PLAY_CIRCLE)
                .font(theme::icon_fill(30.0))
                .color(theme::ACCENT),
        );
        ui.label(
            RichText::new("FramePlayer")
                .font(theme::font(Weight::Bold, 22.0))
                .color(theme::TEXT),
        );
        ui.add_space(28.0);
        for (s, icon, label) in [
            (Screen::Home, icons::HOUSE, "Home"),
            (Screen::Library, icons::SQUARES_FOUR, "Library"),
            (Screen::Sources, icons::HARD_DRIVES, "Sources"),
            (Screen::Settings, icons::GEAR_SIX, "Settings"),
        ] {
            let selected = v.state.screen == s && v.state.details.is_none();
            if widgets::nav_tab(ui, icon, label, selected).clicked() {
                v.state.screen = s;
                v.state.details = None;
                if s == Screen::Home {
                    v.state.home = None;
                }
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if v.playback.is_some() {
                if widgets::button(ui, Some(icons::PLAY), "Back to video", Kind::Primary).clicked()
                {
                    v.actions.push(Action::ShowBrowser(false));
                }
            } else if widgets::icon_button(ui, icons::POWER, 48.0, false)
                .tip("Quit")
                .clicked()
            {
                v.actions.push(Action::Quit);
            }
            if v.passthrough_available
                && widgets::icon_button(ui, icons::ARMCHAIR, 48.0, v.settings.passthrough)
                    .tip("Room")
                    .clicked()
            {
                v.actions.push(Action::TogglePassthrough);
            }
            if widgets::icon_button(ui, icons::CROSSHAIR, 48.0, false)
                .tip("Recenter")
                .clicked()
            {
                v.actions.push(Action::Recenter);
            }
            if let Some(s) = &v.state.worker_status {
                ui.label(
                    RichText::new(s)
                        .font(theme::font(Weight::Regular, 14.0))
                        .color(theme::TEXT_3),
                );
            }
        });
    });
}

/// Opening, an error, or a recent message: a floating pill at the bottom.
fn status_toast(ctx: &egui::Context, v: &mut View) {
    let toast = v
        .state
        .toast
        .as_ref()
        .filter(|(_, t)| t.elapsed().as_secs_f32() < 6.0)
        .map(|(s, _)| s.clone());
    let err = v.state.open_error.clone();
    let opening = v.state.opening.clone();
    if toast.is_none() && err.is_none() && opening.is_none() {
        return;
    }
    egui::Area::new(egui::Id::new("status"))
        .anchor(Align2::CENTER_BOTTOM, Vec2::new(0.0, -24.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme::SURFACE_3)
                .corner_radius(26)
                .inner_margin(egui::Margin::symmetric(20, 10))
                .shadow(egui::Shadow {
                    offset: [0, 8],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(150),
                })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.set_min_height(32.0);
                        if let Some(o) = opening {
                            ui.spinner();
                            ui.label(format!("Opening {}", widgets::display_title(&o)));
                        } else if let Some(e) = err {
                            ui.label(
                                RichText::new(icons::WARNING_CIRCLE)
                                    .font(theme::icon(22.0))
                                    .color(theme::ERROR),
                            );
                            ui.label(format!("Could not open: {e}"));
                            if widgets::button_sized(ui, None, "Dismiss", Kind::Ghost, 36.0)
                                .clicked()
                            {
                                v.state.open_error = None;
                            }
                        } else if let Some(t) = toast {
                            ui.label(
                                RichText::new(icons::INFO)
                                    .font(theme::icon(22.0))
                                    .color(theme::ACCENT_HOVER),
                            );
                            ui.label(t);
                        }
                    });
                });
        });
}

fn thumb_key(r: &MediaRecord) -> Option<String> {
    r.thumbnail
        .as_ref()
        .map(|p| p.display().to_string())
        .or_else(|| r.thumbnail_url.clone())
}

fn request(r: &MediaRecord) -> OpenRequest {
    OpenRequest {
        location: r.location.clone(),
        source_id: (r.source_id != "local"
            && !r.source_id.is_empty()
            && !r.location.starts_with('/'))
        .then(|| r.source_id.clone()),
        entry: None,
        start_at: None,
    }
}

fn open_record(r: &MediaRecord) -> Action {
    Action::Open(request(r))
}

/// Opens `records[i]` with the rest of the list queued for next/previous.
fn open_in(records: &[MediaRecord], i: usize) -> Action {
    Action::OpenList(records.iter().map(request).collect(), i)
}

/// "12:34 · 180° 3D".
fn facts(r: &MediaRecord) -> String {
    let mut f = Vec::new();
    if let Some(d) = r.duration {
        f.push(fmt_time(d));
    }
    f.push(widgets::format_short(&r.effective_format().format));
    f.join("  ·  ")
}

/// Paints a thumbnail (or a placeholder) into `rect` with rounded corners.
fn thumbnail(ui: &egui::Ui, v: &mut View, r: &MediaRecord, rect: egui::Rect, radius: u8) {
    let painter = ui.painter();
    match thumb_key(r).and_then(|k| v.thumbs.get(&k)) {
        Some(tex) => paint_cover_rounded(painter, rect, &tex, radius),
        None => {
            painter.rect_filled(rect, egui::CornerRadius::same(radius), theme::SURFACE_2);
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                icons::FILM_STRIP,
                theme::icon(40.0),
                theme::TEXT_3,
            );
        }
    }
}

/// A thumbnail card; returns (play clicked, details clicked).
fn card(ui: &mut egui::Ui, v: &mut View, r: &MediaRecord) -> (bool, bool) {
    let (rect, resp) = ui.allocate_exact_size(CARD + Vec2::new(0.0, CARD_TEXT), Sense::click());
    if !ui.is_rect_visible(rect) {
        return (false, false);
    }
    let t = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered(), 0.14);
    let base = egui::Rect::from_min_size(rect.min, CARD);
    // Lift: grows a little, casts a shadow, gains a ring.
    let img = egui::Rect::from_center_size(
        base.center() - Vec2::new(0.0, 3.0 * t),
        CARD * (1.0 + 0.04 * t),
    );
    widgets::shadow(ui.painter(), img, 12, t);
    thumbnail(ui, v, r, img, 12);
    let painter = ui.painter();
    widgets::badge(
        painter,
        &widgets::format_short(&r.effective_format().format),
        img.left_top() + Vec2::new(10.0, 10.0),
        Align2::LEFT_TOP,
    );
    if !r.scripts.is_empty() {
        widgets::badge(
            painter,
            icons::WAVEFORM,
            img.right_top() + Vec2::new(-10.0, 10.0),
            Align2::RIGHT_TOP,
        );
    }
    if let Some(p) = r.progress().filter(|p| *p > 0.005) {
        let bar = egui::Rect::from_min_max(
            egui::pos2(img.left() + 12.0, img.bottom() - 10.0),
            egui::pos2(img.right() - 12.0, img.bottom() - 6.0),
        );
        painter.rect_filled(
            bar,
            egui::CornerRadius::same(2),
            Color32::from_black_alpha(140),
        );
        let mut done = bar;
        done.set_width(bar.width() * p as f32);
        painter.rect_filled(done, egui::CornerRadius::same(2), theme::ACCENT);
    }
    if t > 0.01 {
        painter.rect_stroke(
            img,
            egui::CornerRadius::same(12),
            egui::Stroke::new(2.5_f32, Color32::from_white_alpha((t * 230.0) as u8)),
            egui::StrokeKind::Outside,
        );
        // A play glyph in the middle while hovered.
        painter.circle_filled(
            img.center(),
            28.0,
            Color32::from_black_alpha((t * 150.0) as u8),
        );
        widgets::play_mark(
            painter,
            img.center(),
            24.0,
            Color32::from_white_alpha((t * 255.0) as u8),
        );
    }

    // Title and facts.
    let text_top = base.bottom() + 10.0;
    let more = egui::Rect::from_min_size(
        egui::pos2(rect.right() - 36.0, text_top - 4.0),
        Vec2::splat(36.0),
    );
    let title_color = if r.missing {
        theme::TEXT_3
    } else {
        theme::TEXT
    };
    let mut title = widgets::display_title(&r.title);
    if r.favorite {
        title = format!("{}  {title}", icons::HEART);
    }
    let mut job = egui::text::LayoutJob::single_section(
        title,
        egui::TextFormat::simple(theme::font(Weight::SemiBold, 16.0), title_color),
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - 40.0);
    let galley = ui.fonts(|f| f.layout_job(job));
    let painter = ui.painter();
    painter.galley(egui::pos2(rect.left() + 2.0, text_top), galley, title_color);
    painter.text(
        egui::pos2(rect.left() + 2.0, text_top + 24.0),
        Align2::LEFT_TOP,
        facts(r),
        theme::font(Weight::Regular, 14.0),
        theme::TEXT_2,
    );
    let more_resp = ui.interact(more, resp.id.with("more"), Sense::click());
    let m = ui
        .ctx()
        .animate_bool_with_time(more_resp.id, more_resp.hovered(), 0.12);
    let painter = ui.painter();
    painter.circle_filled(
        more.center(),
        18.0,
        Color32::from_white_alpha((m * 20.0) as u8),
    );
    painter.text(
        more.center(),
        Align2::CENTER_CENTER,
        icons::DOTS_THREE,
        theme::icon(24.0),
        theme::TEXT_2.lerp_to_gamma(Color32::WHITE, m.max(t * 0.6)),
    );
    let more_clicked = more_resp.tip("Details").clicked();
    (resp.clicked() && !more_clicked, more_clicked)
}

fn grid(ui: &mut egui::Ui, v: &mut View, records: &[MediaRecord]) {
    let per_row = ((ui.available_width() + GAP) / (CARD.x + GAP))
        .floor()
        .max(1.0) as usize;
    for (ri, row) in records.chunks(per_row).enumerate() {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for (ci, r) in row.iter().enumerate() {
                let (play, more) = card(ui, v, r);
                if play {
                    v.actions.push(open_in(records, ri * per_row + ci));
                }
                if more {
                    v.state.details = Some(r.id);
                }
            }
        });
        ui.add_space(14.0);
    }
}

fn section_title(ui: &mut egui::Ui, title: &str, count: Option<usize>) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title)
                .font(theme::font(Weight::SemiBold, 22.0))
                .color(theme::TEXT),
        );
        if let Some(n) = count {
            ui.label(
                RichText::new(n.to_string())
                    .font(theme::font(Weight::Medium, 16.0))
                    .color(theme::TEXT_3),
            );
        }
    });
    ui.add_space(6.0);
}

fn row(ui: &mut egui::Ui, v: &mut View, title: &str, records: &[MediaRecord]) {
    if records.is_empty() {
        return;
    }
    section_title(ui, title, Some(records.len()));
    egui::ScrollArea::horizontal()
        .id_salt(title)
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                for (i, r) in records.iter().enumerate() {
                    let (play, more) = card(ui, v, r);
                    if play {
                        v.actions.push(open_in(records, i));
                    }
                    if more {
                        v.state.details = Some(r.id);
                    }
                }
            });
        });
    ui.add_space(18.0);
}

/// The featured video at the top of Home: what to resume, else the newest.
fn hero(ui: &mut egui::Ui, v: &mut View, rows: &HomeRows) {
    let (r, caption, resume) = match (rows.continue_watching.first(), rows.recent.first()) {
        (Some(r), _) => (r, "Continue watching", true),
        (None, Some(r)) => (r, "Recently added", false),
        _ => return,
    };
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 300.0), Sense::click());
    thumbnail(ui, v, r, rect, 18);
    let painter = ui.painter();
    // Scrims: the text side and the bottom.
    widgets::hgradient(
        painter,
        egui::Rect::from_min_max(
            rect.min,
            egui::pos2(rect.left() + rect.width() * 0.72, rect.bottom()),
        ),
        Color32::from_rgba_unmultiplied(13, 16, 21, 238),
        Color32::TRANSPARENT,
    );
    widgets::vgradient(
        painter,
        egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 120.0), rect.max),
        Color32::TRANSPARENT,
        Color32::from_rgba_unmultiplied(13, 16, 21, 190),
    );
    // Square scrim corners would show: redraw the rounded frame over them.
    painter.rect_stroke(
        rect.expand(4.0),
        egui::CornerRadius::same(22),
        egui::Stroke::new(8.0_f32, theme::BG),
        egui::StrokeKind::Inside,
    );
    let mut inner = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(40.0, 34.0)))
            .layout(Layout::top_down(Align::Min)),
    );
    inner.set_max_width(rect.width() * 0.55);
    inner.add_space(22.0);
    widgets::section_label(&mut inner, caption);
    inner.add(
        egui::Label::new(
            RichText::new(widgets::display_title(&r.title))
                .font(theme::font(Weight::Bold, 42.0))
                .color(Color32::WHITE),
        )
        .truncate(),
    );
    let mut line = facts(r);
    if resume && r.resume_position > 1.0 {
        let left = r.duration.unwrap_or(0.0) - r.resume_position;
        line = format!("{line}  ·  {} left", fmt_time(left));
    }
    inner.label(
        RichText::new(line)
            .font(theme::font(Weight::Medium, 17.0))
            .color(theme::TEXT_2),
    );
    inner.add_space(18.0);
    inner.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        let label = if resume { "Resume" } else { "Play" };
        if widgets::button(ui, Some(icons::PLAY), label, Kind::Primary).clicked() {
            v.actions.push(open_record(r));
        }
        if widgets::button(ui, Some(icons::INFO), "Details", Kind::Secondary).clicked() {
            v.state.details = Some(r.id);
        }
    });
    if resp.clicked() {
        v.state.details = Some(r.id);
    }
    ui.add_space(26.0);
}

/// A centred message with an icon and actions, for empty screens.
fn empty_state(
    ui: &mut egui::Ui,
    icon: &str,
    title: &str,
    body: &str,
    add_actions: impl FnOnce(&mut egui::Ui),
) {
    ui.add_space(70.0);
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(96.0), Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 48.0, theme::SURFACE_2);
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            icon,
            theme::icon(44.0),
            theme::ACCENT_HOVER,
        );
        ui.add_space(14.0);
        ui.label(
            RichText::new(title)
                .font(theme::font(Weight::Bold, 30.0))
                .color(theme::TEXT),
        );
        ui.add_space(4.0);
        ui.set_max_width(640.0);
        ui.label(RichText::new(body).color(theme::TEXT_2));
        ui.add_space(18.0);
        add_actions(ui);
    });
}

fn home(ui: &mut egui::Ui, v: &mut View) {
    if v.state.home.is_none() {
        let lib = &v.services.library;
        let favorites = lib
            .search(&fp_library::Query {
                favorites_only: true,
                sort: Sort::LastPlayed,
                limit: Some(20),
                ..Default::default()
            })
            .unwrap_or_default();
        v.state.home = Some(HomeRows {
            continue_watching: lib.continue_watching(20).unwrap_or_default(),
            recent: lib.recently_added(30).unwrap_or_default(),
            favorites,
        });
    }
    let Some(rows) = v.state.home.take() else {
        return;
    };
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(14.0);
        if rows.continue_watching.is_empty() && rows.recent.is_empty() {
            let folders: Vec<String> = v
                .settings
                .library_folders
                .iter()
                .map(|f| f.display().to_string())
                .collect();
            let body = if folders.is_empty() {
                "Copy videos to the Frame with FrameDrop or a USB drive, or stream them from \
                 XBVR, Stash, DLNA, SMB and WebDAV servers."
                    .to_string()
            } else {
                format!(
                    "Videos in {} are added automatically. Copy some over with FrameDrop or a \
                     USB drive, or stream from a server on your network.",
                    folders.join(", ")
                )
            };
            empty_state(
                ui,
                icons::FILM_STRIP,
                "Your library is empty",
                &body,
                |ui| {
                    ui.horizontal(|ui| {
                        ui.add_space((ui.available_width() - 400.0).max(0.0) / 2.0);
                        if widgets::button(
                            ui,
                            Some(icons::ARROWS_CLOCKWISE),
                            "Scan now",
                            Kind::Primary,
                        )
                        .clicked()
                        {
                            v.actions.push(Action::Rescan { force: false });
                        }
                        if widgets::button(ui, Some(icons::PLUS), "Add a source", Kind::Secondary)
                            .clicked()
                        {
                            v.state.screen = Screen::Sources;
                            v.state.source_form = Some(Default::default());
                        }
                    });
                },
            );
        } else {
            hero(ui, v, &rows);
        }
        row(ui, v, "Continue watching", &rows.continue_watching);
        row(ui, v, "Favorites", &rows.favorites);
        row(ui, v, "Recently added", &rows.recent);
    });
    v.state.home = Some(rows);
}

/// The search field: a rounded box with a magnifying glass.
fn search_field(ui: &mut egui::Ui, text: &mut String) -> bool {
    let mut changed = false;
    egui::Frame::new()
        .fill(theme::SURFACE_2)
        .corner_radius(12)
        .inner_margin(egui::Margin::symmetric(14, 0))
        .show(ui, |ui| {
            ui.set_height(44.0);
            ui.horizontal_centered(|ui| {
                ui.label(
                    RichText::new(icons::MAGNIFYING_GLASS)
                        .font(theme::icon(20.0))
                        .color(theme::TEXT_2),
                );
                let r = ui.add(
                    egui::TextEdit::singleline(text)
                        .hint_text("Search titles, tags, folders")
                        .frame(false)
                        .font(theme::font(Weight::Regular, 17.0))
                        .desired_width(260.0),
                );
                changed |= r.changed();
                if !text.is_empty()
                    && widgets::icon_button(ui, icons::X, 30.0, false)
                        .tip("Clear")
                        .clicked()
                {
                    text.clear();
                    changed = true;
                }
            });
        });
    changed
}

fn library(ui: &mut egui::Ui, v: &mut View) {
    let mut changed = false;
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        changed |= search_field(ui, &mut v.state.search);
        ui.add_space(10.0);
        for (f, label) in [
            (FormatFilter::All, "All"),
            (FormatFilter::Flat, "Flat"),
            (FormatFilter::Vr180, "180°"),
            (FormatFilter::Vr360, "360°"),
            (FormatFilter::Fisheye, "Fisheye"),
        ] {
            if widgets::chip(ui, label, v.state.format_filter == f).clicked() {
                v.state.format_filter = f;
                changed = true;
            }
        }
        ui.add_space(10.0);
        if widgets::chip_icon(ui, Some(icons::CUBE), "3D", v.state.stereo_only).clicked() {
            v.state.stereo_only = !v.state.stereo_only;
            changed = true;
        }
        if widgets::chip_icon(ui, Some(icons::HEART), "Favorites", v.state.favorites_only).clicked()
        {
            v.state.favorites_only = !v.state.favorites_only;
            changed = true;
        }
    });
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.label(
            RichText::new(icons::SORT_ASCENDING)
                .font(theme::icon(20.0))
                .color(theme::TEXT_3),
        );
        ui.add_space(4.0);
        for (s, label) in [
            (Sort::Added, "Newest"),
            (Sort::Title, "Title"),
            (Sort::LastPlayed, "Recently played"),
            (Sort::Rating, "Rating"),
            (Sort::Duration, "Length"),
            (Sort::Random, "Shuffle"),
        ] {
            let selected = v.state.sort == s;
            let r = widgets::button_sized(ui, None, label, Kind::Ghost, 38.0);
            if selected {
                let bar = egui::Rect::from_center_size(
                    egui::pos2(r.rect.center().x, r.rect.bottom() - 2.0),
                    Vec2::new(r.rect.width() - 28.0, 2.5),
                );
                ui.painter()
                    .rect_filled(bar, egui::CornerRadius::same(1), theme::ACCENT);
            }
            if r.clicked() {
                v.state.sort = s;
                changed = true;
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(format!(
                    "{} video{}",
                    v.state.total,
                    if v.state.total == 1 { "" } else { "s" }
                ))
                .font(theme::font(Weight::Medium, 15.0))
                .color(theme::TEXT_3),
            );
        });
    });
    ui.add_space(12.0);
    if changed {
        v.state.page_size = 48;
        v.state.results = None;
    }
    if v.state.results.is_none() {
        let q = v.state.query();
        v.state.total = v
            .services
            .library
            .count(&fp_library::Query {
                limit: None,
                ..q.clone()
            })
            .unwrap_or(0);
        v.state.results = Some(v.services.library.search(&q).unwrap_or_default());
    }
    let results = v.state.results.take().unwrap_or_default();
    let mut more = false;
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(4.0);
        if results.is_empty() {
            empty_state(
                ui,
                icons::MAGNIFYING_GLASS,
                "Nothing matches",
                "Try another search, or clear the filters.",
                |_| {},
            );
        }
        grid(ui, v, &results);
        if results.len() < v.state.total {
            ui.vertical_centered(|ui| {
                more = widgets::button(ui, None, "Show more", Kind::Secondary).clicked();
            });
            ui.add_space(20.0);
        }
    });
    if more {
        v.state.page_size += 48;
    } else {
        v.state.results = Some(results);
    }
}

fn details(ui: &mut egui::Ui, v: &mut View, id: fp_library::MediaId) {
    let Some(r) = v.services.library.get(id).ok().flatten() else {
        v.state.details = None;
        return;
    };
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(12.0);
        if widgets::button(ui, Some(icons::ARROW_LEFT), "Back", Kind::Ghost).clicked() {
            v.state.details = None;
        }
        ui.add_space(12.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 28.0;
            let (rect, _) = ui.allocate_exact_size(Vec2::new(512.0, 288.0), Sense::hover());
            widgets::shadow(ui.painter(), rect, 16, 0.6);
            thumbnail(ui, v, &r, rect, 16);
            ui.vertical(|ui| details_side(ui, v, &r));
        });
        ui.add_space(28.0);
        widgets::section_label(ui, "Format");
        ui.add_space(2.0);
        let fmt = r.effective_format();
        ui.label(
            RichText::new(format!(
                "Detected {} ({})",
                fmt.format.label(),
                fmt.evidence.label()
            ))
            .color(theme::TEXT_2),
        );
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
            let current = r.user_format;
            if widgets::chip_icon(ui, Some(icons::MAGIC_WAND), "Automatic", current.is_none())
                .clicked()
            {
                v.actions.push(Action::SetUserFormat(r.id, None));
            }
            for f in super::player::format_choices() {
                if widgets::chip(ui, f.0, current == Some(f.1)).clicked() {
                    v.actions.push(Action::SetUserFormat(r.id, Some(f.1)));
                }
            }
        });
        ui.add_space(26.0);
        widgets::section_label(ui, "File");
        ui.add_space(2.0);
        ui.label(
            RichText::new(&r.location)
                .font(theme::font(Weight::Regular, 15.0))
                .color(theme::TEXT_3),
        );
        ui.add_space(12.0);
        if widgets::button(ui, Some(icons::TRASH), "Remove from library", Kind::Danger).clicked() {
            v.actions.push(Action::RemoveMedia(r.id));
            v.state.details = None;
        }
        ui.add_space(24.0);
    });
}

/// Title, facts, actions and rating beside the details thumbnail.
fn details_side(ui: &mut egui::Ui, v: &mut View, r: &MediaRecord) {
    ui.spacing_mut().item_spacing.y = 8.0;
    ui.label(
        RichText::new(widgets::display_title(&r.title))
            .font(theme::font(Weight::Bold, 34.0))
            .color(theme::TEXT),
    );
    let mut f = vec![widgets::format_short(&r.effective_format().format)];
    if let Some(d) = r.duration {
        f.push(fmt_time(d));
    }
    if let (Some(w), Some(h)) = (r.width, r.height) {
        f.push(format!("{w}×{h}"));
    }
    if let Some(c) = &r.video_codec {
        f.push(c.to_uppercase());
    }
    if let Some(s) = r.size {
        f.push(fmt_size(s));
    }
    if r.play_count > 0 {
        f.push(format!("watched {}×", r.play_count));
    }
    ui.label(
        RichText::new(f.join("  ·  "))
            .font(theme::font(Weight::Medium, 16.0))
            .color(theme::TEXT_2),
    );
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        let resume = r.resume_position > 1.0;
        let label = if resume {
            format!("Resume from {}", fmt_time(r.resume_position))
        } else {
            "Play".to_string()
        };
        if widgets::button(ui, Some(icons::PLAY), &label, Kind::Primary).clicked() {
            v.actions.push(open_record(r));
        }
        if resume
            && widgets::button(
                ui,
                Some(icons::ARROW_COUNTER_CLOCKWISE),
                "Start over",
                Kind::Secondary,
            )
            .clicked()
            && let Action::Open(mut req) = open_record(r)
        {
            req.start_at = Some(0.0);
            v.actions.push(Action::Open(req));
        }
        let heart = if r.favorite {
            "Favorite"
        } else {
            "Add to favorites"
        };
        if widgets::chip_icon(ui, Some(icons::HEART), heart, r.favorite).clicked() {
            v.actions.push(Action::SetFavorite(r.id, !r.favorite));
        }
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for n in 1..=5u8 {
            let on = n <= r.rating;
            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
            let t = ui
                .ctx()
                .animate_bool_with_time(resp.id, resp.hovered(), 0.1);
            let size = 28.0 + t * 3.0;
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                icons::STAR,
                if on {
                    theme::icon_fill(size)
                } else {
                    theme::icon(size)
                },
                if on {
                    theme::WARN
                } else {
                    theme::TEXT_3.lerp_to_gamma(theme::WARN, t)
                },
            );
            if resp.tip(&format!("{n}/5")).clicked() {
                v.actions
                    .push(Action::SetRating(r.id, if r.rating == n { 0 } else { n }));
            }
        }
    });
    if !r.tags.is_empty() {
        ui.label(RichText::new(r.tags.join("  ·  ")).color(theme::TEXT_2));
    }
    if !r.scripts.is_empty() {
        let n = r.scripts.len();
        ui.label(
            RichText::new(format!(
                "{}  {n} haptic script{}",
                icons::WAVEFORM,
                if n == 1 { "" } else { "s" }
            ))
            .color(theme::TEXT_2),
        );
    }
    if let Some(e) = &r.probe_error {
        ui.label(
            RichText::new(format!(
                "{}  Could not read the file: {e}",
                icons::WARNING_CIRCLE
            ))
            .color(theme::ERROR),
        );
    }
    if r.missing {
        ui.label(
            RichText::new(format!(
                "{}  The file is missing (drive unplugged?)",
                icons::WARNING
            ))
            .color(theme::WARN),
        );
    }
}
