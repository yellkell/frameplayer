//! The Web XR tab: the Frame's WebXR browser, Chromium XR, embedded in the
//! panel. Defaults to Fish & Chips. The laser clicks and scrolls the page,
//! the virtual keyboard types into it, and Enter VR hands the headset to the
//! page until its VR session ends (see `webview`).

use super::{Action, View, big_button, theme};
use crate::webview::{self, Key, WebView};
use crate::webxr;
use egui::{RichText, Sense, Vec2};

pub fn web(ui: &mut egui::Ui, v: &mut View) {
    let Some(w) = v.web.as_deref_mut() else {
        not_running(ui, v);
        return;
    };

    // Address bar: follows the page unless someone is typing in it.
    let address_id = egui::Id::new("web-address");
    let typing = ui.memory(|m| m.has_focus(address_id));
    if !typing {
        let url = w.url();
        if !url.is_empty() {
            v.state.web_url = url;
        } else if v.state.web_url.is_empty() {
            v.state.web_url = v.settings.web_home.clone();
        }
    }
    let page_id = egui::Id::new("web-page");
    ui.horizontal(|ui| {
        if ui.button(RichText::new("◀").heading()).clicked() {
            w.back();
        }
        if ui.button(RichText::new("▶").heading()).clicked() {
            w.forward();
        }
        if ui.button(RichText::new("⟳").heading()).clicked() {
            w.reload();
        }
        let r = ui.add(
            egui::TextEdit::singleline(&mut v.state.web_url)
                .id(address_id)
                .hint_text(webxr::DEFAULT_URL)
                .font(egui::TextStyle::Heading)
                .desired_width(ui.available_width() - 330.0),
        );
        let entered = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.button(RichText::new("Go").heading()).clicked() || entered {
            match webxr::normalize_url(&v.state.web_url) {
                Ok(url) => {
                    w.navigate(&url);
                    // Remembered: the tab opens here next time.
                    v.settings.web_home = url;
                }
                Err(e) => v.state.toast(e),
            }
        }
        let typing_page = ui.memory(|m| m.has_focus(page_id));
        if big_button(ui, "Keyboard", typing_page).clicked() {
            ui.memory_mut(|m| {
                if typing_page {
                    m.surrender_focus(page_id);
                } else {
                    m.request_focus(page_id);
                }
            });
        }
        if ui
            .button("Open as app")
            .on_hover_text("Open this page in Chromium XR on its own, outside FramePlayer")
            .clicked()
        {
            v.actions.push(Action::LaunchWeb(w.url()));
        }
    });
    ui.add_space(6.0);

    let page_aspect = webview::VIEW_W as f32 / webview::VIEW_H as f32;
    let avail = ui.available_size();
    let size = if avail.x / avail.y > page_aspect {
        Vec2::new(avail.y * page_aspect, avail.y)
    } else {
        Vec2::new(avail.x, avail.x / page_aspect)
    };
    let (_, rect) = ui.allocate_space(size);
    let resp = ui.interact(rect, page_id, Sense::click_and_drag());
    match w.texture(ui.ctx()) {
        Some(tex) => {
            ui.painter().image(
                tex.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        None => {
            ui.painter().rect_filled(rect, 8.0, theme::CARD_BG);
            let text = match w.error() {
                Some(e) => e,
                None => "Loading...".into(),
            };
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(24.0),
                theme::MUTED,
            );
        }
    }
    forward_input(ui, w, &resp, rect, page_id);
}

/// Sends the laser, scrolling and the virtual keyboard to the page.
fn forward_input(
    ui: &mut egui::Ui,
    w: &mut WebView,
    resp: &egui::Response,
    rect: egui::Rect,
    page_id: egui::Id,
) {
    let at = resp.hover_pos().map(|p| webview::to_page(p, rect));
    let (pressed, released, scroll) = ui.input(|i| {
        (
            i.pointer.primary_pressed(),
            i.pointer.primary_released(),
            i.raw_scroll_delta,
        )
    });
    if let Some((x, y)) = at {
        w.mouse_move(x, y);
        if pressed {
            w.mouse_button(x, y, true);
        }
        if scroll != Vec2::ZERO {
            // egui scrolls content down for positive y; the DOM the other way.
            w.wheel(x, y, -scroll.x, -scroll.y);
        }
    }
    if released {
        w.mouse_release(at);
    }

    if ui.memory(|m| m.has_focus(page_id)) {
        // Keep arrows and Tab for the page instead of moving egui's focus.
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                page_id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: false,
                },
            )
        });
        let events = ui.input(|i| i.events.clone());
        for e in events {
            match e {
                egui::Event::Text(t) => w.text(&t),
                egui::Event::Key {
                    key: egui::Key::Escape,
                    pressed: true,
                    ..
                } => ui.memory_mut(|m| m.surrender_focus(page_id)),
                egui::Event::Key {
                    key, pressed: true, ..
                } => {
                    if let Some(k) = Key::from_egui(key) {
                        w.key(k);
                    }
                }
                _ => {}
            }
        }
    }
}

/// The browser is starting, failed to start, or isn't installed.
fn not_running(ui: &mut egui::Ui, v: &mut View) {
    ui.label(RichText::new("Web XR").heading());
    ui.add_space(10.0);
    match v.web_status {
        None => {
            ui.label(RichText::new("Starting the browser...").color(theme::MUTED));
        }
        Some(s) if s.starts_with("Starting") => {
            ui.label(RichText::new(s).color(theme::MUTED));
        }
        Some(err) => {
            ui.label(RichText::new(err).color(theme::WARN));
            ui.add_space(8.0);
            if big_button(ui, "Try again", false).clicked() {
                v.actions.push(Action::WebRetry);
            }
            ui.add_space(10.0);
            egui::Frame::new()
                .fill(theme::CARD_BG)
                .corner_radius(12.0)
                .inner_margin(12.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new("Chromium XR for Steam Frame")
                            .strong()
                            .color(theme::ACCENT),
                    );
                    ui.label(
                        "WebXR needs a browser built for the Steam Frame. Install the \
                         ChromiumXR-Frame-arm64.zip release into ~/chromium-xr-frame:",
                    );
                    ui.label(RichText::new(webxr::BROWSER_PROJECT).monospace());
                });
        }
    }
}
