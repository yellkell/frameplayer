//! Pinch detection from hand-tracking joints with hysteresis, so a pinch
//! held near the threshold does not chatter.

use glam::Vec3;

/// Distances are between the thumb-tip and index-tip joint *surfaces*
/// (centre distance minus both joint radii).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PinchConfig {
    /// Gap below which a pinch starts (metres).
    pub engage_m: f32,
    /// Gap above which a pinch ends (metres). Must exceed `engage_m`.
    pub release_m: f32,
    /// Gap at which `strength` reaches 0.
    pub open_m: f32,
}

impl Default for PinchConfig {
    fn default() -> Self {
        // [verify] tune against Frame hand tracking noise.
        PinchConfig {
            engage_m: 0.010,
            release_m: 0.025,
            open_m: 0.08,
        }
    }
}

/// Per-frame pinch output.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PinchState {
    pub pinching: bool,
    pub just_pinched: bool,
    pub just_released: bool,
    /// 0 = open hand … 1 = touching.
    pub strength: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PinchDetector {
    pub config: PinchConfig,
    pinching: bool,
}

impl PinchDetector {
    pub fn new(config: PinchConfig) -> PinchDetector {
        PinchDetector {
            config,
            pinching: false,
        }
    }

    /// Feed one frame. `None` (hand not tracked) releases any pinch.
    pub fn update(&mut self, joints: Option<(Vec3, f32, Vec3, f32)>) -> PinchState {
        let was = self.pinching;
        let (gap, tracked) = match joints {
            Some((thumb, thumb_r, index, index_r)) => (
                ((thumb - index).length() - thumb_r - index_r).max(0.0),
                true,
            ),
            None => (f32::INFINITY, false),
        };
        self.pinching = if !tracked {
            false
        } else if was {
            gap < self.config.release_m
        } else {
            gap < self.config.engage_m
        };
        let span = (self.config.open_m - self.config.engage_m).max(1e-6);
        let strength = if tracked {
            (1.0 - (gap - self.config.engage_m) / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        PinchState {
            pinching: self.pinching,
            just_pinched: self.pinching && !was,
            just_released: !self.pinching && was,
            strength,
        }
    }

    pub fn is_pinching(&self) -> bool {
        self.pinching
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(gap: f32) -> Option<(Vec3, f32, Vec3, f32)> {
        // Radii of 5 mm each; centre distance = gap + 10 mm.
        Some((Vec3::ZERO, 0.005, Vec3::new(gap + 0.010, 0.0, 0.0), 0.005))
    }

    #[test]
    fn hysteresis() {
        let mut d = PinchDetector::default();
        assert!(!d.update(at(0.05)).pinching);
        assert!(
            !d.update(at(0.015)).pinching,
            "between thresholds, not yet engaged"
        );
        let s = d.update(at(0.005));
        assert!(s.pinching && s.just_pinched);
        let s = d.update(at(0.02));
        assert!(
            s.pinching && !s.just_pinched,
            "held between thresholds stays engaged"
        );
        let s = d.update(at(0.03));
        assert!(!s.pinching && s.just_released);
        assert!(!d.update(at(0.02)).pinching);
    }

    #[test]
    fn tracking_loss_releases() {
        let mut d = PinchDetector::default();
        assert!(d.update(at(0.0)).just_pinched);
        let s = d.update(None);
        assert!(!s.pinching && s.just_released && s.strength == 0.0);
    }

    #[test]
    fn strength_is_monotonic() {
        let mut d = PinchDetector::default();
        assert_eq!(d.update(at(0.0)).strength, 1.0);
        assert_eq!(d.update(at(0.2)).strength, 0.0);
        let mut prev = 1.0;
        for i in 0..100 {
            let s = PinchDetector::default()
                .update(at(i as f32 * 0.001))
                .strength;
            assert!(s <= prev);
            prev = s;
        }
    }
}
