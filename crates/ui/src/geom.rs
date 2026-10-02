//! 2D geometry in panel pixel space (origin top-left, +y down).

pub use glam::Vec2;

/// Axis-aligned rectangle in panel pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const ZERO: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: 0.0,
    };

    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn from_min_max(min: Vec2, max: Vec2) -> Rect {
        Rect::new(
            min.x,
            min.y,
            (max.x - min.x).max(0.0),
            (max.y - min.y).max(0.0),
        )
    }

    pub fn from_center(c: Vec2, size: Vec2) -> Rect {
        Rect::new(c.x - size.x * 0.5, c.y - size.y * 0.5, size.x, size.y)
    }

    pub fn min(&self) -> Vec2 {
        Vec2::new(self.x, self.y)
    }

    pub fn max(&self) -> Vec2 {
        Vec2::new(self.x + self.w, self.y + self.h)
    }

    pub fn size(&self) -> Vec2 {
        Vec2::new(self.w, self.h)
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn center(&self) -> Vec2 {
        Vec2::new(self.x + self.w * 0.5, self.y + self.h * 0.5)
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    /// Half-open containment: the right/bottom edges belong to the neighbour.
    pub fn contains(&self, p: Vec2) -> bool {
        p.x >= self.x && p.y >= self.y && p.x < self.right() && p.y < self.bottom()
    }

    /// Whether `other` lies completely inside `self`.
    pub fn contains_rect(&self, other: &Rect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    /// Intersection; empty (zero-size) when disjoint.
    pub fn intersect(&self, other: &Rect) -> Rect {
        let min = self.min().max(other.min());
        let max = self.max().min(other.max());
        Rect::from_min_max(min, max.max(min))
    }

    pub fn union(&self, other: &Rect) -> Rect {
        Rect::from_min_max(self.min().min(other.min()), self.max().max(other.max()))
    }

    /// Inset on all sides; never produces negative sizes.
    pub fn shrink(&self, d: f32) -> Rect {
        self.shrink2(d, d)
    }

    pub fn shrink2(&self, dx: f32, dy: f32) -> Rect {
        let dx = dx.min(self.w * 0.5);
        let dy = dy.min(self.h * 0.5);
        Rect::new(
            self.x + dx,
            self.y + dy,
            self.w - 2.0 * dx,
            self.h - 2.0 * dy,
        )
    }

    pub fn expand(&self, d: f32) -> Rect {
        Rect::new(self.x - d, self.y - d, self.w + 2.0 * d, self.h + 2.0 * d)
    }

    pub fn translate(&self, d: Vec2) -> Rect {
        Rect::new(self.x + d.x, self.y + d.y, self.w, self.h)
    }

    /// Splits off a strip of width `w` from the left: `(left, rest)`.
    pub fn split_left(&self, w: f32) -> (Rect, Rect) {
        let w = w.clamp(0.0, self.w);
        (
            Rect::new(self.x, self.y, w, self.h),
            Rect::new(self.x + w, self.y, self.w - w, self.h),
        )
    }

    /// Splits off a strip of width `w` from the right: `(rest, right)`.
    pub fn split_right(&self, w: f32) -> (Rect, Rect) {
        let w = w.clamp(0.0, self.w);
        (
            Rect::new(self.x, self.y, self.w - w, self.h),
            Rect::new(self.right() - w, self.y, w, self.h),
        )
    }

    /// Splits off a strip of height `h` from the top: `(top, rest)`.
    pub fn split_top(&self, h: f32) -> (Rect, Rect) {
        let h = h.clamp(0.0, self.h);
        (
            Rect::new(self.x, self.y, self.w, h),
            Rect::new(self.x, self.y + h, self.w, self.h - h),
        )
    }

    /// Splits off a strip of height `h` from the bottom: `(rest, bottom)`.
    pub fn split_bottom(&self, h: f32) -> (Rect, Rect) {
        let h = h.clamp(0.0, self.h);
        (
            Rect::new(self.x, self.y, self.w, self.h - h),
            Rect::new(self.x, self.bottom() - h, self.w, h),
        )
    }

    /// `[x, y, w, h]`, the layout used by [`fp_core::draw::DrawCmd::clip`].
    pub fn to_array(&self) -> [f32; 4] {
        [self.x, self.y, self.w, self.h]
    }

    /// Squared distance from `p` to the rectangle (0 inside).
    pub fn distance_sq(&self, p: Vec2) -> f32 {
        let d = (self.min() - p).max(p - self.max()).max(Vec2::ZERO);
        d.length_squared()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_and_contains() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        assert_eq!(a.intersect(&b), Rect::new(5.0, 5.0, 5.0, 5.0));
        assert!(a.intersect(&Rect::new(20.0, 20.0, 1.0, 1.0)).is_empty());
        assert!(a.contains(Vec2::new(0.0, 0.0)));
        assert!(!a.contains(Vec2::new(10.0, 5.0)));
        assert!(a.contains_rect(&Rect::new(1.0, 1.0, 2.0, 2.0)));
        assert_eq!(a.union(&b), Rect::new(0.0, 0.0, 15.0, 15.0));
    }

    #[test]
    fn splits_and_shrink() {
        let r = Rect::new(0.0, 0.0, 100.0, 50.0);
        let (l, rest) = r.split_left(30.0);
        assert_eq!(l.w, 30.0);
        assert_eq!(rest.x, 30.0);
        assert_eq!(rest.w, 70.0);
        let (top, bottom) = r.split_top(80.0);
        assert_eq!(top.h, 50.0);
        assert_eq!(bottom.h, 0.0);
        let s = r.shrink(40.0);
        assert_eq!(s.h, 0.0);
        assert!(s.w > 0.0);
        assert_eq!(r.distance_sq(Vec2::new(103.0, 54.0)), 25.0);
    }
}
