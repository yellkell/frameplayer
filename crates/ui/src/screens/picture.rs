//! HereSphere-style picture adjustment: stereo alignment, lens distortion,
//! orientation and colour sliders, plus keyframing at the current time.

use super::{panel_background, UiAction};
use crate::geom::Vec2;
use crate::icons::Icon;
use crate::layout::{Dir, FILL};
use crate::ui::Ui;
use crate::widgets::basic::ButtonStyle;
use crate::widgets::slider::SliderOpts;
use crate::widgets::timeline::format_time;
use fp_core::{Corrections, MediaTime};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PictureView {
    /// Corrections in effect now (interpolated if keyframed).
    pub corrections: Corrections,
    pub keyframe_count: usize,
    pub position: MediaTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PictureTab {
    #[default]
    Stereo,
    Lens,
    Orientation,
    Colour,
}

impl PictureTab {
    pub const ALL: [PictureTab; 4] = [
        PictureTab::Stereo,
        PictureTab::Lens,
        PictureTab::Orientation,
        PictureTab::Colour,
    ];
    pub fn label(self) -> &'static str {
        match self {
            PictureTab::Stereo => "Stereo",
            PictureTab::Lens => "Lens",
            PictureTab::Orientation => "Orientation",
            PictureTab::Colour => "Colour",
        }
    }
}

/// One slider definition: label, accessor, range, step, decimals, suffix.
type Field = (
    &'static str,
    fn(&mut Corrections) -> &mut f32,
    f32,
    f32,
    f32,
    usize,
    &'static str,
);

/// Slider rows per tab.
pub fn fields(tab: PictureTab) -> Vec<Field> {
    match tab {
        PictureTab::Stereo => vec![
            ("IPD", |c| &mut c.ipd_deg, -5.0, 5.0, 0.05, 2, "°"),
            (
                "Vertical align",
                |c| &mut c.vertical_align_deg,
                -3.0,
                3.0,
                0.02,
                2,
                "°",
            ),
            (
                "Horizontal align",
                |c| &mut c.horizontal_align_deg,
                -3.0,
                3.0,
                0.02,
                2,
                "°",
            ),
        ],
        PictureTab::Lens => vec![
            ("k1", |c| &mut c.k1, -0.5, 0.5, 0.005, 3, ""),
            ("k2", |c| &mut c.k2, -0.5, 0.5, 0.005, 3, ""),
            ("Zoom", |c| &mut c.zoom, 0.5, 2.0, 0.01, 2, "×"),
        ],
        PictureTab::Orientation => vec![
            ("Tilt", |c| &mut c.pitch_deg, -45.0, 45.0, 0.5, 1, "°"),
            ("Roll", |c| &mut c.roll_deg, -45.0, 45.0, 0.5, 1, "°"),
            ("Yaw", |c| &mut c.yaw_deg, -180.0, 180.0, 1.0, 0, "°"),
        ],
        PictureTab::Colour => vec![
            (
                "Exposure",
                |c| &mut c.exposure_ev,
                -3.0,
                3.0,
                0.05,
                2,
                " EV",
            ),
            ("Contrast", |c| &mut c.contrast, 0.5, 2.0, 0.01, 2, ""),
            ("Saturation", |c| &mut c.saturation, 0.0, 2.0, 0.01, 2, ""),
            ("Sharpen", |c| &mut c.sharpen, 0.0, 1.0, 0.01, 2, ""),
        ],
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PictureAdjustScreen {
    pub tab: PictureTab,
}

impl PictureAdjustScreen {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn show(&mut self, ui: &mut Ui, view: &PictureView) -> Vec<UiAction> {
        let mut actions = Vec::new();
        let screen = panel_background(ui);
        let t = ui.theme.clone();
        ui.region(screen.shrink(t.padding * 1.5), Dir::Vertical, |ui| {
            ui.row(t.widget_height, |ui| {
                if ui.icon_button("close", Icon::Back).clicked {
                    actions.push(UiAction::ClosePictureAdjust);
                }
                ui.add_space(t.spacing);
                ui.label_styled("Picture", t.title_text_size, t.text);
            });
            let mut tab = PictureTab::ALL
                .iter()
                .position(|x| *x == self.tab)
                .unwrap_or(0);
            let labels: Vec<&str> = PictureTab::ALL.iter().map(|x| x.label()).collect();
            if ui.tabs("tabs", &mut tab, &labels) {
                self.tab = PictureTab::ALL[tab];
            }
            let footer = t.widget_height + t.spacing * 2.0;
            let h = (ui.available().h - footer).max(t.widget_height);
            let mut c = view.corrections;
            let tab = self.tab;
            ui.scroll_area(("picture", tab as u8), h, |ui| {
                for (label, get, lo, hi, step, dec, suffix) in fields(tab) {
                    let opts = SliderOpts::default()
                        .step(step)
                        .decimals(dec)
                        .suffix(suffix);
                    ui.slider(label, get(&mut c), lo..=hi, &opts);
                }
            });
            if c != view.corrections {
                actions.push(UiAction::SetCorrections(c));
            }
            ui.row(t.widget_height, |ui| {
                let kf = if view.keyframe_count > 0 {
                    format!(
                        "Keyframe @ {} ({})",
                        format_time(view.position),
                        view.keyframe_count
                    )
                } else {
                    format!("Keyframe @ {}", format_time(view.position))
                };
                if ui
                    .button_ex(
                        &kf,
                        ButtonStyle {
                            icon: Some(Icon::Plus),
                            primary: true,
                            ..Default::default()
                        },
                    )
                    .clicked
                {
                    actions.push(UiAction::AddKeyframe);
                }
                if view.keyframe_count > 0 && ui.button("Clear keyframes").clicked {
                    actions.push(UiAction::ClearKeyframes);
                }
                let avail = ui.available();
                let reset_w = ui.fonts.measure("Reset", t.text_size) + t.padding * 2.0;
                ui.allocate(Vec2::new((avail.w - reset_w - t.spacing).max(0.0), FILL));
                if ui
                    .button_ex(
                        "Reset",
                        ButtonStyle {
                            danger: true,
                            ..Default::default()
                        },
                    )
                    .clicked
                {
                    actions.push(UiAction::ResetCorrections);
                }
            });
        });
        if ui.take_back() {
            actions.push(UiAction::ClosePictureAdjust);
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_within_slider_ranges() {
        let mut c = Corrections::default();
        for tab in PictureTab::ALL {
            for (label, get, lo, hi, ..) in fields(tab) {
                let v = *get(&mut c);
                assert!(
                    (lo..=hi).contains(&v),
                    "{label} default {v} outside {lo}..{hi}"
                );
            }
        }
    }
}
