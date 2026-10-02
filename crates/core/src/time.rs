//! Media timestamps.

use serde::{Deserialize, Serialize};
use std::ops::{Add, Sub};
use std::time::Duration;

/// A presentation timestamp in microseconds. Signed so that A/V offsets and
/// "before start" positions are representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct MediaTime(pub i64);

impl MediaTime {
    pub const ZERO: MediaTime = MediaTime(0);

    pub fn from_secs_f64(s: f64) -> Self {
        MediaTime((s * 1_000_000.0).round() as i64)
    }
    pub fn from_millis(ms: i64) -> Self {
        MediaTime(ms * 1000)
    }
    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }
    pub fn as_millis(self) -> i64 {
        self.0.div_euclid(1000)
    }
    pub fn micros(self) -> i64 {
        self.0
    }
    /// Convert a stream timestamp in `num/den` time base units.
    pub fn from_timebase(pts: i64, num: i32, den: i32) -> Self {
        let v = pts as i128 * num as i128 * 1_000_000 / den as i128;
        MediaTime(v as i64)
    }
    pub fn clamp_to(self, lo: MediaTime, hi: MediaTime) -> Self {
        MediaTime(self.0.clamp(lo.0, hi.0))
    }
}

impl Add for MediaTime {
    type Output = MediaTime;
    fn add(self, o: MediaTime) -> MediaTime {
        MediaTime(self.0 + o.0)
    }
}
impl Sub for MediaTime {
    type Output = MediaTime;
    fn sub(self, o: MediaTime) -> MediaTime {
        MediaTime(self.0 - o.0)
    }
}
impl From<Duration> for MediaTime {
    fn from(d: Duration) -> Self {
        MediaTime(d.as_micros() as i64)
    }
}

impl std::fmt::Display for MediaTime {
    /// `H:MM:SS.mmm`, or `M:SS.mmm` under an hour.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let neg = self.0 < 0;
        let total_ms = self.0.unsigned_abs() / 1000;
        let (h, m, s, ms) = (total_ms / 3_600_000, (total_ms / 60_000) % 60, (total_ms / 1000) % 60, total_ms % 1000);
        if neg {
            write!(f, "-")?;
        }
        if h > 0 {
            write!(f, "{h}:{m:02}:{s:02}.{ms:03}")
        } else {
            write!(f, "{m}:{s:02}.{ms:03}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timebase_conversion() {
        assert_eq!(MediaTime::from_timebase(90_000, 1, 90_000), MediaTime(1_000_000));
        assert_eq!(MediaTime::from_timebase(1001, 1, 30_000).0, 33_366);
    }

    #[test]
    fn display() {
        assert_eq!(MediaTime::from_millis(3_723_004).to_string(), "1:02:03.004");
        assert_eq!(MediaTime::from_millis(65_500).to_string(), "1:05.500");
        assert_eq!(MediaTime::from_millis(-1500).to_string(), "-0:01.500");
    }
}
