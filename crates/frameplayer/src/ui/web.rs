//! The Web XR tab: WebXR pages (games, experiences) opened in the Frame's
//! WebXR browser, which takes over the headset until it is closed.

use super::{Action, View, big_button, theme};
use crate::webxr::{self, Browser, WebApp};
use egui::RichText;

pub fn web(ui: &mut egui::Ui, v: &mut View) {
    ui.label(RichText::new("Web XR").heading());
    ui.label(
        RichText::new(
            "Opens a WebXR page in Chromium XR. FramePlayer steps aside while it runs; \
             press Enter VR on the page, and close the browser to come back here.",
        )
        .color(theme::MUTED),
    );
    ui.add_space(6.0);
    let browser = webxr::find_browser();
    match &browser {
        Some(b) => {
            let how = match b {
                Browser::Launcher(_) => "launcher",
                Browser::Chrome(_) => "build",
            };
            ui.label(
                RichText::new(format!(
                    "Browser: Chromium XR {how} at {}",
                    b.path().display()
                ))
                .small()
                .color(theme::OK),
            );
        }
        None => {
            egui::Frame::new().fill(theme::CARD_BG).corner_radius(12.0).inner_margin(12.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Chromium XR is not installed").strong().color(theme::WARN));
                ui.label("WebXR needs a browser built for the Steam Frame. Install it once from a PC or Desktop Mode:");
                ui.label(RichText::new(webxr::BROWSER_PROJECT).monospace());
                ui.label(
                    RichText::new("Download its arm64 release, then run frame/install.sh on the headset. FramePlayer finds it in ~/.local/bin/chromium-xr.")
                        .small()
                        .color(theme::MUTED),
                );
            });
        }
    }
    ui.add_space(8.0);
    let mut remove = None;
    egui::ScrollArea::vertical()
        .max_height(ui.available_height() - 150.0)
        .show(ui, |ui| {
            for (i, app) in v.settings.web_apps.iter().enumerate() {
                egui::Frame::new()
                    .fill(theme::CARD_BG)
                    .corner_radius(12.0)
                    .inner_margin(12.0)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&app.name).size(22.0).strong());
                                ui.label(RichText::new(&app.url).color(theme::MUTED));
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .button(RichText::new("Remove").color(theme::ERROR))
                                        .clicked()
                                    {
                                        remove = Some(i);
                                    }
                                    if ui
                                        .add_enabled(
                                            browser.is_some(),
                                            egui::Button::new(RichText::new("▶ Launch").size(21.0))
                                                .min_size(egui::vec2(120.0, 48.0)),
                                        )
                                        .clicked()
                                    {
                                        v.actions.push(Action::LaunchWeb(app.url.clone()));
                                    }
                                },
                            );
                        });
                    });
                ui.add_space(4.0);
            }
        });
    if let Some(i) = remove {
        v.actions.push(Action::RemoveWebApp(i));
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut v.state.web_name)
                .hint_text("Name")
                .desired_width(180.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut v.state.web_url)
                .hint_text("https://example.com/my-webxr-game")
                .desired_width(420.0),
        );
    });
    let url = webxr::normalize_url(&v.state.web_url);
    ui.horizontal(|ui| {
        let ok = url.is_ok();
        if ui
            .add_enabled(ok, egui::Button::new("Add to list"))
            .clicked()
            && let Ok(u) = &url
        {
            let name = if v.state.web_name.trim().is_empty() {
                u.split("://")
                    .nth(1)
                    .unwrap_or(u)
                    .trim_end_matches('/')
                    .to_string()
            } else {
                v.state.web_name.trim().to_string()
            };
            v.actions.push(Action::AddWebApp(WebApp {
                name,
                url: u.clone(),
            }));
            v.state.web_name.clear();
            v.state.web_url.clear();
        }
        if big_button(ui, "Open now", false).clicked()
            && ok
            && browser.is_some()
            && let Ok(u) = &url
        {
            v.actions.push(Action::LaunchWeb(u.clone()));
        }
        if let (Err(e), false) = (&url, v.state.web_url.trim().is_empty()) {
            ui.label(RichText::new(e).color(theme::WARN));
        }
    });
}
