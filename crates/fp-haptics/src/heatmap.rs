//! Script heatmaps for the timeline strip: each time bucket coloured by how
//! fast the script moves there, in the style of DeoVR and OFS.
//!
//! A bucket's speed is the distance the script travels inside the bucket
//! divided by the bucket's length, in funscript units (0–100) per second.
//! Pauses therefore pull a bucket towards the stationary colour.

use crate::script::Script;

/// Speed (units per second) at which the heatmap saturates: full red and
/// intensity 1.
pub const MAX_HEAT_SPEED: f32 = 500.0;

/// Gradient stops: (speed in units/s, colour).
const STOPS: [(f32, [u8; 3]); 7] = [
    (0.0, [0x0b, 0x12, 0x3a]),            // stationary: dark blue
    (60.0, [0x1e, 0x50, 0xc8]),           // creeping: blue
    (140.0, [0x00, 0xbe, 0xd2]),          // slow: cyan
    (230.0, [0x32, 0xc8, 0x50]),          // moderate: green
    (320.0, [0xf0, 0xdc, 0x28]),          // fast: yellow
    (410.0, [0xf5, 0x82, 0x1e]),          // faster: orange
    (MAX_HEAT_SPEED, [0xdc, 0x1e, 0x1e]), // very fast: red
];

/// The heatmap colour for a speed in funscript units per second.
pub fn speed_color(speed: f32) -> [u8; 3] {
    let speed = if speed.is_finite() {
        speed.max(0.0)
    } else {
        0.0
    };
    for w in STOPS.windows(2) {
        let (s0, c0) = w[0];
        let (s1, c1) = w[1];
        if speed <= s1 {
            let t = ((speed - s0) / (s1 - s0)).clamp(0.0, 1.0);
            let mix =
                |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
            return [mix(c0[0], c1[0]), mix(c0[1], c1[1]), mix(c0[2], c1[2])];
        }
    }
    STOPS[STOPS.len() - 1].1
}

/// Average speed (units per second) in each of `buckets` equal slices of
/// `start_ms..end_ms`.
pub fn bucket_speeds(script: &Script, start_ms: i64, end_ms: i64, buckets: usize) -> Vec<f32> {
    if buckets == 0 {
        return Vec::new();
    }
    let span = (end_ms - start_ms).max(1) as f64;
    let len = span / buckets as f64;
    let mut dist = vec![0.0f64; buckets];
    for w in script.actions().windows(2) {
        let (a, b) = (w[0], w[1]);
        let dt = (b.at - a.at) as f64;
        let dp = f64::from((b.pos - a.pos).abs()) * 100.0;
        if dt <= 0.0 || dp == 0.0 {
            continue;
        }
        let per_ms = dp / dt;
        let s = (a.at.max(start_ms) - start_ms) as f64;
        let e = (b.at.min(end_ms) - start_ms) as f64;
        if e <= s {
            continue;
        }
        let first = (s / len).floor() as usize;
        let last = ((e / len).ceil() as usize).min(buckets);
        for (i, d) in dist.iter_mut().enumerate().take(last).skip(first) {
            let b0 = i as f64 * len;
            let overlap = e.min(b0 + len) - s.max(b0);
            if overlap > 0.0 {
                *d += overlap * per_ms;
            }
        }
    }
    dist.into_iter()
        .map(|d| (d / (len / 1000.0)) as f32)
        .collect()
}

/// A heatmap over a time range: one entry per bucket in each vector.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Heatmap {
    /// RGB colour per bucket.
    pub colors: Vec<[u8; 3]>,
    /// Intensity per bucket, `0.0..=1.0` (speed / [`MAX_HEAT_SPEED`]); for
    /// drawing bar heights or alpha.
    pub intensity: Vec<f32>,
    /// Average speed per bucket in funscript units per second.
    pub speeds: Vec<f32>,
}

/// Heatmap of `start_ms..end_ms` (for example `0..video duration`, so the
/// strip lines up with the seek bar even when the script ends early).
pub fn heatmap_range(script: &Script, start_ms: i64, end_ms: i64, buckets: usize) -> Heatmap {
    let speeds = bucket_speeds(script, start_ms, end_ms, buckets);
    Heatmap {
        colors: speeds.iter().map(|&s| speed_color(s)).collect(),
        intensity: speeds
            .iter()
            .map(|&s| (s / MAX_HEAT_SPEED).clamp(0.0, 1.0))
            .collect(),
        speeds,
    }
}

/// Colours for `buckets` equal slices of the script's own duration
/// (`0..last action`).
pub fn heatmap(script: &Script, buckets: usize) -> Vec<[u8; 3]> {
    heatmap_range(script, 0, script.duration_ms(), buckets).colors
}

/// Intensities (`0.0..=1.0`) for `buckets` equal slices of the script's own
/// duration, matching [`heatmap`].
pub fn heatmap_intensity(script: &Script, buckets: usize) -> Vec<f32> {
    heatmap_range(script, 0, script.duration_ms(), buckets).intensity
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::Action;

    #[test]
    fn colour_gradient() {
        assert_eq!(speed_color(0.0), STOPS[0].1);
        assert_eq!(speed_color(-5.0), STOPS[0].1);
        assert_eq!(speed_color(f32::NAN), STOPS[0].1);
        assert_eq!(speed_color(140.0), [0x00, 0xbe, 0xd2]);
        assert_eq!(speed_color(10_000.0), [0xdc, 0x1e, 0x1e]);
        // Fast is redder than slow.
        assert!(speed_color(450.0)[0] > speed_color(100.0)[0]);
        assert!(speed_color(100.0)[2] > speed_color(450.0)[2]);
    }

    #[test]
    fn buckets_reflect_speed() {
        // 0-2 s: 0→100→0 (100 u/s each way); 2-4 s: hold; 4-5 s: five
        // 100-unit strokes (500 u/s).
        let mut acts = vec![
            Action::new(0, 0.0),
            Action::new(1000, 1.0),
            Action::new(2000, 0.0),
            Action::new(4000, 0.0),
        ];
        for i in 1..=5 {
            acts.push(Action::new(
                4000 + i * 200,
                if i % 2 == 1 { 1.0 } else { 0.0 },
            ));
        }
        let sc = Script::new(acts);
        let speeds = bucket_speeds(&sc, 0, 5000, 5);
        assert!((speeds[0] - 100.0).abs() < 1e-3, "{speeds:?}");
        assert!((speeds[1] - 100.0).abs() < 1e-3);
        assert_eq!(speeds[2], 0.0);
        assert_eq!(speeds[3], 0.0);
        assert!((speeds[4] - 500.0).abs() < 1e-3);

        let colors = heatmap(&sc, 5);
        assert_eq!(colors.len(), 5);
        assert_eq!(colors[2], STOPS[0].1);
        assert_eq!(colors[4], [0xdc, 0x1e, 0x1e]);
        let inten = heatmap_intensity(&sc, 5);
        assert!((inten[0] - 0.2).abs() < 1e-3);
        assert_eq!(inten[4], 1.0);

        // A range wider than the script leaves the tail stationary, and
        // segments split across buckets are apportioned.
        let hm = heatmap_range(&sc, 0, 10_000, 4);
        assert!((hm.speeds[0] - 80.0).abs() < 1e-3, "{:?}", hm.speeds);
        assert!((hm.speeds[1] - 200.0).abs() < 1e-3);
        assert_eq!(hm.speeds[2], 0.0);
        assert_eq!(hm.speeds[3], 0.0);
        assert!(heatmap(&Script::default(), 0).is_empty());
        assert_eq!(heatmap(&Script::default(), 3), vec![STOPS[0].1; 3]);
    }
}
