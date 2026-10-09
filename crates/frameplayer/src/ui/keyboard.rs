//! A virtual keyboard panel for text fields (search, addresses, passwords).

use super::theme::{self, Weight};
use super::{icons, widgets};
use egui::{Align2, Event, Key, Modifiers, Sense, Vec2};

const ROWS: [&str; 4] = ["1234567890-", "qwertyuiop/", "asdfghjkl:_", "zxcvbnm.@?&"];
const KEY_H: f32 = 60.0;
const GAP: f32 = 7.0;

/// How a key looks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    /// A character.
    Char,
    /// Shift, arrows, delete: a darker key.
    Function,
    /// Enter: the accent.
    Action,
    /// A function key that is on (shift).
    On,
}

/// One key; true when pressed. `label` may be an icon.
fn key(ui: &mut egui::Ui, label: &str, w: f32, style: Style, icon: bool) -> bool {
    let (_, resp) = ui.allocate_exact_size(Vec2::new(w, KEY_H), Sense::click());
    // Raised keys: Enter is the accent, shift glows while it is on, and
    // function keys keep a quieter label.
    let kind = if style == Style::Action {
        widgets::Kind::Primary
    } else {
        widgets::Kind::Secondary
    };
    let (face, fg) = widgets::key_face_kind(ui, &resp, 12.0, style == Style::On, kind);
    let fg = if style == Style::Function {
        fg.lerp_to_gamma(theme::TEXT_2, 0.5)
    } else {
        fg
    };
    let p = ui.painter();
    let font = if icon {
        theme::icon(26.0)
    } else if label.chars().count() > 1 {
        theme::font(Weight::SemiBold, 19.0)
    } else {
        theme::font(Weight::Medium, 25.0)
    };
    p.text(face.center(), Align2::CENTER_CENTER, label, font, fg);
    resp.clicked()
}

/// Draws the keyboard; returns the events to send to the focused panel and
/// whether the keyboard should close.
pub fn keyboard(ctx: &egui::Context, shift: &mut bool) -> (Vec<Event>, bool) {
    let mut out = Vec::new();
    let mut close = false;
    widgets::panel_slab(ctx, 24.0);
    let frame = egui::Frame::new()
        .outer_margin(widgets::SHADOW_ROOM)
        .inner_margin(egui::Margin::same(14));
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        ui.spacing_mut().item_spacing = Vec2::new(GAP, GAP);
        // 11 keys plus a 1.4-wide function key on the widest row.
        let key_w = (ui.available_width() - GAP * 12.0) / 12.4;
        for (i, row) in ROWS.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.add_space(i as f32 * key_w * 0.2);
                for c in row.chars() {
                    let ch = if *shift {
                        c.to_uppercase().next().unwrap_or(c)
                    } else {
                        c
                    };
                    if key(ui, &ch.to_string(), key_w, Style::Char, false) {
                        out.push(Event::Text(ch.to_string()));
                        *shift = false;
                    }
                }
                if i == 0 && key(ui, icons::BACKSPACE, key_w * 1.4, Style::Function, true) {
                    out.push(key_event(Key::Backspace));
                }
                if i == 1 && key(ui, icons::KEY_RETURN, key_w * 1.2, Style::Action, true) {
                    out.push(key_event(Key::Enter));
                    close = true;
                }
            });
        }
        ui.horizontal(|ui| {
            let shift_style = if *shift { Style::On } else { Style::Function };
            if key(ui, icons::ARROW_FAT_UP, key_w * 1.5, shift_style, true) {
                *shift = !*shift;
            }
            if key(ui, icons::CARET_LEFT, key_w, Style::Function, true) {
                out.push(key_event(Key::ArrowLeft));
            }
            if key(ui, icons::CARET_RIGHT, key_w, Style::Function, true) {
                out.push(key_event(Key::ArrowRight));
            }
            if key(ui, "space", key_w * 4.4, Style::Char, false) {
                out.push(Event::Text(" ".into()));
            }
            if key(ui, ".com", key_w * 1.5, Style::Function, false) {
                out.push(Event::Text(".com".into()));
            }
            if key(ui, "Done", ui.available_width(), Style::Function, false) {
                close = true;
            }
        });
    });
    (out, close)
}

fn key_event(key: Key) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    }
}
