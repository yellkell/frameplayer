//! On-panel virtual keyboard (QWERTY + symbols page, shift / caps lock,
//! backspace with auto-repeat, enter, caret keys).
//!
//! Key presses become [`TextEvent`]s, returned to the caller and also queued
//! for whichever text field currently has keyboard focus.

use crate::geom::{Rect, Vec2};
use crate::icons::{draw_icon, Icon};
use crate::input::TextEvent;
use crate::layout::FILL;
use crate::text::Align;
use crate::ui::{Sense, Ui};
use std::hash::Hash;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShiftState {
    #[default]
    Off,
    /// Next letter only.
    Once,
    Locked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyboardState {
    pub shift: ShiftState,
    pub symbols: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Char(char),
    Space,
    Backspace,
    Enter,
    Shift,
    Symbols,
    Left,
    Right,
    Hide,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Key {
    pub action: KeyAction,
    /// Width in key units.
    pub width: f32,
}

const fn k(c: char) -> Key {
    Key {
        action: KeyAction::Char(c),
        width: 1.0,
    }
}

const fn wide(action: KeyAction, width: f32) -> Key {
    Key { action, width }
}

/// The key grid for a page.
pub fn keyboard_rows(symbols: bool) -> Vec<Vec<Key>> {
    let chars = |s: &str| s.chars().map(k).collect::<Vec<_>>();
    let mut rows = vec![chars("1234567890")];
    rows[0].push(wide(KeyAction::Backspace, 1.5));
    if symbols {
        rows.push(chars("@#$%&*-+()"));
        let mut r2 = chars("!\"':;/?_=");
        r2.push(wide(KeyAction::Enter, 1.5));
        rows.push(r2);
        rows.push(chars("~`|\\<>[]{}"));
    } else {
        rows.push(chars("qwertyuiop"));
        let mut r2 = chars("asdfghjkl");
        r2.push(wide(KeyAction::Enter, 1.5));
        rows.push(r2);
        let mut r3 = vec![wide(KeyAction::Shift, 1.5)];
        r3.extend(chars("zxcvbnm,."));
        rows.push(r3);
    }
    rows.push(vec![
        wide(KeyAction::Symbols, 1.5),
        k(if symbols { ',' } else { '@' }),
        wide(KeyAction::Space, 5.0),
        k(if symbols { '.' } else { '-' }),
        wide(KeyAction::Left, 1.0),
        wide(KeyAction::Right, 1.0),
        wide(KeyAction::Hide, 1.5),
    ]);
    rows
}

/// Keyboard state machine: applies `action`, returning the text event to emit.
pub fn press_key(state: &mut KeyboardState, action: KeyAction) -> Option<TextEvent> {
    match action {
        KeyAction::Char(c) => {
            let upper = state.shift != ShiftState::Off && !state.symbols;
            if state.shift == ShiftState::Once {
                state.shift = ShiftState::Off;
            }
            Some(TextEvent::Text(if upper {
                c.to_uppercase().collect()
            } else {
                c.to_string()
            }))
        }
        KeyAction::Space => Some(TextEvent::Text(" ".into())),
        KeyAction::Backspace => Some(TextEvent::Backspace),
        KeyAction::Enter => Some(TextEvent::Enter),
        KeyAction::Left => Some(TextEvent::Left),
        KeyAction::Right => Some(TextEvent::Right),
        KeyAction::Shift => {
            state.shift = match state.shift {
                ShiftState::Off => ShiftState::Once,
                ShiftState::Once => ShiftState::Locked,
                ShiftState::Locked => ShiftState::Off,
            };
            None
        }
        KeyAction::Symbols => {
            state.symbols = !state.symbols;
            state.shift = ShiftState::Off;
            None
        }
        KeyAction::Hide => None,
    }
}

/// Key rectangles for a keyboard filling `area`: `(row, col, rect, key)`.
/// Rows are centred; key widths scale with their unit width.
pub fn key_rects(area: Rect, gap: f32, symbols: bool) -> Vec<(usize, usize, Rect, Key)> {
    let rows = keyboard_rows(symbols);
    let max_units = rows
        .iter()
        .map(|r| r.iter().map(|k| k.width).sum::<f32>())
        .fold(0.0, f32::max);
    let unit = (area.w + gap) / max_units;
    let row_h = (area.h + gap) / rows.len() as f32;
    let mut out = Vec::new();
    for (ri, row) in rows.iter().enumerate() {
        let row_units: f32 = row.iter().map(|k| k.width).sum();
        let mut x = area.x + (max_units - row_units) * unit * 0.5;
        let y = area.y + ri as f32 * row_h;
        for (ci, key) in row.iter().enumerate() {
            out.push((
                ri,
                ci,
                Rect::new(x, y, key.width * unit - gap, row_h - gap),
                *key,
            ));
            x += key.width * unit;
        }
    }
    out
}

/// Text shown on a key (also its automation label).
pub fn key_label(action: KeyAction, state: &KeyboardState) -> String {
    let upper = state.shift != ShiftState::Off && !state.symbols;
    match action {
        KeyAction::Char(c) if upper => c.to_uppercase().collect(),
        KeyAction::Char(c) => c.to_string(),
        KeyAction::Space => "space".into(),
        KeyAction::Backspace => "backspace".into(),
        KeyAction::Enter => "enter".into(),
        KeyAction::Shift => "shift".into(),
        KeyAction::Symbols => if state.symbols { "ABC" } else { "?123" }.into(),
        KeyAction::Left => "left".into(),
        KeyAction::Right => "right".into(),
        KeyAction::Hide => "hide".into(),
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct KeyboardResponse {
    pub events: Vec<TextEvent>,
    /// The hide key was pressed.
    pub hide: bool,
}

/// Seconds a repeatable key must be held before auto-repeat, then the interval.
const REPEAT_DELAY: f32 = 0.5;
const REPEAT_INTERVAL: f32 = 0.07;

impl Ui {
    /// Keyboard filling the available width, `height` px tall.
    pub fn virtual_keyboard(
        &mut self,
        key: impl Hash,
        state: &mut KeyboardState,
        height: f32,
    ) -> KeyboardResponse {
        let t = self.theme.clone();
        let area = self.allocate(Vec2::new(FILL, height));
        let id = self.make_id(("keyboard", key));
        let gap = t.spacing * 0.5;
        self.painter.rect_rounded(
            area.expand(gap),
            t.corner_radius,
            t.panel_bg.lerp(t.surface, 0.3),
        );
        let mut out = KeyboardResponse::default();
        self.push_id(id.0);
        for (ri, ci, r, key) in key_rects(area, gap, state.symbols) {
            let kid = self.make_id((ri, ci, state.symbols));
            let resp = self.interact(r, kid, Sense::CLICK);
            self.record(kid, r, || key_label(key.action, state));
            let mut fire = resp.clicked;
            // Auto-repeat for held editing keys.
            if matches!(
                key.action,
                KeyAction::Backspace | KeyAction::Left | KeyAction::Right
            ) {
                let timer_id = kid.with("repeat");
                if resp.pressed {
                    let before = self.memory.get(&timer_id).copied().unwrap_or(0.0);
                    let held = before + self.dt;
                    self.memory.insert(timer_id, held);
                    if held >= REPEAT_DELAY {
                        let n_prev = ((before - REPEAT_DELAY).max(-REPEAT_INTERVAL)
                            / REPEAT_INTERVAL)
                            .floor();
                        let n_now = ((held - REPEAT_DELAY) / REPEAT_INTERVAL).floor();
                        if n_now > n_prev {
                            fire = true;
                        }
                    }
                } else if let Some(held) = self.memory.remove(&timer_id) {
                    // Release after auto-repeat already fired: don't add one more.
                    if held >= REPEAT_DELAY {
                        fire = false;
                    }
                }
            }
            if fire {
                if key.action == KeyAction::Hide {
                    out.hide = true;
                }
                if let Some(ev) = press_key(state, key.action) {
                    out.events.push(ev);
                }
            }
            self.draw_key(r, &resp, key.action, state);
        }
        self.pop_id();
        self.text_queue.extend(out.events.iter().cloned());
        out
    }

    fn draw_key(
        &mut self,
        r: Rect,
        resp: &crate::ui::Response,
        action: KeyAction,
        state: &KeyboardState,
    ) {
        let t = self.theme.clone();
        let special = !matches!(action, KeyAction::Char(_) | KeyAction::Space);
        let lit = matches!(action, KeyAction::Shift) && state.shift != ShiftState::Off;
        let mut bg = self.surface_color(resp);
        if special && !resp.highlighted() && !resp.pressed {
            bg = bg.lerp(t.panel_bg, 0.35);
        }
        if lit {
            bg = if state.shift == ShiftState::Locked {
                t.accent
            } else {
                t.accent.alpha(0.55)
            };
        }
        if action == KeyAction::Enter {
            bg = if resp.highlighted() {
                t.accent_hover
            } else {
                t.accent
            };
        }
        self.painter.rect_rounded(r, t.corner_radius * 0.6, bg);
        let icon_s = (r.h * 0.45).min(t.icon_size);
        let icon_rect = Rect::from_center(r.center(), Vec2::splat(icon_s));
        match action {
            KeyAction::Char(_) => self.draw_text_in(
                r,
                &key_label(action, state),
                t.text_size,
                t.text,
                Align::Center,
            ),
            KeyAction::Space => {
                self.draw_text_in(r, "space", t.small_text_size, t.text_dim, Align::Center)
            }
            KeyAction::Symbols => self.draw_text_in(
                r,
                &key_label(action, state),
                t.small_text_size,
                t.text,
                Align::Center,
            ),
            KeyAction::Backspace => {
                draw_icon(&mut self.painter, Icon::Backspace, icon_rect, t.text)
            }
            KeyAction::Enter => draw_icon(&mut self.painter, Icon::Enter, icon_rect, t.on_accent),
            KeyAction::Shift => draw_icon(&mut self.painter, Icon::Shift, icon_rect, t.text),
            KeyAction::Left => draw_icon(&mut self.painter, Icon::ChevronLeft, icon_rect, t.text),
            KeyAction::Right => draw_icon(&mut self.painter, Icon::ChevronRight, icon_rect, t.text),
            KeyAction::Hide => draw_icon(&mut self.painter, Icon::ChevronDown, icon_rect, t.text),
        }
        self.focus_ring(resp, t.corner_radius * 0.6);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_cycles_and_applies_once() {
        let mut s = KeyboardState::default();
        assert_eq!(
            press_key(&mut s, KeyAction::Char('a')),
            Some(TextEvent::Text("a".into()))
        );
        press_key(&mut s, KeyAction::Shift);
        assert_eq!(s.shift, ShiftState::Once);
        assert_eq!(
            press_key(&mut s, KeyAction::Char('a')),
            Some(TextEvent::Text("A".into()))
        );
        assert_eq!(s.shift, ShiftState::Off);
        press_key(&mut s, KeyAction::Shift);
        press_key(&mut s, KeyAction::Shift);
        assert_eq!(s.shift, ShiftState::Locked);
        press_key(&mut s, KeyAction::Char('b'));
        assert_eq!(
            press_key(&mut s, KeyAction::Char('c')),
            Some(TextEvent::Text("C".into()))
        );
        press_key(&mut s, KeyAction::Shift);
        assert_eq!(s.shift, ShiftState::Off);
    }

    #[test]
    fn symbols_page_and_specials() {
        let mut s = KeyboardState {
            shift: ShiftState::Once,
            symbols: false,
        };
        assert_eq!(press_key(&mut s, KeyAction::Symbols), None);
        assert!(s.symbols);
        assert_eq!(s.shift, ShiftState::Off);
        assert_eq!(
            press_key(&mut s, KeyAction::Space),
            Some(TextEvent::Text(" ".into()))
        );
        assert_eq!(
            press_key(&mut s, KeyAction::Backspace),
            Some(TextEvent::Backspace)
        );
        assert_eq!(press_key(&mut s, KeyAction::Enter), Some(TextEvent::Enter));
        assert_eq!(press_key(&mut s, KeyAction::Hide), None);
        let rows = keyboard_rows(true);
        assert!(rows
            .iter()
            .flatten()
            .any(|k| k.action == KeyAction::Char('@')));
        let letters = keyboard_rows(false);
        let n_letters = letters
            .iter()
            .flatten()
            .filter(|k| matches!(k.action, KeyAction::Char(c) if c.is_ascii_lowercase()))
            .count();
        assert_eq!(n_letters, 26);
        assert!(letters
            .iter()
            .flatten()
            .any(|k| k.action == KeyAction::Shift));
    }
}
