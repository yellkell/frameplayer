//! Single-line text field with caret, fed by [`TextEvent`]s from the
//! virtual keyboard, a hardware keyboard or the web remote.

use crate::geom::{Rect, Vec2};
use crate::icons::{draw_icon, Icon};
use crate::input::TextEvent;
use crate::text::TextParams;
use crate::ui::{Response, Sense, TextEditState, Ui};
use std::hash::Hash;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TextInputResponse {
    pub response: Response,
    pub changed: bool,
    /// Enter pressed while focused.
    pub submitted: bool,
    /// Gained keyboard focus this frame (show the virtual keyboard).
    pub gained_focus: bool,
    pub has_focus: bool,
}

/// Applies one edit to `text`/`caret`. Returns `(changed, submitted)`.
pub fn apply_text_event(
    text: &mut String,
    caret: &mut usize,
    ev: &TextEvent,
    max_len: Option<usize>,
) -> (bool, bool) {
    *caret = clamp_boundary(text, *caret);
    match ev {
        TextEvent::Text(s) => {
            let s: String = s.chars().filter(|c| !c.is_control()).collect();
            let room = max_len
                .map(|m| m.saturating_sub(text.chars().count()))
                .unwrap_or(usize::MAX);
            let s: String = s.chars().take(room).collect();
            if s.is_empty() {
                return (false, false);
            }
            text.insert_str(*caret, &s);
            *caret += s.len();
            (true, false)
        }
        TextEvent::Backspace => match prev_boundary(text, *caret) {
            Some(p) => {
                text.replace_range(p..*caret, "");
                *caret = p;
                (true, false)
            }
            None => (false, false),
        },
        TextEvent::Delete => match next_boundary(text, *caret) {
            Some(n) => {
                text.replace_range(*caret..n, "");
                (true, false)
            }
            None => (false, false),
        },
        TextEvent::Left => {
            *caret = prev_boundary(text, *caret).unwrap_or(0);
            (false, false)
        }
        TextEvent::Right => {
            *caret = next_boundary(text, *caret).unwrap_or(text.len());
            (false, false)
        }
        TextEvent::Home => {
            *caret = 0;
            (false, false)
        }
        TextEvent::End => {
            *caret = text.len();
            (false, false)
        }
        TextEvent::Enter => (false, true),
    }
}

fn clamp_boundary(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn prev_boundary(text: &str, i: usize) -> Option<usize> {
    text[..i].char_indices().next_back().map(|(p, _)| p)
}

fn next_boundary(text: &str, i: usize) -> Option<usize> {
    text[i..].chars().next().map(|c| i + c.len_utf8())
}

impl Ui {
    /// Text field `width` px wide ([`crate::layout::FILL`] for the row).
    /// Clicking it (or A when focused) takes keyboard focus.
    pub fn text_input(
        &mut self,
        key: impl Hash,
        text: &mut String,
        hint: &str,
        width: f32,
    ) -> TextInputResponse {
        let rect = self.allocate(Vec2::new(width, self.theme.widget_height));
        let id = self.make_id(("text_input", key));
        self.text_input_at(rect, id, text, hint, None)
    }

    /// Text field with a leading magnifier icon.
    pub fn search_input(
        &mut self,
        key: impl Hash,
        text: &mut String,
        hint: &str,
        width: f32,
    ) -> TextInputResponse {
        let rect = self.allocate(Vec2::new(width, self.theme.widget_height));
        let id = self.make_id(("text_input", key));
        self.text_input_at(rect, id, text, hint, Some(Icon::Search))
    }

    pub fn text_input_at(
        &mut self,
        rect: Rect,
        id: crate::ui::Id,
        text: &mut String,
        hint: &str,
        icon: Option<Icon>,
    ) -> TextInputResponse {
        let t = self.theme.clone();
        let resp = self.interact(rect, id, Sense::CLICK);
        self.record(id, rect, || hint.to_string());
        let mut out = TextInputResponse {
            response: resp,
            ..Default::default()
        };
        let icon_s = if icon.is_some() {
            t.icon_size * 0.75
        } else {
            0.0
        };
        let icon_gap = if icon.is_some() { t.padding * 0.5 } else { 0.0 };
        let inner = Rect::new(
            rect.x + t.padding + icon_s + icon_gap,
            rect.y,
            (rect.w - t.padding * 2.0 - icon_s - icon_gap).max(0.0),
            rect.h,
        );
        let mut st = self.edit_states.get(&id).copied().unwrap_or(TextEditState {
            caret: text.len(),
            scroll_x: 0.0,
        });
        st.caret = st.caret.min(text.len());

        if resp.clicked && self.text_focus != Some(id) {
            self.set_text_focus(Some(id));
            out.gained_focus = true;
            st.caret = text.len();
        } else if resp.clicked {
            if let Some(p) = resp.pointer {
                let layout = self.fonts.layout(text, TextParams::new(t.text_size));
                st.caret = layout.hit_byte(p.x - inner.x + st.scroll_x, text.len());
            }
        }
        let focused = self.text_focus == Some(id);
        if focused {
            self.set_text_focus(Some(id));
            for ev in std::mem::take(&mut self.text_queue) {
                let (c, s) = apply_text_event(text, &mut st.caret, &ev, Some(256));
                out.changed |= c;
                out.submitted |= s;
            }
        }
        out.has_focus = focused;

        // Visuals.
        let bg = if focused {
            t.surface_active
        } else {
            self.surface_color(&resp)
        };
        self.painter.rect_rounded(rect, t.corner_radius, bg);
        if focused {
            self.painter
                .rect_stroke(rect, t.corner_radius, 2.0, t.accent);
        }
        if let Some(icon) = icon {
            draw_icon(
                &mut self.painter,
                icon,
                Rect::new(
                    rect.x + t.padding,
                    rect.center().y - icon_s * 0.5,
                    icon_s,
                    icon_s,
                ),
                t.text_dim,
            );
        }
        let layout = self.fonts.layout(text, TextParams::new(t.text_size));
        let caret_x = layout.caret_x(st.caret);
        // Keep the caret visible inside the field.
        if caret_x - st.scroll_x > inner.w {
            st.scroll_x = caret_x - inner.w;
        } else if caret_x < st.scroll_x {
            st.scroll_x = caret_x;
        }
        st.scroll_x = st.scroll_x.clamp(0.0, (layout.size.x - inner.w).max(0.0));
        self.painter.push_clip(inner.expand(2.0));
        let ty = rect.y + (rect.h - layout.size.y) * 0.5;
        if text.is_empty() {
            let hl = self
                .fonts
                .layout(hint, TextParams::new(t.text_size).width(inner.w).ellipsis());
            self.draw_text_at(Vec2::new(inner.x, ty), &hl, t.text_dim);
        } else {
            self.draw_text_at(Vec2::new(inner.x - st.scroll_x, ty), &layout, t.text);
        }
        if focused && (self.time * 2.0).fract() < 0.6 {
            let cx = inner.x + caret_x - st.scroll_x;
            self.painter.rect_filled(
                Rect::new(cx, rect.y + rect.h * 0.22, 2.0, rect.h * 0.56),
                t.accent,
            );
        }
        self.painter.pop_clip();
        self.focus_ring(&resp, t.corner_radius);
        self.edit_states.insert(id, st);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_ops() {
        let mut s = String::new();
        let mut c = 0;
        assert_eq!(
            apply_text_event(&mut s, &mut c, &TextEvent::Text("héllo".into()), None),
            (true, false)
        );
        assert_eq!((s.as_str(), c), ("héllo", 6));
        apply_text_event(&mut s, &mut c, &TextEvent::Left, None);
        apply_text_event(&mut s, &mut c, &TextEvent::Left, None);
        apply_text_event(&mut s, &mut c, &TextEvent::Left, None);
        apply_text_event(&mut s, &mut c, &TextEvent::Left, None);
        assert_eq!(c, 1);
        apply_text_event(&mut s, &mut c, &TextEvent::Delete, None);
        assert_eq!(s, "hllo");
        apply_text_event(&mut s, &mut c, &TextEvent::Text("e".into()), None);
        assert_eq!(s, "hello");
        apply_text_event(&mut s, &mut c, &TextEvent::End, None);
        apply_text_event(&mut s, &mut c, &TextEvent::Backspace, None);
        assert_eq!((s.as_str(), c), ("hell", 4));
        apply_text_event(&mut s, &mut c, &TextEvent::Home, None);
        assert_eq!(
            apply_text_event(&mut s, &mut c, &TextEvent::Backspace, None),
            (false, false)
        );
        assert_eq!(
            apply_text_event(&mut s, &mut c, &TextEvent::Enter, None),
            (false, true)
        );
        // Length limit and control chars.
        let mut c2 = 0;
        let mut short = String::new();
        apply_text_event(
            &mut short,
            &mut c2,
            &TextEvent::Text("ab\ncdef".into()),
            Some(3),
        );
        assert_eq!(short, "abc");
        // Caret on a non-boundary gets fixed up.
        let mut s3 = "日本".to_string();
        let mut c3 = 4;
        apply_text_event(&mut s3, &mut c3, &TextEvent::Backspace, None);
        assert_eq!(s3, "本");
    }
}
