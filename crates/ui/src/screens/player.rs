//! Player controls panel: title/time, timeline with chapters, A-B loop and
//! script heat strip, transport buttons, and quick menus for speed,
//! projection/stereo, subtitles and audio tracks.

use super::{projection_label, UiAction};
use crate::geom::{Rect, Vec2};
use crate::icons::{draw_icon, Icon};
use crate::layout::{Dir, FILL};
use crate::painter::Layer;
use crate::text::Align;
use crate::ui::Ui;
use crate::widgets::basic::ButtonStyle;
use crate::widgets::timeline::{format_time, TimelineView};
use fp_core::draw::TextureId;
use fp_core::media::Chapter;
use fp_core::{FisheyeLens, MediaTime, Projection, StereoMode};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackEntry {
    pub id: u32,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScriptStatus {
    pub name: String,
    /// Connected device name, if any.
    pub device: Option<String>,
    pub enabled: bool,
    /// Intensity buckets for the timeline heat strip.
    pub heat: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerView {
    pub title: String,
    pub position: MediaTime,
    pub duration: MediaTime,
    pub playing: bool,
    pub buffering: bool,
    pub buffered: Vec<(MediaTime, MediaTime)>,
    pub chapters: Vec<Chapter>,
    pub loop_a: Option<MediaTime>,
    pub loop_b: Option<MediaTime>,
    pub speed: f32,
    pub projection: Projection,
    pub stereo: StereoMode,
    pub swap_eyes: bool,
    pub subtitle_tracks: Vec<TrackEntry>,
    pub selected_subtitle: Option<u32>,
    pub audio_tracks: Vec<TrackEntry>,
    pub selected_audio: Option<u32>,
    pub script: Option<ScriptStatus>,
    /// Preview sprite for the last requested hover time.
    pub preview: Option<(TextureId, [f32; 4])>,
    pub preview_aspect: f32,
    pub seek_step: MediaTime,
}

impl Default for PlayerView {
    fn default() -> Self {
        PlayerView {
            title: String::new(),
            position: MediaTime::ZERO,
            duration: MediaTime::ZERO,
            playing: false,
            buffering: false,
            buffered: Vec::new(),
            chapters: Vec::new(),
            loop_a: None,
            loop_b: None,
            speed: 1.0,
            projection: Projection::default(),
            stereo: StereoMode::Mono,
            swap_eyes: false,
            subtitle_tracks: Vec::new(),
            selected_subtitle: None,
            audio_tracks: Vec::new(),
            selected_audio: None,
            script: None,
            preview: None,
            preview_aspect: 16.0 / 9.0,
            seek_step: MediaTime::from_millis(10_000),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerMenu {
    Speed,
    Projection,
    Subtitles,
    Audio,
}

pub const SPEEDS: [f32; 9] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];

/// Projection presets offered in the quick picker.
pub fn projection_presets() -> Vec<(&'static str, Projection)> {
    vec![
        ("Flat", Projection::FLAT_DEFAULT),
        ("180°", Projection::EQUIRECT_180),
        ("360°", Projection::EQUIRECT_360),
        ("Fisheye 180°", Projection::fisheye_fov(180.0)),
        ("Fisheye 190°", Projection::fisheye_fov(190.0)),
        ("Fisheye 200°", Projection::fisheye_fov(200.0)),
        ("Canon RF 5.2", Projection::fisheye(FisheyeLens::CanonRf52)),
        ("MKX200", Projection::fisheye(FisheyeLens::Mkx200)),
        ("MKX220", Projection::fisheye(FisheyeLens::Mkx220)),
        ("EAC", Projection::Eac),
    ]
}

/// What the A-B loop button does next given the current markers.
pub fn next_loop_action(a: Option<MediaTime>, b: Option<MediaTime>, now: MediaTime) -> UiAction {
    match (a, b) {
        (None, _) => UiAction::SetLoopA(now),
        (Some(a), None) if now > a => UiAction::SetLoopB(now),
        (Some(_), None) => UiAction::SetLoopA(now),
        (Some(_), Some(_)) => UiAction::ClearLoop,
    }
}

/// Start of the previous/next chapter relative to `now` (previous restarts the
/// current chapter unless within the first 3 s of it).
pub fn chapter_skip(chapters: &[Chapter], now: MediaTime, forward: bool) -> Option<MediaTime> {
    if forward {
        chapters.iter().find(|c| c.start > now).map(|c| c.start)
    } else {
        let grace = MediaTime::from_millis(3000);
        chapters
            .iter()
            .rev()
            .find(|c| c.start + grace < now)
            .map(|c| c.start)
            .or(chapters.first().map(|c| c.start))
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlayerControls {
    pub menu: Option<PlayerMenu>,
    last_preview: Option<MediaTime>,
    /// Scroll the just-opened menu to its selected entry.
    scroll_to_selected: bool,
}

impl PlayerControls {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn show(&mut self, ui: &mut Ui, view: &PlayerView) -> Vec<UiAction> {
        let mut actions = Vec::new();
        let t = ui.theme.clone();
        // Controls hug the bottom edge; the transparent space above them is
        // where quick menus open, so the video stays visible around them.
        let heat_h = if view
            .script
            .as_ref()
            .is_some_and(|s| s.enabled && !s.heat.is_empty())
        {
            t.track_thickness * 1.5
        } else {
            0.0
        };
        let content_h = t.widget_height * 0.8
            + (t.widget_height + heat_h)
            + t.widget_height * 1.15
            + t.spacing * 2.0;
        let screen = ui.screen_rect();
        let strip = Rect::new(
            screen.x,
            (screen.bottom() - content_h - t.padding * 3.0).max(0.0),
            screen.w,
            (content_h + t.padding * 3.0).min(screen.h),
        );
        ui.painter()
            .rect_rounded(strip, t.corner_radius * 2.0, t.panel_bg);
        let inner = strip.shrink(t.padding * 1.5);
        let mut menu_anchor: Option<(PlayerMenu, Rect)> = None;

        ui.region(inner, Dir::Vertical, |ui| {
            // Title + time.
            ui.row(t.widget_height * 0.8, |ui| {
                let time = format!(
                    "{} / {}",
                    format_time(view.position),
                    format_time(view.duration)
                );
                let tw = ui.fonts.measure(&time, t.text_size) + t.padding;
                let avail = ui.available();
                let title_rect = ui.allocate(Vec2::new((avail.w - tw).max(0.0), FILL));
                ui.draw_text_in(title_rect, &view.title, t.text_size, t.text, Align::Left);
                let r = ui.allocate(Vec2::new(FILL, FILL));
                ui.draw_text_in(r, &time, t.text_size, t.text_dim, Align::Right);
            });

            let heat = view
                .script
                .as_ref()
                .filter(|s| s.enabled && !s.heat.is_empty())
                .map(|s| s.heat.as_slice());
            let tl = ui.timeline(
                "seek",
                &TimelineView {
                    position: view.position,
                    duration: view.duration,
                    buffered: &view.buffered,
                    chapters: &view.chapters,
                    loop_a: view.loop_a,
                    loop_b: view.loop_b,
                    heat,
                    preview: view.preview,
                    preview_aspect: view.preview_aspect,
                    step: view.seek_step,
                },
            );
            if let Some(s) = tl.scrub {
                actions.push(UiAction::Scrub(s));
            }
            if let Some(s) = tl.seek {
                actions.push(UiAction::Seek(s));
            }
            if let Some(h) = tl.hover_time.or(tl.scrub) {
                // Request previews at ~1 s granularity to keep the app's sprite cache hot.
                let bucket = MediaTime::from_millis(h.as_millis() / 1000 * 1000);
                if self.last_preview != Some(bucket) {
                    self.last_preview = Some(bucket);
                    actions.push(UiAction::RequestPreview(bucket));
                }
            }

            // Transport row.
            let h = t.widget_height * 1.15;
            ui.row(h, |ui| {
                if ui.icon_button("back", Icon::Back).clicked {
                    actions.push(UiAction::Back);
                }
                ui.add_space(t.spacing * 2.0);
                if !view.chapters.is_empty() && ui.icon_button("prevch", Icon::SkipBack).clicked {
                    if let Some(s) = chapter_skip(&view.chapters, view.position, false) {
                        actions.push(UiAction::Seek(s));
                    }
                }
                if ui.icon_button("rew", Icon::SeekBack).clicked {
                    actions.push(UiAction::SeekRelative(-view.seek_step.as_secs_f64()));
                }
                let big = ui.allocate(Vec2::new(h, h));
                let icon = if view.playing {
                    Icon::Pause
                } else {
                    Icon::Play
                };
                let id = ui.make_id("playpause");
                if ui
                    .button_at(
                        big,
                        id,
                        "",
                        ButtonStyle {
                            primary: true,
                            icon: Some(icon),
                            ..Default::default()
                        },
                    )
                    .clicked
                {
                    actions.push(UiAction::TogglePlay);
                }
                if view.buffering {
                    ui.draw_spinner(big.expand(4.0));
                }
                if ui.icon_button("ff", Icon::SeekForward).clicked {
                    actions.push(UiAction::SeekRelative(view.seek_step.as_secs_f64()));
                }
                if !view.chapters.is_empty() && ui.icon_button("nextch", Icon::SkipForward).clicked
                {
                    if let Some(s) = chapter_skip(&view.chapters, view.position, true) {
                        actions.push(UiAction::Seek(s));
                    }
                }
                ui.add_space(t.spacing * 2.0);

                let speed = format!("{}×", trim_float(view.speed));
                let r = self.menu_button(ui, &speed, None, PlayerMenu::Speed);
                if r.0 {
                    menu_anchor = Some((PlayerMenu::Speed, r.1));
                }
                let proj = projection_label(&view.projection, view.stereo);
                let r = self.menu_button(ui, &proj, Some(Icon::Eye), PlayerMenu::Projection);
                if r.0 {
                    menu_anchor = Some((PlayerMenu::Projection, r.1));
                }
                if !view.subtitle_tracks.is_empty() {
                    let r = self.menu_button(ui, "", Some(Icon::Subtitles), PlayerMenu::Subtitles);
                    if r.0 {
                        menu_anchor = Some((PlayerMenu::Subtitles, r.1));
                    }
                }
                if view.audio_tracks.len() > 1 {
                    let r = self.menu_button(ui, "", Some(Icon::Audio), PlayerMenu::Audio);
                    if r.0 {
                        menu_anchor = Some((PlayerMenu::Audio, r.1));
                    }
                }
                let loop_label = match (view.loop_a, view.loop_b) {
                    (None, _) => "A",
                    (Some(_), None) => "B",
                    _ => "A-B",
                };
                let looping = view.loop_a.is_some();
                if ui
                    .button_ex(
                        loop_label,
                        ButtonStyle {
                            icon: Some(Icon::Loop),
                            selected: looping,
                            ..Default::default()
                        },
                    )
                    .clicked
                {
                    actions.push(next_loop_action(view.loop_a, view.loop_b, view.position));
                }
                if ui.icon_button("recenter", Icon::Recenter).clicked {
                    actions.push(UiAction::Recenter);
                }
                if ui.icon_button("adjust", Icon::Adjust).clicked {
                    actions.push(UiAction::OpenPictureAdjust);
                }
                if let Some(s) = &view.script {
                    let r = ui.allocate(Vec2::new(h, h));
                    let resp = ui.icon_button_at(r, "script", Icon::Haptics, s.enabled);
                    if resp.clicked {
                        actions.push(UiAction::SetScriptEnabled(!s.enabled));
                    }
                    // Status dot: green = device connected, amber = script only.
                    let dot = if s.device.is_some() {
                        t.success
                    } else {
                        t.warning
                    };
                    ui.painter()
                        .circle(Vec2::new(r.right() - 8.0, r.y + 8.0), 6.0, dot);
                    if resp.hovered {
                        let tip = match &s.device {
                            Some(d) => format!("{} → {}", s.name, d),
                            None => format!("{} (no device)", s.name),
                        };
                        ui.tooltip(r, &tip);
                    }
                }
            });
        });

        if let Some((menu, anchor)) = menu_anchor {
            self.draw_menu(ui, menu, anchor, view, &mut actions);
        } else if ui.take_back() {
            actions.push(UiAction::Back);
        }
        actions
    }

    /// Button that toggles a menu; returns `(menu open, button rect)`.
    fn menu_button(
        &mut self,
        ui: &mut Ui,
        label: &str,
        icon: Option<Icon>,
        menu: PlayerMenu,
    ) -> (bool, Rect) {
        let open = self.menu == Some(menu);
        let resp = ui.button_ex(
            label,
            ButtonStyle {
                icon,
                selected: open,
                ..Default::default()
            },
        );
        if resp.clicked {
            self.menu = if open { None } else { Some(menu) };
            self.scroll_to_selected = !open;
        }
        (self.menu == Some(menu), resp.rect)
    }

    fn draw_menu(
        &mut self,
        ui: &mut Ui,
        menu: PlayerMenu,
        anchor: Rect,
        view: &PlayerView,
        actions: &mut Vec<UiAction>,
    ) {
        let t = ui.theme.clone();
        let item_h = t.widget_height * 0.85;
        let entries: Vec<(String, bool, UiAction)> = match menu {
            PlayerMenu::Speed => SPEEDS
                .iter()
                .map(|&s| {
                    (
                        format!("{}×", trim_float(s)),
                        (view.speed - s).abs() < 1e-3,
                        UiAction::SetSpeed(s),
                    )
                })
                .collect(),
            PlayerMenu::Projection => projection_presets()
                .into_iter()
                .map(|(name, p)| {
                    (
                        name.to_string(),
                        same_projection(&p, &view.projection),
                        UiAction::SetProjection(p),
                    )
                })
                .collect(),
            PlayerMenu::Subtitles => std::iter::once((
                "Off".to_string(),
                view.selected_subtitle.is_none(),
                UiAction::SelectSubtitle(None),
            ))
            .chain(view.subtitle_tracks.iter().map(|tr| {
                (
                    tr.label.clone(),
                    view.selected_subtitle == Some(tr.id),
                    UiAction::SelectSubtitle(Some(tr.id)),
                )
            }))
            .collect(),
            PlayerMenu::Audio => view
                .audio_tracks
                .iter()
                .map(|tr| {
                    (
                        tr.label.clone(),
                        view.selected_audio == Some(tr.id),
                        UiAction::SelectAudio(tr.id),
                    )
                })
                .collect(),
        };
        let extra_h = if menu == PlayerMenu::Projection {
            t.widget_height * 2.0 + t.spacing * 2.0
        } else {
            0.0
        };
        let w = (anchor.w * 2.0).max(t.text_size * 11.0);
        let screen = ui.screen_rect();
        // The menu sits above its button and never covers the transport row;
        // long lists scroll.
        let room = (anchor.y - t.spacing - t.padding).max(item_h * 2.0);
        let max_list = (room - extra_h - t.padding * 2.0).max(item_h);
        let list_h = (entries.len() as f32 * (item_h + t.spacing) - t.spacing)
            .min(max_list)
            .max(0.0);
        let h = list_h + extra_h + t.padding * 2.0;
        let x = (anchor.center().x - w * 0.5)
            .clamp(t.padding, (screen.w - w - t.padding).max(t.padding));
        let y = (anchor.y - t.spacing - h).max(t.padding.min(anchor.y));
        let rect = Rect::new(x, y, w, h);

        let prev = ui.painter().set_layer(Layer::Popup);
        ui.painter().push_clip_absolute(rect.expand(2.0));
        ui.add_area(Layer::Popup, rect);
        ui.painter().rect_rounded(rect, t.corner_radius, t.surface);
        ui.painter()
            .rect_stroke(rect, t.corner_radius, 1.5, t.border);
        let mut close = false;
        ui.scope(("menu", menu as u8), |ui| {
            ui.region(rect.shrink(t.padding), Dir::Vertical, |ui| {
                if menu == PlayerMenu::Projection {
                    let mut sel = match view.stereo {
                        StereoMode::Mono => 0,
                        StereoMode::Sbs => 1,
                        StereoMode::Ou => 2,
                    };
                    if ui.segmented("stereo", &mut sel, &["2D", "SBS", "OU"]) {
                        actions.push(UiAction::SetStereo(
                            [StereoMode::Mono, StereoMode::Sbs, StereoMode::Ou][sel],
                        ));
                    }
                    let mut swap = view.swap_eyes;
                    if ui.toggle("Swap eyes", &mut swap).changed {
                        actions.push(UiAction::SetSwapEyes(swap));
                    }
                }
                let entries_ref = &entries;
                let picked = std::cell::Cell::new(None);
                let vr = ui.virtual_list("entries", entries.len(), item_h, list_h, |ui, i, r| {
                    let (label, selected, _) = &entries_ref[i];
                    let id = ui.make_id(i);
                    let style = ButtonStyle {
                        selected: *selected,
                        ..Default::default()
                    };
                    if ui.button_at(r, id, label, style).clicked {
                        picked.set(Some(i));
                    }
                    if *selected {
                        let s = t.icon_size * 0.6;
                        draw_icon(
                            ui.painter(),
                            Icon::Check,
                            Rect::new(r.right() - s - t.padding, r.center().y - s * 0.5, s, s),
                            t.on_accent,
                        );
                    }
                });
                if std::mem::take(&mut self.scroll_to_selected) {
                    if let Some(i) = entries.iter().position(|e| e.1) {
                        let top = i as f32 * (item_h + t.spacing);
                        let st = ui.scroll_state_mut(vr.id);
                        st.set_extent(
                            crate::layout::list_height(entries.len(), item_h, t.spacing),
                            list_h,
                        );
                        st.scroll_to_visible(top, top + item_h);
                    }
                }
                if let Some(i) = picked.get() {
                    actions.push(entries[i].2.clone());
                    // Projection menu stays open for further stereo tweaks.
                    close = menu != PlayerMenu::Projection;
                }
            });
        });
        ui.painter().pop_clip();
        ui.painter().set_layer(prev);
        let outside = ui
            .any_just_pressed()
            .is_some_and(|p| !rect.contains(p) && !anchor.contains(p));
        if close || outside || ui.take_back() {
            self.menu = None;
        }
    }
}

fn same_projection(a: &Projection, b: &Projection) -> bool {
    match (a, b) {
        (Projection::Flat { .. }, Projection::Flat { .. }) => true,
        (
            Projection::Fisheye {
                lens: la,
                fov_deg: fa,
                ..
            },
            Projection::Fisheye {
                lens: lb,
                fov_deg: fb,
                ..
            },
        ) => la == lb && (fa - fb).abs() < 0.5,
        _ => a == b,
    }
}

fn trim_float(v: f32) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_button_cycle() {
        let now = MediaTime::from_millis(5000);
        assert_eq!(next_loop_action(None, None, now), UiAction::SetLoopA(now));
        assert_eq!(
            next_loop_action(Some(MediaTime::from_millis(1000)), None, now),
            UiAction::SetLoopB(now)
        );
        assert_eq!(
            next_loop_action(Some(MediaTime::from_millis(9000)), None, now),
            UiAction::SetLoopA(now)
        );
        assert_eq!(
            next_loop_action(Some(MediaTime::ZERO), Some(now), now),
            UiAction::ClearLoop
        );
    }

    #[test]
    fn chapter_skipping() {
        let ch: Vec<Chapter> = [0, 60_000, 120_000]
            .iter()
            .map(|&ms| Chapter {
                start: MediaTime::from_millis(ms),
                title: String::new(),
            })
            .collect();
        assert_eq!(
            chapter_skip(&ch, MediaTime::from_millis(70_000), true),
            Some(MediaTime::from_millis(120_000))
        );
        assert_eq!(
            chapter_skip(&ch, MediaTime::from_millis(130_000), true),
            None
        );
        assert_eq!(
            chapter_skip(&ch, MediaTime::from_millis(70_000), false),
            Some(MediaTime::from_millis(60_000))
        );
        assert_eq!(
            chapter_skip(&ch, MediaTime::from_millis(61_000), false),
            Some(MediaTime::ZERO)
        );
        assert_eq!(
            chapter_skip(&ch, MediaTime::from_millis(1000), false),
            Some(MediaTime::ZERO)
        );
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(trim_float(1.0), "1");
        assert_eq!(trim_float(0.25), "0.25");
        assert_eq!(trim_float(1.5), "1.5");
        assert!(same_projection(
            &Projection::FLAT_DEFAULT,
            &Projection::Flat {
                width_m: 2.0,
                distance_m: 1.0,
                curvature: 0.5
            }
        ));
        assert!(!same_projection(
            &Projection::EQUIRECT_180,
            &Projection::EQUIRECT_360
        ));
        assert_eq!(projection_presets().len(), 10);
    }
}
