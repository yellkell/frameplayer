//! The Quest's drawn hands: a glove of hard light round your real ones
//! (Pepperball League's hands). Each hand is one small mesh built every frame
//! from the tracked joints: a rounded slab for the palm, fading out up the
//! wrist, and a tapered tube with a rounded tip for the thumb and each finger,
//! drawn a few millimetres fatter than your hand. The renderer traces its
//! silhouette in a rim of light with the pointing fingertip lit in the accent
//! blue (brighter as you pinch, with a flash on the pinch itself).
//!
//! With passthrough it also REVEALS your real hand: the glove wipes what's
//! drawn where your hand is, so the menus open there and show the hand itself,
//! traced in light. Without passthrough there's no real hand behind to show:
//! it's a ghost glove instead, with more body.

use fp_render::{HandDraw, HandVertex};
use fp_xr::{Hand, Joint};
use glam::Vec3;
use std::f32::consts::PI;

/// How much fatter than your real hand the glove is drawn (m): the reveal
/// shows all of it, with the rim just outside it.
const INFLATE: f32 = 0.004;
/// Sides of each finger's tube.
const SIDES: usize = 10;
/// A trigger (pinch) past this is a press, as the menus count it.
const PINCH: f32 = 0.7;
const FLASH_SECS: f32 = 0.25;

// Joint indices (XR_EXT_hand_tracking).
const PALM: usize = 0;
const THUMB_META: usize = 2;
const INDEX_META: usize = 6;

/// A finger's joints start at 6 + 5f (f: index, middle, ring, little):
/// metacarpal, proximal, intermediate, distal, tip.
fn finger_base(f: usize) -> usize {
    INDEX_META + 5 * f
}

/// Default radii (m), used where the runtime gives none (and to keep its own
/// sensible).
const FINGER_R: [f32; 5] = [0.0105, 0.0098, 0.0085, 0.0076, 0.007];
const FINGER_SCALE: [f32; 4] = [0.98, 1.0, 0.95, 0.85];
const THUMB_R: [f32; 4] = [0.0165, 0.0118, 0.0098, 0.0088];
/// The palm's slices, from up the wrist (v < 0, fading out) through the
/// metacarpals' bases (0) to the knuckles (1).
const PALM_V: [f32; 7] = [-0.5, -0.3, -0.12, 0.0, 0.35, 0.7, 1.0];

/// Both hands' gloves, eased in and out.
#[derive(Default)]
pub(crate) struct Gloves {
    hands: [Glove; 2],
}

#[derive(Default)]
struct Glove {
    /// Shown, eased 0..1.
    vis: f32,
    /// The last pose's mesh: it's held while the glove fades out.
    mesh: Option<Mesh>,
    pinched: bool,
    /// Seconds since the last pinch, while it flashes.
    since_pinch: Option<f32>,
}

impl Gloves {
    /// This frame's gloves. `shown`: hands are drawn (with the menus);
    /// `passthrough`: the room shows behind, so the real hands can be
    /// revealed.
    pub(crate) fn frame(
        &mut self,
        hands: &[Hand; 2],
        shown: bool,
        passthrough: bool,
        dt: f32,
    ) -> Vec<HandDraw> {
        let accent = egui::Rgba::from(crate::ui::theme::ACCENT);
        let accent = Vec3::new(accent.r(), accent.g(), accent.b());
        let tint = Vec3::ONE.lerp(accent, 0.3) * 0.75;
        let tip_tint = accent * 0.9;
        let mut out = Vec::new();
        for (g, h) in self.hands.iter_mut().zip(hands) {
            let joints = h.joints.filter(|_| h.tracked_hand && shown);
            let target = joints.is_some() as u8 as f32;
            let secs = if joints.is_some() { 0.15 } else { 0.1 };
            g.vis = move_towards(g.vis, target, dt / secs);
            if let Some(j) = &joints {
                g.mesh = Some(build(j));
            }
            g.since_pinch = g.since_pinch.map(|s| s + dt).filter(|&s| s < FLASH_SECS);
            if !g.pinched && h.trigger > PINCH {
                g.since_pinch = Some(0.0);
            }
            g.pinched = if g.pinched {
                h.trigger >= 0.35
            } else {
                h.trigger > PINCH
            };
            let Some(mesh) = g.mesh.as_ref().filter(|_| g.vis > 0.001) else {
                continue;
            };
            let flash = g.since_pinch.map_or(0.0, |s| 1.0 - s / FLASH_SECS);
            out.push(HandDraw {
                vertices: mesh.v.clone(),
                indices: mesh.i.clone(),
                tint: tint.to_array(),
                tip_tint: tip_tint.to_array(),
                rim: g.vis,
                fill: if passthrough { 0.04 } else { 0.14 } * g.vis,
                tip: g.vis * (0.8 + 0.6 * h.trigger.clamp(0.0, 1.0) + flash),
                reveal: if passthrough { g.vis } else { 0.0 },
            });
        }
        out
    }
}

fn move_towards(v: f32, target: f32, step: f32) -> f32 {
    if v < target {
        (v + step).min(target)
    } else {
        (v - step).max(target)
    }
}

/// One point along a finger's tube: its centre, the back of the finger
/// there, the tube's radius, and how lit it is (the pointing fingertip).
#[derive(Clone, Copy, Default)]
struct Seg {
    p: Vec3,
    up: Vec3,
    r: f32,
    lit: f32,
}

#[derive(Default)]
struct Mesh {
    v: Vec<HandVertex>,
    i: Vec<u16>,
}

/// The back of the hand at a joint (its +Y), or `fallback` where the runtime
/// gave no orientation.
fn back(j: &Joint, fallback: Vec3) -> Vec3 {
    j.rot.map_or(fallback, |q| q * Vec3::Y)
}

fn perp(axis: Vec3) -> Vec3 {
    let p = axis.cross(Vec3::Y);
    let p = if p.length_squared() < 1e-6 {
        axis.cross(Vec3::X)
    } else {
        p
    };
    p.normalize_or_zero()
}

fn radius(j: &Joint, fallback: f32) -> f32 {
    if j.radius > 0.0 {
        j.radius.clamp(fallback * 0.7, fallback * 1.4)
    } else {
        fallback
    }
}

/// The glove round a hand's joints.
fn build(j: &[Joint; 26]) -> Mesh {
    let mut m = Mesh::default();
    // The palm's frame: the back of the hand, and across it from the index
    // to the little finger.
    let back_n = back(&j[PALM], Vec3::Y).normalize_or_zero();
    let mut across = j[finger_base(3) + 1].pos - j[finger_base(0) + 1].pos;
    across -= back_n * across.dot(back_n);
    let across = if across.length_squared() > 1e-8 {
        across.normalize()
    } else {
        perp(back_n)
    };
    m.palm(j, back_n, across);

    // The thumb, from its base by the wrist (capped there) to its tip.
    let thumb: [Seg; 4] = std::array::from_fn(|k| {
        let p = &j[THUMB_META + k];
        Seg {
            p: p.pos,
            up: back(p, back_n),
            r: radius(p, THUMB_R[k]) + INFLATE,
            lit: 0.0,
        }
    });
    m.chain(&thumb, true);

    // The fingers, from inside the palm to their tips; the index's tip is lit.
    for (f, &scale) in FINGER_SCALE.iter().enumerate() {
        let b = finger_base(f);
        let segs: [Seg; 5] = std::array::from_fn(|k| {
            if k == 0 {
                return Seg {
                    p: j[b].pos.lerp(j[b + 1].pos, 0.55),
                    up: back(&j[b + 1], back_n),
                    r: FINGER_R[0] * scale + INFLATE,
                    lit: 0.0,
                };
            }
            let p = &j[b + k];
            Seg {
                p: p.pos,
                up: back(p, back_n),
                r: radius(p, FINGER_R[k] * scale) + INFLATE,
                lit: match (f, k) {
                    (0, 2) => 0.12,
                    (0, 3) => 0.55,
                    (0, 4) => 1.0,
                    _ => 0.0,
                },
            }
        });
        m.chain(&segs, false);
    }
    m
}

impl Mesh {
    fn vertex(&mut self, p: Vec3, n: Vec3, lit: f32, presence: f32) -> u16 {
        self.v.push(HandVertex {
            pos: p.to_array(),
            normal: n.to_array(),
            color: [lit, 0.0, 0.0, presence],
        });
        (self.v.len() - 1) as u16
    }

    /// The palm: slices across it from up the wrist to the knuckles, each a
    /// loop round a flat slab with rounded sides (over the backs of the four
    /// metacarpals, round the index edge, under the palm, round the little
    /// finger's edge), capped at both ends.
    fn palm(&mut self, j: &[Joint; 26], back: Vec3, across: Vec3) {
        const L: usize = 14;
        let (mut first, mut prev) = (None, None);
        for v in PALM_V {
            let mut cols = [Vec3::ZERO; 4];
            for (f, c) in cols.iter_mut().enumerate() {
                let meta = j[finger_base(f)].pos;
                let knuckle = j[finger_base(f) + 1].pos;
                *c = if v >= 0.0 {
                    meta.lerp(knuckle, v)
                } else {
                    meta + (meta - knuckle) * (-v * 0.55)
                };
            }
            // Up the wrist it narrows toward the forearm.
            if v < 0.0 {
                let mean = cols.iter().sum::<Vec3>() * 0.25;
                for c in &mut cols {
                    *c = c.lerp(mean, -v * 0.5);
                }
            }
            let h = if v >= 0.0 {
                0.0155 + (0.0115 - 0.0155) * v
            } else {
                0.0155 - v * 0.004
            } + INFLATE;
            let t = ((v + 0.5) / 0.42).clamp(0.0, 1.0);
            let presence = t * t * (3.0 - 2.0 * t);

            let start = self.v.len() as u16;
            for f in (0..4).rev() {
                self.vertex(cols[f] + back * h, back, 0.0, presence);
            }
            for k in 1..=3 {
                let a = k as f32 * PI / 4.0;
                let d = back * a.cos() - across * a.sin();
                self.vertex(cols[0] + d * h, d, 0.0, presence);
            }
            for c in cols {
                self.vertex(c - back * h, -back, 0.0, presence);
            }
            for k in 1..=3 {
                let a = k as f32 * PI / 4.0;
                let d = -back * a.cos() + across * a.sin();
                self.vertex(cols[3] + d * h, d, 0.0, presence);
            }
            if let Some(p) = prev {
                self.join(p, start, L);
            }
            first.get_or_insert(start);
            prev = Some(start);
        }
        // Caps at both ends.
        if let (Some(first), Some(last)) = (first, prev) {
            self.cap(first, L);
            self.cap(last, L);
        }
    }

    /// A fan over a loop's open end, from its middle.
    fn cap(&mut self, start: u16, count: usize) {
        let s = start as usize;
        let at = |m: &Mesh, k: usize| Vec3::from_array(m.v[s + k].pos);
        let centre = (0..count).map(|k| at(self, k)).sum::<Vec3>() / count as f32;
        let n = (at(self, 1) - at(self, 0)).cross(at(self, count / 2) - at(self, 0));
        let n = if n.length_squared() > 1e-12 {
            n.normalize()
        } else {
            Vec3::Y
        };
        let [lit, _, _, presence] = self.v[s].color;
        let c = self.vertex(centre, n, lit, presence);
        for k in 0..count {
            self.i
                .extend([c, start + k as u16, start + ((k + 1) % count) as u16]);
        }
    }

    /// A tapered tube through `segs`, a ring of SIDES round each point (each
    /// ring's seam on the back of the finger, so it doesn't twist), with a
    /// round cap on its tip (its pole lands on the tip joint) and, if asked,
    /// on its base.
    fn chain(&mut self, segs: &[Seg], cap_start: bool) {
        let n = segs.len();
        let axis0 = (segs[1].p - segs[0].p).normalize_or_zero();
        let mut prev = None;
        if cap_start {
            let s = segs[0];
            let pole = self.vertex(s.p - axis0 * s.r, -axis0, s.lit, 1.0);
            let a = self.ring(s.p, axis0, s, -60.0);
            let b = self.ring(s.p, axis0, s, -30.0);
            self.fan(pole, a);
            self.join(a, b, SIDES);
            prev = Some(b);
        }
        let (mut last_axis, mut cap_centre) = (axis0, segs[n - 1].p);
        for k in 0..n {
            let mut axis =
                (segs[(k + 1).min(n - 1)].p - segs[k.saturating_sub(1)].p).normalize_or_zero();
            if axis.length_squared() < 0.5 {
                axis = last_axis;
            }
            last_axis = axis;
            let mut at = segs[k].p;
            if k == n - 1 {
                // The tip: the last ring sits a radius back, so the cap's
                // pole is the tip itself.
                cap_centre = at - axis * segs[k].r;
                at = cap_centre;
            }
            let ring = self.ring(at, axis, segs[k], 0.0);
            if let Some(p) = prev {
                self.join(p, ring, SIDES);
            }
            prev = Some(ring);
        }
        let s = segs[n - 1];
        let c1 = self.ring(cap_centre, last_axis, s, 32.0);
        let c2 = self.ring(cap_centre, last_axis, s, 64.0);
        let tip = self.vertex(cap_centre + last_axis * s.r, last_axis, s.lit, 1.0);
        if let Some(p) = prev {
            self.join(p, c1, SIDES);
        }
        self.join(c1, c2, SIDES);
        self.fan(tip, c2);
    }

    /// A ring of `s`'s radius round `centre` across `axis`, tilted `tilt`
    /// degrees along it (for a cap's rings); its first vertex.
    fn ring(&mut self, centre: Vec3, axis: Vec3, s: Seg, tilt: f32) -> u16 {
        let u = s.up - axis * s.up.dot(axis);
        let u = if u.length_squared() > 1e-8 {
            u.normalize()
        } else {
            perp(axis)
        };
        let x = axis.cross(u);
        let (st, ct) = tilt.to_radians().sin_cos();
        let start = self.v.len() as u16;
        for k in 0..SIDES {
            let a = k as f32 * PI * 2.0 / SIDES as f32;
            let d = (u * a.cos() + x * a.sin()) * ct + axis * st;
            self.vertex(centre + d * s.r, d, s.lit, 1.0);
        }
        start
    }

    /// Quads between two loops of `count` vertices.
    fn join(&mut self, a: u16, b: u16, count: usize) {
        for k in 0..count as u16 {
            let k1 = (k + 1) % count as u16;
            self.i.extend([a + k, a + k1, b + k1, a + k, b + k1, b + k]);
        }
    }

    fn fan(&mut self, pole: u16, ring: u16) {
        for k in 0..SIDES as u16 {
            self.i
                .extend([pole, ring + k, ring + (k + 1) % SIDES as u16]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Quat;

    /// An open right hand, palm down, fingers along -Z.
    fn open_hand(x: f32) -> [Joint; 26] {
        let mut j = [Joint::default(); 26];
        let rot = Some(Quat::IDENTITY);
        j[PALM] = Joint {
            pos: Vec3::new(x, 1.0, -0.35),
            rot,
            radius: 0.0,
        };
        j[1] = Joint {
            pos: Vec3::new(x, 1.0, -0.3),
            rot,
            radius: 0.0,
        };
        for k in 0..4 {
            j[THUMB_META + k] = Joint {
                pos: Vec3::new(x - 0.03 - 0.015 * k as f32, 1.0, -0.31 - 0.025 * k as f32),
                rot,
                radius: 0.01,
            };
        }
        for f in 0..4 {
            for k in 0..5 {
                j[finger_base(f) + k] = Joint {
                    pos: Vec3::new(
                        x - 0.025 + 0.018 * f as f32,
                        1.0,
                        -0.31 - [0.0, 0.07, 0.11, 0.135, 0.155][k],
                    ),
                    rot,
                    radius: 0.008,
                };
            }
        }
        j
    }

    #[test]
    fn glove_mesh_is_whole() {
        let m = build(&open_hand(0.2));
        assert!(m.v.len() < u16::MAX as usize);
        assert_eq!(m.i.len() % 3, 0);
        assert!(m.i.iter().all(|&i| (i as usize) < m.v.len()));
        for v in &m.v {
            let n = Vec3::from_array(v.normal);
            assert!((n.length() - 1.0).abs() < 1e-3, "normal {n}");
            assert!(v.pos.iter().all(|c| c.is_finite()));
        }
        // Only the index finger's tip end is lit, fully at its very tip.
        let lit = m.v.iter().filter(|v| v.color[0] > 0.0).count();
        assert!(lit > 0 && lit < m.v.len() / 4);
        let brightest =
            m.v.iter()
                .max_by(|a, b| a.color[0].total_cmp(&b.color[0]))
                .unwrap();
        assert_eq!(brightest.color[0], 1.0);
        // Every pose gives the same topology.
        let other = build(&open_hand(-0.2));
        assert_eq!((other.v.len(), &other.i), (m.v.len(), &m.i));
    }

    #[test]
    fn gloves_fade_with_the_menus_and_reveal_only_with_passthrough() {
        let mut g = Gloves::default();
        let mut hands = [Hand::default(), Hand::default()];
        hands[1].tracked_hand = true;
        hands[1].joints = Some(open_hand(0.2));
        let d = g.frame(&hands, true, true, 0.2);
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].rim, d[0].reveal), (1.0, 1.0));
        assert_eq!(g.frame(&hands, true, false, 0.016)[0].reveal, 0.0);
        // The menus go: it fades out over a tenth of a second, then it's gone.
        assert_eq!(g.frame(&hands, false, false, 0.05).len(), 1);
        assert!(g.frame(&hands, false, false, 0.06).is_empty());
    }
}
