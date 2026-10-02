//! Library browser: sources sidebar, search + sort/filter bar, tag chips and
//! a virtualized grid of video tiles.

use super::{codec_label, panel_background, projection_label, resolution_label, UiAction};
use crate::geom::{Rect, Vec2};
use crate::icons::{draw_icon, Icon};
use crate::layout::{Dir, FILL};
use crate::text::{Align, TextParams};
use crate::theme::Color;
use crate::ui::{Sense, Ui};
use crate::widgets::basic::ButtonStyle;
use crate::widgets::keyboard::KeyboardState;
use crate::widgets::timeline::format_time;
use fp_core::draw::TextureId;
use fp_core::{Codec, MediaTime, Projection, StereoMode};
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SourceKind {
    #[default]
    Local,
    Removable,
    Smb,
    WebDav,
    Sftp,
    Dlna,
    Http,
    /// DeoVR-compatible JSON feed (XBVR, Stash…).
    DeoVrFeed,
}

impl SourceKind {
    pub fn icon(self) -> Icon {
        match self {
            SourceKind::Local | SourceKind::Removable => Icon::Folder,
            SourceKind::DeoVrFeed => Icon::Grid,
            _ => Icon::Network,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SourceEntry {
    pub id: u64,
    pub name: String,
    pub kind: SourceKind,
    pub online: bool,
    pub item_count: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LibraryItem {
    pub id: u64,
    pub title: String,
    pub duration: Option<MediaTime>,
    pub width: u32,
    pub height: u32,
    pub codec: Option<Codec>,
    pub projection: Option<Projection>,
    pub stereo: StereoMode,
    pub hdr: bool,
    /// Image-cache key for the thumbnail (`TextureId::Image`).
    pub thumbnail: Option<u64>,
    pub favourite: bool,
    /// Resume point as a fraction of the duration.
    pub resume: Option<f32>,
    pub tags: Vec<String>,
    pub has_script: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SortKey {
    #[default]
    Title,
    Added,
    Duration,
    LastWatched,
    Resolution,
}

impl SortKey {
    pub const ALL: [SortKey; 5] = [
        SortKey::Title,
        SortKey::Added,
        SortKey::Duration,
        SortKey::LastWatched,
        SortKey::Resolution,
    ];
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Title => "Title",
            SortKey::Added => "Date added",
            SortKey::Duration => "Duration",
            SortKey::LastWatched => "Last watched",
            SortKey::Resolution => "Resolution",
        }
    }
}

/// What the library screen shows; filtering/sorting is done by the app.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LibraryView {
    pub sources: Vec<SourceEntry>,
    pub selected_source: Option<u64>,
    pub items: Vec<LibraryItem>,
    pub all_tags: Vec<String>,
    pub active_tags: Vec<String>,
    pub sort: SortKey,
    pub sort_descending: bool,
    pub favourites_only: bool,
    pub loading: bool,
    /// Error / empty-state message.
    pub status: Option<String>,
}

/// UI-local library state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LibraryScreen {
    pub search: String,
    pub keyboard: KeyboardState,
    pub show_keyboard: bool,
    last_visible: Option<Range<usize>>,
}

/// Tile height for a given tile width: 16:9 thumbnail plus two text lines.
pub fn tile_height(tile_w: f32, text_size: f32) -> f32 {
    tile_w * 9.0 / 16.0 + text_size * 2.6
}

impl LibraryScreen {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn show(&mut self, ui: &mut Ui, view: &LibraryView) -> Vec<UiAction> {
        let mut actions = Vec::new();
        let screen = panel_background(ui);
        let t = ui.theme.clone();
        let inner = screen.shrink(t.padding * 1.5);
        let (side, main) = inner.split_left((inner.w * 0.22).clamp(220.0, 420.0));
        let main = Rect::new(
            main.x + t.padding * 1.5,
            main.y,
            main.w - t.padding * 1.5,
            main.h,
        );
        self.sidebar(ui, side, view, &mut actions);
        self.main(ui, main, view, &mut actions);
        if !self.show_keyboard && ui.take_back() {
            actions.push(UiAction::Back);
        }
        actions
    }

    fn sidebar(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        view: &LibraryView,
        actions: &mut Vec<UiAction>,
    ) {
        let t = ui.theme.clone();
        ui.painter()
            .rect_rounded(rect, t.corner_radius, t.surface.alpha(0.5));
        ui.region(rect.shrink(t.padding), Dir::Vertical, |ui| {
            ui.heading("Sources");
            let footer = t.widget_height * 2.0 + t.spacing * 2.0;
            let list_h = (ui.available().h - footer).max(t.widget_height);
            let sources = &view.sources;
            ui.virtual_list(
                "sources",
                sources.len(),
                t.widget_height,
                list_h,
                |ui, i, rect| {
                    let s = &sources[i];
                    let id = ui.make_id(("src", s.id));
                    let resp = ui.interact(rect, id, Sense::CLICK);
                    ui.record(id, rect, || s.name.clone());
                    let selected = view.selected_source == Some(s.id);
                    let bg = if selected {
                        t.accent.alpha(0.45)
                    } else if resp.highlighted() {
                        t.surface_hover
                    } else {
                        Color::TRANSPARENT
                    };
                    ui.painter().rect_rounded(rect, t.corner_radius, bg);
                    let fg = if s.online { t.text } else { t.text_dim };
                    let is = t.icon_size * 0.8;
                    draw_icon(
                        ui.painter(),
                        s.kind.icon(),
                        Rect::new(rect.x + t.padding * 0.5, rect.center().y - is * 0.5, is, is),
                        fg,
                    );
                    let count = s.item_count.map(|n| n.to_string()).unwrap_or_default();
                    let cw = if count.is_empty() {
                        0.0
                    } else {
                        ui.fonts.measure(&count, t.small_text_size) + t.padding * 0.5
                    };
                    let tx = rect.x + t.padding + is;
                    ui.draw_text_in(
                        Rect::new(tx, rect.y, rect.right() - tx - cw - t.padding * 0.5, rect.h),
                        &s.name,
                        t.text_size,
                        fg,
                        Align::Left,
                    );
                    if !count.is_empty() {
                        ui.draw_text_in(
                            Rect::new(rect.right() - cw - t.padding * 0.5, rect.y, cw, rect.h),
                            &count,
                            t.small_text_size,
                            t.text_dim,
                            Align::Right,
                        );
                    }
                    ui.focus_ring(&resp, t.corner_radius);
                    if resp.clicked && !selected {
                        actions.push(UiAction::SelectSource(s.id));
                    }
                },
            );
            if ui
                .button_ex(
                    "Add source",
                    ButtonStyle {
                        icon: Some(Icon::Plus),
                        fill: true,
                        ..Default::default()
                    },
                )
                .clicked
            {
                actions.push(UiAction::AddSource);
            }
            if ui
                .button_ex(
                    "Settings",
                    ButtonStyle {
                        icon: Some(Icon::Settings),
                        fill: true,
                        ..Default::default()
                    },
                )
                .clicked
            {
                actions.push(UiAction::OpenSettings);
            }
        });
    }

    fn main(&mut self, ui: &mut Ui, rect: Rect, view: &LibraryView, actions: &mut Vec<UiAction>) {
        let t = ui.theme.clone();
        ui.region(rect, Dir::Vertical, |ui| {
            // Top bar: search, sort, direction, favourites, refresh.
            ui.row(t.widget_height, |ui| {
                let h = t.widget_height;
                let sort_w = (rect.w * 0.22).max(200.0);
                let buttons = h * 3.0 + t.spacing * 4.0 + sort_w;
                let search_w = (ui.available().w - buttons).max(h * 3.0);
                let s = ui.search_input("search", &mut self.search, "Search library", search_w);
                if s.gained_focus {
                    self.show_keyboard = true;
                }
                if s.changed {
                    actions.push(UiAction::Search(self.search.clone()));
                }
                if s.submitted {
                    self.show_keyboard = false;
                    ui.set_text_focus(None);
                }
                let sort_rect = ui.allocate(Vec2::new(sort_w, h));
                let labels: Vec<&str> = SortKey::ALL.iter().map(|k| k.label()).collect();
                let cur = SortKey::ALL
                    .iter()
                    .position(|k| *k == view.sort)
                    .unwrap_or(0);
                if let Some(i) = ui
                    .region(sort_rect, Dir::Vertical, |ui| {
                        ui.dropdown("sort", "", cur, &labels)
                    })
                    .0
                {
                    actions.push(UiAction::SetSort {
                        key: SortKey::ALL[i],
                        descending: view.sort_descending,
                    });
                }
                let dir_icon = if view.sort_descending {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronUp
                };
                if ui.icon_button("sortdir", dir_icon).clicked {
                    actions.push(UiAction::SetSort {
                        key: view.sort,
                        descending: !view.sort_descending,
                    });
                }
                let fav_rect = ui.allocate(Vec2::new(h, h));
                let fav_icon = if view.favourites_only {
                    Icon::Star
                } else {
                    Icon::StarOutline
                };
                if ui
                    .icon_button_at(fav_rect, "favonly", fav_icon, view.favourites_only)
                    .clicked
                {
                    actions.push(UiAction::SetFavouritesOnly(!view.favourites_only));
                }
                if let Some(src) = view.selected_source {
                    if ui.icon_button("refresh", Icon::Refresh).clicked {
                        actions.push(UiAction::RefreshSource(src));
                    }
                }
            });

            // Tag chips.
            if !view.all_tags.is_empty() {
                let chip_h = t.widget_height * 0.7;
                ui.row(chip_h, |ui| {
                    for tag in &view.all_tags {
                        let w = ui.fonts.measure(tag, t.small_text_size) + t.padding * 1.5;
                        if ui.available().w < w {
                            break;
                        }
                        let r = ui.allocate(Vec2::new(w, chip_h));
                        let id = ui.make_id(("tag", tag));
                        let resp = ui.interact(r, id, Sense::CLICK);
                        ui.record(id, r, || format!("tag {tag}"));
                        let on = view.active_tags.contains(tag);
                        let bg = if on {
                            t.accent
                        } else if resp.highlighted() {
                            t.surface_hover
                        } else {
                            t.surface
                        };
                        ui.painter().rect_rounded(r, chip_h * 0.5, bg);
                        ui.draw_text_in(
                            r,
                            tag,
                            t.small_text_size,
                            if on { t.on_accent } else { t.text },
                            Align::Center,
                        );
                        ui.focus_ring(&resp, chip_h * 0.5);
                        if resp.clicked {
                            actions.push(UiAction::ToggleTag(tag.clone()));
                        }
                    }
                    if !view.active_tags.is_empty()
                        && ui.available().w > chip_h * 3.0
                        && ui.button("Clear").clicked
                    {
                        actions.push(UiAction::ClearTags);
                    }
                });
            }

            let kb_h = if self.show_keyboard {
                (rect.h * 0.42).max(t.widget_height * 5.0)
            } else {
                0.0
            };
            let grid_h = (ui.available().h - kb_h - if kb_h > 0.0 { t.spacing } else { 0.0 })
                .max(t.widget_height);

            if view.items.is_empty() {
                let r = ui.allocate(Vec2::new(FILL, grid_h));
                if view.loading {
                    let s = t.widget_height * 1.5;
                    ui.draw_spinner(Rect::from_center(r.center(), Vec2::splat(s)));
                } else {
                    let msg = view.status.as_deref().unwrap_or(if self.search.is_empty() {
                        "No videos here yet"
                    } else {
                        "No matches"
                    });
                    ui.draw_text_in(r, msg, t.text_size, t.text_dim, Align::Center);
                }
            } else {
                let items = &view.items;
                let min_tile = (t.text_size * 13.0).max(220.0);
                let ts = t.text_size;
                let vr = ui.virtual_grid(
                    "grid",
                    items.len(),
                    min_tile,
                    |w| tile_height(w, ts),
                    grid_h,
                    |ui, i, rect| {
                        library_tile(ui, &items[i], rect, actions);
                    },
                );
                if self.last_visible.as_ref() != Some(&vr.visible) {
                    self.last_visible = Some(vr.visible.clone());
                    actions.push(UiAction::VisibleItems(vr.visible));
                }
                if view.loading {
                    let s = t.widget_height;
                    let r = Rect::new(
                        vr.viewport.right() - s - t.padding,
                        vr.viewport.y + t.padding,
                        s,
                        s,
                    );
                    ui.draw_spinner(r);
                }
            }

            if self.show_keyboard {
                let kb = ui.virtual_keyboard("kb", &mut self.keyboard, kb_h);
                if kb.hide || ui.take_back() {
                    self.show_keyboard = false;
                    ui.set_text_focus(None);
                }
                if kb.events.contains(&crate::input::TextEvent::Enter) {
                    self.show_keyboard = false;
                }
            }
        });
    }
}

/// One grid tile: thumbnail with badges, favourite star, resume bar and title.
pub fn library_tile(ui: &mut Ui, item: &LibraryItem, rect: Rect, actions: &mut Vec<UiAction>) {
    let t = ui.theme.clone();
    let thumb = Rect::new(rect.x, rect.y, rect.w, rect.w * 9.0 / 16.0);
    // The star sits on top of the tile, so it must claim the press first.
    let star_s = t.widget_height * 0.8;
    let star = Rect::new(
        thumb.right() - star_s - t.spacing * 0.5,
        thumb.y + t.spacing * 0.5,
        star_s,
        star_s,
    );
    let star_id = ui.make_id(("fav", item.id));
    let star_resp = ui.interact(star, star_id, Sense::CLICK);
    ui.record(star_id, star, || format!("favourite {}", item.title));
    let id = ui.make_id(("tile", item.id));
    let resp = ui.interact(rect, id, Sense::CLICK);
    ui.record(id, rect, || item.title.clone());
    if star_resp.clicked {
        actions.push(UiAction::ToggleFavourite(item.id));
    } else if resp.clicked {
        actions.push(UiAction::OpenItem(item.id));
    }

    let lift = if resp.highlighted() { 1.0 } else { 0.0 };
    if lift > 0.0 {
        ui.painter()
            .rect_rounded(rect.expand(4.0), t.corner_radius + 4.0, t.surface_hover);
    }
    match item.thumbnail {
        Some(img) => ui.painter().image(
            thumb,
            TextureId::Image(img),
            [0.0, 0.0, 1.0, 1.0],
            Color::WHITE,
        ),
        None => {
            ui.painter().rect_rounded(thumb, t.corner_radius, t.surface);
            let s = thumb.h * 0.3;
            draw_icon(
                ui.painter(),
                Icon::Play,
                Rect::from_center(thumb.center(), Vec2::splat(s)),
                t.text_dim,
            );
        }
    }

    // Top-left badges: resolution, codec, projection, HDR, script.
    let mut x = thumb.x + t.spacing * 0.5;
    let y = thumb.y + t.spacing * 0.5;
    let badge_bg = Color::BLACK.alpha(0.65);
    let mut badges: Vec<String> = Vec::new();
    let res = resolution_label(item.width, item.height);
    if !res.is_empty() {
        badges.push(res);
    }
    if let Some(c) = item.codec {
        badges.push(codec_label(c).into());
    }
    if let Some(p) = &item.projection {
        badges.push(projection_label(p, item.stereo));
    }
    if item.hdr {
        badges.push("HDR".into());
    }
    let max_x = star.x - t.spacing * 0.5;
    for b in badges {
        let w = ui.fonts.measure(&b, t.small_text_size * 0.85) + t.small_text_size * 0.8;
        if x + w > max_x {
            break;
        }
        let r = ui.draw_badge(Vec2::new(x, y), &b, badge_bg, t.text);
        x = r.right() + t.spacing * 0.4;
    }
    if item.has_script {
        let s = t.icon_size * 0.7;
        let r = Rect::new(
            thumb.x + t.spacing * 0.5,
            thumb.bottom() - s - t.spacing,
            s + 8.0,
            s + 4.0,
        );
        ui.painter().rect_rounded(r, 4.0, badge_bg);
        draw_icon(ui.painter(), Icon::Haptics, r.shrink(3.0), t.warning);
    }
    // Duration bottom-right.
    if let Some(d) = item.duration {
        let label = format_time(d);
        let w = ui.fonts.measure(&label, t.small_text_size * 0.85) + t.small_text_size * 0.8;
        let h = t.small_text_size * 0.85 * 1.4 + t.small_text_size * 0.3;
        ui.draw_badge(
            Vec2::new(
                thumb.right() - w - t.spacing * 0.5,
                thumb.bottom() - h - t.spacing,
            ),
            &label,
            badge_bg,
            t.text,
        );
    }
    // Favourite star.
    let star_color = if item.favourite {
        t.warning
    } else {
        t.text
            .alpha(if star_resp.highlighted() { 1.0 } else { 0.7 })
    };
    if star_resp.highlighted() {
        ui.painter()
            .circle(star.center(), star_s * 0.55, Color::BLACK.alpha(0.5));
    }
    ui.focus_ring(&star_resp, star_s * 0.5);
    draw_icon(
        ui.painter(),
        if item.favourite {
            Icon::Star
        } else {
            Icon::StarOutline
        },
        star.shrink(star_s * 0.15),
        star_color,
    );
    // Resume bar.
    if let Some(f) = item.resume.filter(|f| *f > 0.0) {
        let bar = Rect::new(thumb.x, thumb.bottom() - 6.0, thumb.w, 6.0);
        ui.painter().rect_filled(bar, Color::BLACK.alpha(0.6));
        ui.painter().rect_filled(
            Rect::new(bar.x, bar.y, bar.w * f.clamp(0.0, 1.0), bar.h),
            t.accent,
        );
    }
    // Title (two lines max) and tags line.
    let title_y = thumb.bottom() + t.spacing * 0.5;
    let layout = ui.fonts.layout(
        &item.title,
        TextParams::new(t.text_size).width(rect.w).ellipsis(),
    );
    ui.draw_text_at(Vec2::new(rect.x, title_y), &layout, t.text);
    if !item.tags.is_empty() {
        let tags = item.tags.join(" · ");
        let l = ui.fonts.layout(
            &tags,
            TextParams::new(t.small_text_size).width(rect.w).ellipsis(),
        );
        ui.draw_text_at(Vec2::new(rect.x, title_y + layout.size.y), &l, t.text_dim);
    }
    ui.focus_ring(&resp, t.corner_radius);
}
