//! How a video frame maps onto the viewer's sphere / screen, and the
//! per-video picture corrections layered on top.

use serde::{Deserialize, Serialize};

/// Named fisheye lens presets. FOV and centre values for presets are the
/// published nominal values; per-file overrides go in [`Projection::Fisheye`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FisheyeLens {
    /// Generic equidistant fisheye with an explicit FOV.
    Generic,
    /// Canon RF 5.2mm F2.8 L Dual Fisheye (190° per eye).
    CanonRf52,
    /// MKX200 (200° per eye, slightly compressed edges).
    Mkx200,
    /// MKX220 (220° per eye).
    Mkx220,
}

impl FisheyeLens {
    /// Nominal per-eye field of view in degrees.
    pub fn nominal_fov_deg(self) -> f32 {
        match self {
            FisheyeLens::Generic => 180.0,
            FisheyeLens::CanonRf52 => 190.0,
            FisheyeLens::Mkx200 => 200.0,
            FisheyeLens::Mkx220 => 220.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Projection {
    /// A rectangular virtual screen. `curvature` 0 = flat, 1 = cylinder
    /// segment centred on the viewer at `distance_m`.
    Flat {
        width_m: f32,
        distance_m: f32,
        curvature: f32,
    },
    /// Equirectangular covering `h_fov_deg` horizontally (180 or 360) and
    /// 180° vertically.
    Equirect { h_fov_deg: f32 },
    /// Equidistant fisheye, one image circle per eye.
    Fisheye {
        lens: FisheyeLens,
        fov_deg: f32,
        /// Image-circle centre offset in normalized eye-image coordinates
        /// (0,0 = centre; ±0.5 = edge).
        center_x: f32,
        center_y: f32,
        /// Image-circle radius as a fraction of the eye image's half-height
        /// (1.0 = circle touches top/bottom edges).
        radius: f32,
    },
    /// YouTube equi-angular cubemap.
    Eac,
    /// User-supplied OBJ mesh with UVs; path relative to the app data dir.
    CustomMesh { path: String },
}

impl Projection {
    pub const FLAT_DEFAULT: Projection = Projection::Flat {
        width_m: 4.0,
        distance_m: 3.5,
        curvature: 0.0,
    };
    pub const EQUIRECT_180: Projection = Projection::Equirect { h_fov_deg: 180.0 };
    pub const EQUIRECT_360: Projection = Projection::Equirect { h_fov_deg: 360.0 };

    pub fn fisheye(lens: FisheyeLens) -> Projection {
        Projection::Fisheye {
            lens,
            fov_deg: lens.nominal_fov_deg(),
            center_x: 0.0,
            center_y: 0.0,
            radius: 1.0,
        }
    }

    pub fn fisheye_fov(fov_deg: f32) -> Projection {
        Projection::Fisheye {
            lens: FisheyeLens::Generic,
            fov_deg,
            center_x: 0.0,
            center_y: 0.0,
            radius: 1.0,
        }
    }

    pub fn is_immersive(&self) -> bool {
        !matches!(self, Projection::Flat { .. })
    }

    /// Short human label for UI badges.
    pub fn label(&self) -> String {
        match self {
            Projection::Flat { .. } => "Flat".into(),
            Projection::Equirect { h_fov_deg } => format!("{}°", h_fov_deg.round() as i32),
            Projection::Fisheye {
                lens: FisheyeLens::CanonRf52,
                ..
            } => "RF 5.2".into(),
            Projection::Fisheye {
                lens: FisheyeLens::Mkx200,
                ..
            } => "MKX200".into(),
            Projection::Fisheye {
                lens: FisheyeLens::Mkx220,
                ..
            } => "MKX220".into(),
            Projection::Fisheye { fov_deg, .. } => format!("Fisheye {}°", fov_deg.round() as i32),
            Projection::Eac => "EAC".into(),
            Projection::CustomMesh { .. } => "Mesh".into(),
        }
    }
}

impl Default for Projection {
    fn default() -> Self {
        Projection::FLAT_DEFAULT
    }
}

/// How the two eye views are packed into one decoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StereoMode {
    #[default]
    Mono,
    /// Side by side, left eye on the left.
    Sbs,
    /// Over/under, left eye on top.
    Ou,
}

impl StereoMode {
    /// UV sub-rectangle `(u0, v0, u1, v1)` holding the given eye
    /// (0 = left, 1 = right) before any swap.
    pub fn eye_rect(self, eye: usize, swap: bool) -> [f32; 4] {
        let e = if swap { 1 - eye.min(1) } else { eye.min(1) };
        match self {
            StereoMode::Mono => [0.0, 0.0, 1.0, 1.0],
            StereoMode::Sbs => {
                let u0 = 0.5 * e as f32;
                [u0, 0.0, u0 + 0.5, 1.0]
            }
            StereoMode::Ou => {
                let v0 = 0.5 * e as f32;
                [0.0, v0, 1.0, v0 + 0.5]
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StereoMode::Mono => "2D",
            StereoMode::Sbs => "SBS",
            StereoMode::Ou => "OU",
        }
    }
}

/// HereSphere-class per-video picture corrections. All defaults are neutral.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Corrections {
    /// Software IPD adjustment in degrees of yaw applied symmetrically
    /// (positive pushes the image apart / further away).
    pub ipd_deg: f32,
    /// Vertical misalignment between eyes, degrees (applied +½ / -½).
    pub vertical_align_deg: f32,
    /// Horizontal per-eye offset, degrees.
    pub horizontal_align_deg: f32,
    /// Radial lens distortion coefficients (Brown–Conrady), applied in
    /// eye-image space.
    pub k1: f32,
    pub k2: f32,
    /// Zoom factor (1.0 = native).
    pub zoom: f32,
    /// Whole-sphere rotation, degrees.
    pub yaw_deg: f32,
    pub pitch_deg: f32,
    pub roll_deg: f32,
    /// Per-eye crop as a fraction trimmed from each edge (l, t, r, b).
    pub crop: [f32; 4],
    /// Exposure in stops, applied before tone mapping.
    pub exposure_ev: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub sharpen: f32,
}

impl Default for Corrections {
    fn default() -> Self {
        Corrections {
            ipd_deg: 0.0,
            vertical_align_deg: 0.0,
            horizontal_align_deg: 0.0,
            k1: 0.0,
            k2: 0.0,
            zoom: 1.0,
            yaw_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
            crop: [0.0; 4],
            exposure_ev: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            sharpen: 0.0,
        }
    }
}

impl Corrections {
    /// Linear interpolation, used for keyframed settings over time.
    pub fn lerp(&self, o: &Corrections, t: f32) -> Corrections {
        let l = |a: f32, b: f32| a + (b - a) * t;
        Corrections {
            ipd_deg: l(self.ipd_deg, o.ipd_deg),
            vertical_align_deg: l(self.vertical_align_deg, o.vertical_align_deg),
            horizontal_align_deg: l(self.horizontal_align_deg, o.horizontal_align_deg),
            k1: l(self.k1, o.k1),
            k2: l(self.k2, o.k2),
            zoom: l(self.zoom, o.zoom),
            yaw_deg: l(self.yaw_deg, o.yaw_deg),
            pitch_deg: l(self.pitch_deg, o.pitch_deg),
            roll_deg: l(self.roll_deg, o.roll_deg),
            crop: std::array::from_fn(|i| l(self.crop[i], o.crop[i])),
            exposure_ev: l(self.exposure_ev, o.exposure_ev),
            contrast: l(self.contrast, o.contrast),
            saturation: l(self.saturation, o.saturation),
            sharpen: l(self.sharpen, o.sharpen),
        }
    }
}

/// A correction snapshot pinned to a time in the video.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CorrectionKeyframe {
    pub at: crate::time::MediaTime,
    pub corrections: Corrections,
}

/// Everything needed to put a video in front of the viewer.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewSettings {
    pub projection: Projection,
    pub stereo: StereoMode,
    pub swap_eyes: bool,
    pub corrections: Corrections,
    /// Sorted by time; when non-empty, overrides `corrections` with the
    /// interpolated value at the playback position.
    pub keyframes: Vec<CorrectionKeyframe>,
}

impl ViewSettings {
    /// Corrections in effect at `t`, interpolating between keyframes and
    /// holding the first/last value outside the keyed range.
    pub fn corrections_at(&self, t: crate::time::MediaTime) -> Corrections {
        let k = &self.keyframes;
        match k.len() {
            0 => self.corrections,
            1 => k[0].corrections,
            _ => {
                if t <= k[0].at {
                    return k[0].corrections;
                }
                if t >= k[k.len() - 1].at {
                    return k[k.len() - 1].corrections;
                }
                let i = k.partition_point(|kf| kf.at <= t);
                let (a, b) = (&k[i - 1], &k[i]);
                let span = (b.at.0 - a.at.0) as f32;
                let f = if span > 0.0 {
                    (t.0 - a.at.0) as f32 / span
                } else {
                    0.0
                };
                a.corrections.lerp(&b.corrections, f)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::MediaTime;

    #[test]
    fn eye_rects() {
        assert_eq!(StereoMode::Sbs.eye_rect(0, false), [0.0, 0.0, 0.5, 1.0]);
        assert_eq!(StereoMode::Sbs.eye_rect(1, false), [0.5, 0.0, 1.0, 1.0]);
        assert_eq!(StereoMode::Sbs.eye_rect(0, true), [0.5, 0.0, 1.0, 1.0]);
        assert_eq!(StereoMode::Ou.eye_rect(1, false), [0.0, 0.5, 1.0, 1.0]);
        assert_eq!(StereoMode::Mono.eye_rect(1, true), [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn keyframe_interpolation() {
        let a = Corrections::default();
        let b = Corrections { zoom: 2.0, ..a };
        let vs = ViewSettings {
            keyframes: vec![
                CorrectionKeyframe {
                    at: MediaTime::from_millis(1000),
                    corrections: a,
                },
                CorrectionKeyframe {
                    at: MediaTime::from_millis(3000),
                    corrections: b,
                },
            ],
            ..Default::default()
        };
        assert_eq!(vs.corrections_at(MediaTime::ZERO).zoom, 1.0);
        assert!((vs.corrections_at(MediaTime::from_millis(2000)).zoom - 1.5).abs() < 1e-6);
        assert_eq!(vs.corrections_at(MediaTime::from_millis(9000)).zoom, 2.0);
    }

    #[test]
    fn serde_roundtrip() {
        let vs = ViewSettings {
            projection: Projection::fisheye(FisheyeLens::Mkx200),
            stereo: StereoMode::Sbs,
            ..Default::default()
        };
        let s = serde_json::to_string(&vs).unwrap();
        let back: ViewSettings = serde_json::from_str(&s).unwrap();
        assert_eq!(vs, back);
    }
}
