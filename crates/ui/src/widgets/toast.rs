//! Transient toast notifications stacked at the bottom of the panel.

use crate::geom::{Rect, Vec2};
use crate::painter::Layer;
use crate::text::TextParams;
use crate::ui::Ui;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToastKind {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    /// Total lifetime in seconds.
    pub duration: f32,
    pub age: f32,
}

/// Fade-in/out time at each end of a toast's life.
const FADE: f32 = 0.25;
/// At most this many toasts are visible; older ones are dropped.
pub const MAX_TOASTS: usize = 4;

impl Toast {
    /// Opacity from the fade envelope.
    pub fn alpha(&self) -> f32 {
        let a_in = (self.age / FADE).min(1.0);
        let a_out = ((self.duration - self.age) / FADE).min(1.0);
        a_in.min(a_out).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Toasts {
    pub items: Vec<Toast>,
}

impl Toasts {
    pub fn push(&mut self, text: impl Into<String>, kind: ToastKind, duration: f32) {
        let text = text.into();
        // Repeated identical message: restart it instead of stacking.
        if let Some(t) = self
            .items
            .iter_mut()
            .find(|t| t.text == text && t.kind == kind)
        {
            t.age = t.age.min(FADE);
            t.duration = duration;
            return;
        }
        self.items.push(Toast {
            text,
            kind,
            duration: duration.max(2.0 * FADE),
            age: 0.0,
        });
        if self.items.len() > MAX_TOASTS {
            self.items.remove(0);
        }
    }

    pub fn update(&mut self, dt: f32) {
        for t in &mut self.items {
            t.age += dt;
        }
        self.items.retain(|t| t.age < t.duration);
    }
}

impl Ui {
    /// Shows a toast for `seconds`.
    pub fn toast(&mut self, text: impl Into<String>, kind: ToastKind, seconds: f32) {
        self.toasts.push(text, kind, seconds);
    }

    pub(crate) fn draw_toasts(&mut self) {
        let dt = self.dt;
        self.toasts.update(dt);
        if self.toasts.items.is_empty() {
            return;
        }
        let t = self.theme.clone();
        let prev = self.painter.set_layer(Layer::Tooltip);
        self.painter.push_clip_absolute(self.screen_rect());
        let max_w = (self.size.x * 0.6).max(200.0);
        let mut y = self.size.y - t.padding * 2.0;
        let items = self.toasts.items.clone();
        for toast in items.iter().rev() {
            let a = toast.alpha();
            let layout = self.fonts.layout(
                &toast.text,
                TextParams::new(t.text_size)
                    .width(max_w - t.padding * 2.0)
                    .wrap()
                    .lines(3)
                    .ellipsis(),
            );
            let size = layout.size + Vec2::new(t.padding * 2.0 + 6.0, t.padding * 1.5);
            y -= size.y;
            let rect = Rect::new((self.size.x - size.x) * 0.5, y, size.x, size.y);
            let accent = match toast.kind {
                ToastKind::Info => t.accent,
                ToastKind::Success => t.success,
                ToastKind::Warning => t.warning,
                ToastKind::Error => t.danger,
            };
            self.painter
                .rect_rounded(rect, t.corner_radius, t.surface_active.alpha(a));
            self.painter
                .rect_rounded(Rect::new(rect.x, rect.y, 6.0, rect.h), 3.0, accent.alpha(a));
            self.draw_text_at(
                Vec2::new(rect.x + t.padding + 6.0, rect.y + t.padding * 0.75),
                &layout,
                t.text.alpha(a),
            );
            y -= t.spacing;
        }
        self.painter.pop_clip();
        self.painter.set_layer(prev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_lifecycle() {
        let mut t = Toasts::default();
        t.push("Saved", ToastKind::Success, 2.0);
        assert_eq!(t.items[0].alpha(), 0.0);
        t.update(0.5);
        assert_eq!(t.items[0].alpha(), 1.0);
        t.update(1.4);
        assert!(t.items[0].alpha() < 1.0);
        t.update(0.2);
        assert!(t.items.is_empty());
    }

    #[test]
    fn dedupe_and_cap() {
        let mut t = Toasts::default();
        t.push("x", ToastKind::Info, 3.0);
        t.update(1.0);
        t.push("x", ToastKind::Info, 3.0);
        assert_eq!(t.items.len(), 1);
        assert!(t.items[0].age <= FADE);
        for i in 0..10 {
            t.push(format!("m{i}"), ToastKind::Error, 3.0);
        }
        assert_eq!(t.items.len(), MAX_TOASTS);
        assert_eq!(t.items.last().unwrap().text, "m9");
    }
}
