//! The Web XR tab: a web address opened in the Frame's WebXR browser,
//! Chromium XR, which takes over the headset. Defaults to Fish & Chips.

use super::{Action, View, big_button, theme};
use crate::webxr::{self, Browser};
use egui::RichText;

pub fn web(ui: &mut egui::Ui, v: &mut View) {
    ui.label(RichText::new("Web XR").heading());
    ui.label(
        RichText::new(
            "Opens a web page in Chromium XR, the Frame's WebXR browser. FramePlayer closes \
             while it runs: press Enter VR on the page to play, and start FramePlayer again \
             from your library afterwards. Type any address in the browser to go elsewhere.",
        )
        .color(theme::MUTED),
    );
    ui.add_space(10.0);

    if v.state.web_url.is_empty() {
        v.state.web_url = v.settings.web_home.clone();
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut v.state.web_url)
                .hint_text(webxr::DEFAULT_URL)
                .font(egui::TextStyle::Heading)
                .desired_width(ui.available_width() - 140.0),
        );
        if ui.button("Reset").clicked() {
            v.state.web_url = webxr::DEFAULT_URL.into();
        }
    });
    let url = webxr::normalize_url(&v.state.web_url);
    let browser = webxr::find_browser();
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if big_button(ui, "Open in Chromium XR", false).clicked()
            && browser.is_some()
            && let Ok(u) = &url
        {
            v.actions.push(Action::LaunchWeb(u.clone()));
        }
        if let Err(e) = &url {
            ui.label(RichText::new(e).color(theme::WARN));
        }
    });
    ui.add_space(10.0);

    match &browser {
        Some(b) => {
            let home = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default();
            let how = match (b, webxr::steam_appid(&home)) {
                (_, Some(_)) => "opens as its own Steam app",
                (Browser::Launcher(_), None) => "launcher, no Steam entry",
                (Browser::Chrome(_), None) => "bare build, no Steam entry",
            };
            ui.label(
                RichText::new(format!("Chromium XR at {} ({how})", b.path().display()))
                    .small()
                    .color(theme::OK),
            );
        }
        None => {
            egui::Frame::new().fill(theme::CARD_BG).corner_radius(12.0).inner_margin(12.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Chromium XR is not installed").strong().color(theme::WARN));
                ui.label("WebXR needs a browser built for the Steam Frame. Install it once from a PC:");
                ui.label(RichText::new(webxr::BROWSER_PROJECT).monospace());
                ui.label(
                    RichText::new("The Chromium XR for Steam Frame release (ChromiumXR-Frame-arm64.zip) goes in ~/chromium-xr-frame. FramePlayer also finds saphid's build in ~/.local/bin/chromium-xr.")
                        .small()
                        .color(theme::MUTED),
                );
            });
        }
    }
    // Chromium offers immersive-ar only when the runtime can blend with the
    // real world (ALPHA_BLEND or ADDITIVE), the same check as FramePlayer's
    // own passthrough.
    if !v.passthrough_available {
        ui.label(
            RichText::new(
                "This headset's runtime does not offer passthrough to apps, so pages that need \
                 immersive-ar (mixed reality) will report XR as unavailable; immersive-vr pages work.",
            )
            .small()
            .color(theme::MUTED),
        );
    }
}
