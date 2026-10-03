//! The browser panel: navigation, home rows, library grid, video details.

use super::thumbs::paint_cover;
use super::{Action, FormatFilter, HomeRows, Screen, View, big_button, fmt_size, fmt_time, theme};
use crate::playback::OpenRequest;
use egui::{Align, Color32, Layout, RichText, Sense, Vec2};
use fp_library::{MediaRecord, Sort};

const CARD: Vec2 = Vec2::new(236.0, 133.0);

/// Main browser panel.
pub fn browser(ctx: &egui::Context, v: &mut View) {
    egui::TopBottomPanel::top("nav")
        .exact_height(64.0)
        .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.label(
                    RichText::new("FramePlayer")
                        .size(26.0)
                        .strong()
                        .color(theme::ACCENT),
                );
                ui.add_space(18.0);
                for (s, label) in [
                    (Screen::Home, "Home"),
                    (Screen::Library, "Library"),
                    (Screen::Sources, "Sources"),
                    (Screen::Web, "Web XR"),
                    (Screen::Settings, "Settings"),
                ] {
                    if big_button(ui, label, v.state.screen == s).clicked() {
                        v.state.screen = s;
                        v.state.details = None;
                        if s == Screen::Home {
                            v.state.home = None;
                        }
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if v.playback.is_some() {
                        if big_button(ui, "🗙", false)
                            .on_hover_text("Back to the video")
                            .clicked()
                        {
                            v.actions.push(Action::ShowBrowser(false));
                        }
                    } else if big_button(ui, "Exit", false)
                        .on_hover_text("Quit FramePlayer")
                        .clicked()
                    {
                        v.actions.push(Action::Quit);
                    }
                    if v.passthrough_available {
                        let on = v.settings.passthrough;
                        if big_button(ui, "👓", on)
                            .on_hover_text("Passthrough")
                            .clicked()
                        {
                            v.actions.push(Action::TogglePassthrough);
                        }
                    }
                    if big_button(ui, "⌖", false)
                        .on_hover_text("Recenter (or squeeze both grips)")
                        .clicked()
                    {
                        v.actions.push(Action::Recenter);
                    }
                    if let Some(s) = &v.state.worker_status {
                        ui.label(RichText::new(s).small().color(theme::MUTED));
                    }
                });
            });
        });
    status_bar(ctx, v);
    egui::CentralPanel::default().show(ctx, |ui| {
        if let Some(id) = v.state.details {
            details(ui, v, id);
            return;
        }
        match v.state.screen {
            Screen::Home => home(ui, v),
            Screen::Library => library(ui, v),
            Screen::Sources => super::sources::sources(ui, v),
            Screen::Web => super::web::web(ui, v),
            Screen::Settings => super::settings::settings(ui, v),
        }
    });
}

fn status_bar(ctx: &egui::Context, v: &mut View) {
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
    egui::TopBottomPanel::bottom("status")
        .exact_height(46.0)
        .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                if let Some(o) = opening {
                    ui.spinner();
                    ui.label(format!("Opening {o}…"));
                } else if let Some(e) = err {
                    ui.label(RichText::new(format!("Could not open: {e}")).color(theme::ERROR));
                    if ui.button("Dismiss").clicked() {
                        v.state.open_error = None;
                    }
                } else if let Some(t) = toast {
                    ui.label(t);
                }
            });
        });
}

fn thumb_key(r: &MediaRecord) -> Option<String> {
    r.thumbnail
        .as_ref()
        .map(|p| p.display().to_string())
        .or_else(|| r.thumbnail_url.clone())
}

fn open_record(r: &MediaRecord) -> Action {
    Action::Open(OpenRequest {
        location: r.location.clone(),
        source_id: (r.source_id != "local"
            && !r.source_id.is_empty()
            && !r.location.starts_with('/'))
        .then(|| r.source_id.clone()),
        entry: None,
        start_at: None,
    })
}

/// A thumbnail card; returns (play clicked, details clicked).
fn card(ui: &mut egui::Ui, v: &mut View, r: &MediaRecord) -> (bool, bool) {
    let (rect, resp) = ui.allocate_exact_size(CARD + Vec2::new(0.0, 50.0), Sense::click());
    let img_rect = egui::Rect::from_min_size(rect.min, CARD);
    let painter = ui.painter_at(rect);
    painter.rect_filled(img_rect, 10.0, theme::CARD_BG);
    if let Some(tex) = thumb_key(r).and_then(|k| v.thumbs.get(&k)) {
        paint_cover(&painter, img_rect.shrink(1.0), &tex);
    } else {
        painter.text(
            img_rect.center(),
            egui::Align2::CENTER_CENTER,
            "▶",
            egui::FontId::proportional(36.0),
            theme::MUTED,
        );
    }
    if resp.hovered() {
        painter.rect_stroke(
            img_rect,
            10.0,
            egui::Stroke::new(3.0_f32, theme::ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    // Badges: format, duration, progress, favourite.
    let fmt = r.effective_format().format;
    let badge = |text: String, pos: egui::Pos2, align: egui::Align2| {
        let galley = painter.layout_no_wrap(text, egui::FontId::proportional(15.0), Color32::WHITE);
        let br = align.anchor_size(pos, galley.size() + Vec2::new(10.0, 4.0));
        painter.rect_filled(br, 6.0, Color32::from_black_alpha(190));
        painter.galley(br.min + Vec2::new(5.0, 2.0), galley, Color32::WHITE);
    };
    let label = match (fmt.projection, fmt.stereo) {
        (fp_core::format::Projection::Flat, fp_core::format::StereoLayout::Mono) => {
            "2D".to_string()
        }
        (p, s) => {
            let p = match p {
                fp_core::format::Projection::Flat => "3D".to_string(),
                fp_core::format::Projection::Equirect { h_fov, .. } => {
                    format!("{}°", h_fov.round())
                }
                fp_core::format::Projection::Fisheye { fov } => format!("FE{}", fov.round()),
                fp_core::format::Projection::Eac { h_fov } => format!("EAC{}", h_fov.round()),
            };
            match s {
                fp_core::format::StereoLayout::Mono => p,
                fp_core::format::StereoLayout::SideBySide => format!("{p} SBS"),
                fp_core::format::StereoLayout::TopBottom => format!("{p} TB"),
            }
        }
    };
    badge(
        label,
        img_rect.left_top() + Vec2::new(6.0, 6.0),
        egui::Align2::LEFT_TOP,
    );
    if let Some(d) = r.duration {
        badge(
            fmt_time(d),
            img_rect.right_bottom() - Vec2::new(6.0, 8.0),
            egui::Align2::RIGHT_BOTTOM,
        );
    }
    if !r.scripts.is_empty() {
        badge(
            "〰".into(),
            img_rect.right_top() + Vec2::new(-6.0, 6.0),
            egui::Align2::RIGHT_TOP,
        );
    }
    if let Some(p) = r.progress() {
        let bar = egui::Rect::from_min_max(
            img_rect.left_bottom() - Vec2::new(0.0, 4.0),
            img_rect.right_bottom(),
        );
        painter.rect_filled(bar, 0.0, Color32::from_black_alpha(160));
        let mut done = bar;
        done.set_width(bar.width() * p as f32);
        painter.rect_filled(done, 0.0, theme::ACCENT);
    }
    let title_rect = egui::Rect::from_min_max(
        img_rect.left_bottom() + Vec2::new(2.0, 4.0),
        rect.right_bottom(),
    );
    let title = if r.favorite {
        format!("★ {}", r.title)
    } else {
        r.title.clone()
    };
    let galley = painter.layout(
        title,
        egui::FontId::proportional(16.0),
        if r.missing {
            theme::MUTED
        } else {
            Color32::WHITE
        },
        title_rect.width() - 34.0,
    );
    painter.galley(title_rect.min, galley, Color32::WHITE);
    // "More" button.
    let more = egui::Rect::from_min_size(
        egui::pos2(rect.right() - 32.0, title_rect.top()),
        Vec2::new(32.0, 30.0),
    );
    let more_resp = ui.interact(more, resp.id.with("more"), Sense::click());
    painter.text(
        more.center(),
        egui::Align2::CENTER_CENTER,
        "…",
        egui::FontId::proportional(22.0),
        if more_resp.hovered() {
            theme::ACCENT
        } else {
            theme::MUTED
        },
    );
    (resp.clicked() && !more_resp.clicked(), more_resp.clicked())
}

fn grid(ui: &mut egui::Ui, v: &mut View, records: &[MediaRecord]) {
    let per_row = ((ui.available_width() + 12.0) / (CARD.x + 12.0))
        .floor()
        .max(1.0) as usize;
    for row in records.chunks(per_row) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            for r in row {
                let (play, more) = card(ui, v, r);
                if play {
                    v.actions.push(open_record(r));
                }
                if more {
                    v.state.details = Some(r.id);
                }
            }
        });
        ui.add_space(6.0);
    }
}

fn row(ui: &mut egui::Ui, v: &mut View, title: &str, records: &[MediaRecord]) {
    if records.is_empty() {
        return;
    }
    ui.label(RichText::new(title).heading());
    egui::ScrollArea::horizontal()
        .id_salt(title)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                for r in records {
                    let (play, more) = card(ui, v, r);
                    if play {
                        v.actions.push(open_record(r));
                    }
                    if more {
                        v.state.details = Some(r.id);
                    }
                }
            });
        });
    ui.add_space(8.0);
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
        if rows.continue_watching.is_empty() && rows.recent.is_empty() {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Welcome to FramePlayer").size(34.0).strong());
                ui.add_space(10.0);
                ui.label("Your library is empty. Videos in these folders are added automatically:");
                for f in &v.settings.library_folders {
                    ui.label(RichText::new(f.display().to_string()).monospace());
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 520.0).max(0.0) / 2.0);
                    if big_button(ui, "Scan now", false).clicked() {
                        v.actions.push(Action::Rescan { force: false });
                    }
                    if big_button(ui, "Add a network source", false).clicked() {
                        v.state.screen = Screen::Sources;
                        v.state.source_form = Some(Default::default());
                    }
                });
                ui.add_space(10.0);
                ui.label(RichText::new("Tip: copy videos to the Frame with FrameDrop or a USB drive, or stream from XBVR, Stash, DLNA, SMB and WebDAV.").color(theme::MUTED));
            });
        }
        row(ui, v, "Continue watching", &rows.continue_watching);
        row(ui, v, "Favorites", &rows.favorites);
        row(ui, v, "Recently added", &rows.recent);
    });
    v.state.home = Some(rows);
}

fn library(ui: &mut egui::Ui, v: &mut View) {
    let mut changed = false;
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut v.state.search)
                .hint_text("Search titles, tags, folders")
                .desired_width(330.0),
        );
        changed |= r.changed();
        if !v.state.search.is_empty() && ui.button("🗙").clicked() {
            v.state.search.clear();
            changed = true;
        }
        for (f, label) in [
            (FormatFilter::All, "All"),
            (FormatFilter::Flat, "Flat"),
            (FormatFilter::Vr180, "180°"),
            (FormatFilter::Vr360, "360°"),
            (FormatFilter::Fisheye, "Fisheye"),
        ] {
            if ui
                .selectable_label(v.state.format_filter == f, label)
                .clicked()
            {
                v.state.format_filter = f;
                changed = true;
            }
        }
        changed |= ui.toggle_value(&mut v.state.stereo_only, "3D").changed();
        changed |= ui.toggle_value(&mut v.state.favorites_only, "★").changed();
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("Sort").color(theme::MUTED));
        for (s, label) in [
            (Sort::Added, "Newest"),
            (Sort::Title, "Title"),
            (Sort::LastPlayed, "Recently played"),
            (Sort::Rating, "Rating"),
            (Sort::Duration, "Length"),
            (Sort::Random, "Shuffle"),
        ] {
            if ui.selectable_label(v.state.sort == s, label).clicked() {
                v.state.sort = s;
                changed = true;
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(format!("{} videos", v.state.total)).color(theme::MUTED));
        });
    });
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
        if results.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Nothing matches.").color(theme::MUTED))
            });
        }
        grid(ui, v, &results);
        if results.len() < v.state.total {
            ui.vertical_centered(|ui| {
                more = big_button(ui, "Show more", false).clicked();
            });
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
    ui.horizontal(|ui| {
        if big_button(ui, "◀ Back", false).clicked() {
            v.state.details = None;
        }
        ui.label(RichText::new(&r.title).heading());
    });
    ui.add_space(8.0);
    ui.horizontal_top(|ui| {
        let size = Vec2::new(480.0, 270.0);
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        ui.painter().rect_filled(rect, 12.0, theme::CARD_BG);
        if let Some(tex) = thumb_key(&r).and_then(|k| v.thumbs.get(&k)) {
            paint_cover(ui.painter(), rect, &tex);
        }
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                if big_button(ui, "▶ Play", false).clicked() {
                    v.actions.push(open_record(&r));
                }
                if r.resume_position > 1.0
                    && big_button(ui, "Play from start", false).clicked()
                    && let Action::Open(mut req) = open_record(&r)
                {
                    req.start_at = Some(0.0);
                    v.actions.push(Action::Open(req));
                }
            });
            ui.horizontal(|ui| {
                let fav = if r.favorite {
                    "★ Favorite"
                } else {
                    "☆ Favorite"
                };
                if ui.selectable_label(r.favorite, fav).clicked() {
                    v.actions.push(Action::SetFavorite(r.id, !r.favorite));
                }
                ui.add_space(12.0);
                for n in 1..=5u8 {
                    let star = if n <= r.rating { "★" } else { "☆" };
                    if ui
                        .button(RichText::new(star).size(22.0).color(theme::WARN))
                        .clicked()
                    {
                        v.actions
                            .push(Action::SetRating(r.id, if r.rating == n { 0 } else { n }));
                    }
                }
            });
            let fmt = r.effective_format();
            ui.label(format!(
                "Format: {} ({})",
                fmt.format.label(),
                fmt.evidence.label()
            ));
            let mut facts = Vec::new();
            if let Some(d) = r.duration {
                facts.push(fmt_time(d));
            }
            if let (Some(w), Some(h)) = (r.width, r.height) {
                facts.push(format!("{w}×{h}"));
            }
            if let Some(c) = &r.video_codec {
                facts.push(c.to_uppercase());
            }
            if let Some(s) = r.size {
                facts.push(fmt_size(s));
            }
            if r.play_count > 0 {
                facts.push(format!("watched {}×", r.play_count));
            }
            ui.label(RichText::new(facts.join(" · ")).color(theme::MUTED));
            if !r.scripts.is_empty() {
                ui.label(format!("Haptic scripts: {}", r.scripts.len()));
            }
            if !r.tags.is_empty() {
                ui.label(format!("Tags: {}", r.tags.join(", ")));
            }
            if let Some(e) = &r.probe_error {
                ui.label(RichText::new(format!("Probe failed: {e}")).color(theme::ERROR));
            }
            if r.missing {
                ui.label(RichText::new("File is missing (drive unplugged?)").color(theme::WARN));
            }
        });
    });
    ui.add_space(10.0);
    ui.label(RichText::new("Format override").strong());
    ui.horizontal_wrapped(|ui| {
        let current = r.user_format;
        if ui
            .selectable_label(current.is_none(), "Automatic")
            .clicked()
        {
            v.actions.push(Action::SetUserFormat(r.id, None));
        }
        for f in super::player::format_choices() {
            if ui.selectable_label(current == Some(f.1), f.0).clicked() {
                v.actions.push(Action::SetUserFormat(r.id, Some(f.1)));
            }
        }
    });
    ui.add_space(10.0);
    ui.label(RichText::new(&r.location).small().color(theme::MUTED));
    if ui
        .button(RichText::new("Remove from library").color(theme::ERROR))
        .clicked()
    {
        v.actions.push(Action::RemoveMedia(r.id));
        v.state.details = None;
    }
}
