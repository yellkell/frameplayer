//! Layout math: stacking cursors, even splits, grid sizing, scroll physics
//! and virtualized visible ranges. Everything here is pure and unit-tested;
//! [`crate::Ui`] wires it to widgets.

use crate::geom::{Rect, Vec2};
use std::ops::Range;

/// Size component meaning "take all remaining space".
pub const FILL: f32 = f32::INFINITY;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Vertical,
    Horizontal,
}

/// A stacking region: widgets are placed one after another along `dir`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub rect: Rect,
    pub dir: Dir,
    pub spacing: f32,
    pub cursor: Vec2,
    /// Bounding box of everything placed so far (starts empty at the origin).
    pub used: Rect,
    placed: bool,
}

impl Layout {
    pub fn new(rect: Rect, dir: Dir, spacing: f32) -> Layout {
        Layout {
            rect,
            dir,
            spacing,
            cursor: rect.min(),
            used: Rect::new(rect.x, rect.y, 0.0, 0.0),
            placed: false,
        }
    }

    /// Space left from the cursor to the region's far edge.
    pub fn available(&self) -> Rect {
        let gap = if self.placed { self.spacing } else { 0.0 };
        match self.dir {
            Dir::Vertical => {
                let y = self.cursor.y + gap;
                Rect::new(
                    self.rect.x,
                    y,
                    self.rect.w,
                    (self.rect.bottom() - y).max(0.0),
                )
            }
            Dir::Horizontal => {
                let x = self.cursor.x + gap;
                Rect::new(
                    x,
                    self.rect.y,
                    (self.rect.right() - x).max(0.0),
                    self.rect.h,
                )
            }
        }
    }

    /// Places an item. A size component of [`FILL`] takes all remaining
    /// space on that axis.
    pub fn allocate(&mut self, size: Vec2) -> Rect {
        let avail = self.available();
        let w = if size.x.is_infinite() {
            avail.w
        } else {
            size.x.max(0.0)
        };
        let h = if size.y.is_infinite() {
            avail.h
        } else {
            size.y.max(0.0)
        };
        let r = Rect::new(avail.x, avail.y, w, h);
        match self.dir {
            Dir::Vertical => self.cursor.y = r.bottom(),
            Dir::Horizontal => self.cursor.x = r.right(),
        }
        self.used = if self.placed { self.used.union(&r) } else { r };
        self.placed = true;
        r
    }

    pub fn add_space(&mut self, px: f32) {
        match self.dir {
            Dir::Vertical => self.cursor.y += px,
            Dir::Horizontal => self.cursor.x += px,
        }
    }

    /// Size consumed along both axes, measured from the region's origin.
    pub fn content_size(&self) -> Vec2 {
        if !self.placed {
            return Vec2::ZERO;
        }
        self.used.max() - self.rect.min()
    }
}

/// Splits `total` into `n` equal cells separated by `spacing`: `(offset, size)`.
pub fn split_even(total: f32, n: usize, spacing: f32) -> Vec<(f32, f32)> {
    if n == 0 {
        return Vec::new();
    }
    let size = ((total - spacing * (n - 1) as f32) / n as f32).max(0.0);
    (0..n)
        .map(|i| (i as f32 * (size + spacing), size))
        .collect()
}

/// Splits `total` by weights (e.g. `[1.0, 3.0]` for a 1:3 sidebar).
pub fn split_weighted(total: f32, weights: &[f32], spacing: f32) -> Vec<(f32, f32)> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }
    let sum: f32 = weights.iter().sum::<f32>().max(f32::EPSILON);
    let free = (total - spacing * (n - 1) as f32).max(0.0);
    let mut x = 0.0;
    weights
        .iter()
        .map(|w| {
            let s = free * w / sum;
            let cell = (x, s);
            x += s + spacing;
            cell
        })
        .collect()
}

/// Number of columns and tile width for a grid that fits tiles of at least
/// `min_tile_w` into `width`.
pub fn grid_columns(width: f32, min_tile_w: f32, spacing: f32) -> (usize, f32) {
    let cols = (((width + spacing) / (min_tile_w + spacing)).floor() as usize).max(1);
    let tile_w = ((width - spacing * (cols - 1) as f32) / cols as f32).max(0.0);
    (cols, tile_w)
}

/// Rows of a uniformly sized list intersecting the viewport, plus `overscan`
/// rows on each side.
pub fn visible_range(
    offset: f32,
    viewport: f32,
    row_h: f32,
    spacing: f32,
    count: usize,
    overscan: usize,
) -> Range<usize> {
    let stride = row_h + spacing;
    if count == 0 || stride <= 0.0 || viewport <= 0.0 {
        return 0..0;
    }
    let first = (offset.max(0.0) / stride).floor() as usize;
    let last = ((offset.max(0.0) + viewport) / stride).ceil() as usize;
    first.saturating_sub(overscan).min(count)..(last + overscan).min(count)
}

/// Content height of a uniform list (no trailing spacing).
pub fn list_height(count: usize, row_h: f32, spacing: f32) -> f32 {
    if count == 0 {
        0.0
    } else {
        count as f32 * row_h + (count - 1) as f32 * spacing
    }
}

/// Item index range of a virtualized grid intersecting the viewport.
pub fn grid_visible_range(
    offset: f32,
    viewport: f32,
    cols: usize,
    row_h: f32,
    spacing: f32,
    count: usize,
    overscan_rows: usize,
) -> Range<usize> {
    let cols = cols.max(1);
    let rows = count.div_ceil(cols);
    let r = visible_range(offset, viewport, row_h, spacing, rows, overscan_rows);
    (r.start * cols).min(count)..(r.end * cols).min(count)
}

/// Scroll position with momentum, clamping and thumbstick input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollState {
    pub offset: f32,
    /// px/s, positive = content moves up (offset increases).
    pub velocity: f32,
    pub content: f32,
    pub viewport: f32,
    pub dragging: bool,
}

impl Default for ScrollState {
    fn default() -> Self {
        ScrollState {
            offset: 0.0,
            velocity: 0.0,
            content: 0.0,
            viewport: 0.0,
            dragging: false,
        }
    }
}

/// Velocity decay rate for momentum scrolling (1/s).
pub const SCROLL_FRICTION: f32 = 4.0;
/// Thumbstick scroll speed at full deflection, in viewports per second.
pub const STICK_SCROLL_SPEED: f32 = 1.6;

impl ScrollState {
    pub fn max_offset(&self) -> f32 {
        (self.content - self.viewport).max(0.0)
    }

    pub fn clamp(&mut self) {
        let max = self.max_offset();
        if self.offset < 0.0 || self.offset > max {
            self.offset = self.offset.clamp(0.0, max);
            self.velocity = 0.0;
        }
    }

    pub fn set_extent(&mut self, content: f32, viewport: f32) {
        self.content = content.max(0.0);
        self.viewport = viewport.max(0.0);
        self.clamp();
    }

    /// Pointer drag: the content follows the pointer (`dy` > 0 = pointer moved down).
    pub fn drag(&mut self, dy: f32, dt: f32) {
        self.dragging = true;
        self.offset -= dy;
        if dt > 0.0 {
            let v = -dy / dt;
            // Smooth so the release velocity isn't one noisy sample.
            self.velocity = self.velocity * 0.6 + v * 0.4;
        }
        self.clamp();
    }

    pub fn release(&mut self) {
        self.dragging = false;
    }

    /// Thumbstick: `axis` > 0 (stick up) scrolls towards the top.
    pub fn stick(&mut self, axis: f32, dt: f32) {
        if axis.abs() < 0.15 {
            return;
        }
        self.velocity = 0.0;
        // Quadratic response for fine control near centre.
        let s = axis.signum() * axis * axis;
        self.offset -= s * STICK_SCROLL_SPEED * self.viewport.max(200.0) * dt;
        self.clamp();
    }

    /// Advances momentum.
    pub fn step(&mut self, dt: f32) {
        if self.dragging {
            return;
        }
        if self.velocity.abs() < 5.0 {
            self.velocity = 0.0;
            return;
        }
        self.offset += self.velocity * dt;
        self.velocity *= (-SCROLL_FRICTION * dt).exp();
        self.clamp();
    }

    /// Minimal scroll so `[top, bottom)` (content coordinates) is visible.
    pub fn scroll_to_visible(&mut self, top: f32, bottom: f32) {
        if top < self.offset {
            self.offset = top;
        } else if bottom > self.offset + self.viewport {
            self.offset = bottom - self.viewport;
        }
        self.velocity = 0.0;
        self.clamp();
    }

    /// Scrollbar thumb `(offset, length)` within a track of `track_len`.
    pub fn thumb(&self, track_len: f32, min_len: f32) -> Option<(f32, f32)> {
        if self.content <= self.viewport || self.content <= 0.0 {
            return None;
        }
        let len =
            (track_len * self.viewport / self.content).clamp(min_len.min(track_len), track_len);
        let max = self.max_offset();
        let t = if max > 0.0 { self.offset / max } else { 0.0 };
        Some(((track_len - len) * t, len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_stack() {
        let mut l = Layout::new(Rect::new(10.0, 20.0, 200.0, 300.0), Dir::Vertical, 8.0);
        let a = l.allocate(Vec2::new(FILL, 40.0));
        let b = l.allocate(Vec2::new(100.0, 30.0));
        assert_eq!(a, Rect::new(10.0, 20.0, 200.0, 40.0));
        assert_eq!(b, Rect::new(10.0, 68.0, 100.0, 30.0));
        assert_eq!(l.content_size(), Vec2::new(200.0, 78.0));
        let rest = l.allocate(Vec2::new(FILL, FILL));
        assert_eq!(rest.y, 106.0);
        assert_eq!(rest.bottom(), 320.0);
    }

    #[test]
    fn horizontal_stack_and_space() {
        let mut l = Layout::new(Rect::new(0.0, 0.0, 300.0, 50.0), Dir::Horizontal, 10.0);
        let a = l.allocate(Vec2::new(50.0, FILL));
        l.add_space(20.0);
        let b = l.allocate(Vec2::new(50.0, 20.0));
        assert_eq!(a.h, 50.0);
        assert_eq!(b.x, 80.0);
        assert_eq!(l.available().w, 300.0 - 140.0);
    }

    #[test]
    fn splits() {
        let s = split_even(100.0, 4, 4.0);
        assert_eq!(s.len(), 4);
        assert_eq!(s[0], (0.0, 22.0));
        assert_eq!(s[3], (78.0, 22.0));
        let w = split_weighted(410.0, &[1.0, 3.0], 10.0);
        assert_eq!(w, vec![(0.0, 100.0), (110.0, 300.0)]);
        assert!(split_even(10.0, 0, 1.0).is_empty());
    }

    #[test]
    fn grid_sizing() {
        assert_eq!(grid_columns(1000.0, 300.0, 20.0), (3, 320.0));
        assert_eq!(grid_columns(100.0, 300.0, 20.0).0, 1);
    }

    #[test]
    fn virtual_ranges() {
        // 100 rows of 50px (+10 spacing), viewport 200 at offset 0 → rows 0..4.
        assert_eq!(visible_range(0.0, 200.0, 50.0, 10.0, 100, 0), 0..4);
        assert_eq!(visible_range(600.0, 200.0, 50.0, 10.0, 100, 1), 9..15);
        assert_eq!(visible_range(5900.0, 200.0, 50.0, 10.0, 100, 2), 96..100);
        assert_eq!(visible_range(0.0, 200.0, 50.0, 10.0, 0, 2), 0..0);
        assert_eq!(list_height(3, 50.0, 10.0), 170.0);
        assert_eq!(grid_visible_range(0.0, 300.0, 4, 140.0, 10.0, 30, 0), 0..8);
        assert_eq!(
            grid_visible_range(900.0, 300.0, 4, 140.0, 10.0, 30, 1),
            20..30
        );
    }

    #[test]
    fn scroll_clamping_and_momentum() {
        let mut s = ScrollState::default();
        s.set_extent(1000.0, 300.0);
        assert_eq!(s.max_offset(), 700.0);
        s.drag(50.0, 0.016);
        assert_eq!(s.offset, 0.0, "can't drag above the top");
        for _ in 0..5 {
            s.drag(-20.0, 0.016);
        }
        assert_eq!(s.offset, 100.0);
        s.release();
        let v = s.velocity;
        assert!(v > 0.0);
        s.step(0.016);
        assert!(s.offset > 100.0, "momentum continues");
        for _ in 0..1000 {
            s.step(0.016);
        }
        assert_eq!(s.velocity, 0.0);
        assert!(s.offset <= 700.0);
        s.offset = 10_000.0;
        s.clamp();
        assert_eq!(s.offset, 700.0);
        // Content shrinking re-clamps.
        s.set_extent(200.0, 300.0);
        assert_eq!(s.offset, 0.0);
        assert!(s.thumb(300.0, 20.0).is_none());
    }

    #[test]
    fn stick_scroll_and_visibility() {
        let mut s = ScrollState::default();
        s.set_extent(2000.0, 400.0);
        s.stick(-1.0, 0.5);
        assert!(s.offset > 0.0);
        let o = s.offset;
        s.stick(0.1, 0.5);
        assert_eq!(s.offset, o, "dead zone");
        s.stick(1.0, 10.0);
        assert_eq!(s.offset, 0.0);
        s.scroll_to_visible(900.0, 960.0);
        assert_eq!(s.offset, 560.0);
        s.scroll_to_visible(100.0, 160.0);
        assert_eq!(s.offset, 100.0);
        let (pos, len) = s.thumb(400.0, 30.0).unwrap();
        assert_eq!(len, 80.0);
        assert!((pos - 320.0 * 100.0 / 1600.0).abs() < 1e-3);
    }
}
