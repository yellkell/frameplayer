//! A virtual keyboard panel for text fields (search, addresses, passwords).

use egui::{Event, Key, Modifiers, RichText, Vec2};

const ROWS: [&str; 4] = ["1234567890-", "qwertyuiop/", "asdfghjkl:_", "zxcvbnm.@?&"];

/// Draws the keyboard; returns the events to send to the focused panel and
/// whether the keyboard should close.
pub fn keyboard(ctx: &egui::Context, shift: &mut bool) -> (Vec<Event>, bool) {
    let mut out = Vec::new();
    let mut close = false;
    let frame = egui::Frame::new()
        .fill(super::theme::PANEL_BG)
        .corner_radius(16.0)
        .inner_margin(10.0);
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
        let key_w = (ui.available_width() - 6.0 * 11.0) / 12.0;
        let key = |ui: &mut egui::Ui, label: &str, w: f32| {
            ui.add(egui::Button::new(RichText::new(label).size(24.0)).min_size(Vec2::new(w, 56.0)))
                .clicked()
        };
        for (i, row) in ROWS.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.add_space(i as f32 * key_w * 0.2);
                for c in row.chars() {
                    let ch = if *shift {
                        c.to_uppercase().next().unwrap_or(c)
                    } else {
                        c
                    };
                    if key(ui, &ch.to_string(), key_w) {
                        out.push(Event::Text(ch.to_string()));
                        *shift = false;
                    }
                }
                if i == 0 && key(ui, "Del", key_w * 1.4) {
                    out.push(key_event(Key::Backspace));
                }
                if i == 1 && key(ui, "Enter", key_w * 1.2) {
                    out.push(key_event(Key::Enter));
                    close = true;
                }
            });
        }
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("Shift").size(24.0))
                        .selected(*shift)
                        .min_size(Vec2::new(key_w * 1.5, 56.0)),
                )
                .clicked()
            {
                *shift = !*shift;
            }
            if key(ui, "◀", key_w) {
                out.push(key_event(Key::ArrowLeft));
            }
            if key(ui, "▶", key_w) {
                out.push(key_event(Key::ArrowRight));
            }
            if key(ui, "space", key_w * 4.5) {
                out.push(Event::Text(" ".into()));
            }
            if key(ui, ".com", key_w * 1.5) {
                out.push(Event::Text(".com".into()));
            }
            if key(ui, "Done", key_w * 1.8) {
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
