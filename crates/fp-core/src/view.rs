//! Per-video picture and placement corrections (HereSphere-style).

use serde::{Deserialize, Serialize};

/// Everything the viewer can adjust about how a video is shown. Defaults are
/// neutral. Persisted per video by the library and applied by the renderer.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewSettings {
    /// Rotates the whole image: yaw (left/right), pitch (tilt), roll, degrees.
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
    /// Magnification; 1.0 is natural size.
    pub zoom: f32,
    /// Software IPD: extra horizontal convergence between eyes, degrees.
    /// Positive pushes the image further away.
    pub ipd_offset: f32,
    /// Per-eye stereo alignment errors from the camera, degrees.
    pub vertical_align: f32,
    pub rotation_align: f32,
    /// Radial lens distortion correction (k1, k2) applied in source space.
    pub lens_k1: f32,
    pub lens_k2: f32,
    /// Swap left and right eyes.
    pub swap_eyes: bool,
    /// Picture: brightness and contrast around 0 and 1, saturation 1,
    /// gamma 1, sharpen 0..1.
    pub brightness: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub gamma: f32,
    pub sharpen: f32,
    /// Flat screen: distance in metres, width in metres, curvature 0..1.
    pub screen_distance: f32,
    pub screen_width: f32,
    pub screen_curvature: f32,
    /// Subtitle depth in metres.
    pub subtitle_distance: f32,
    /// Chroma key: makes one colour of the video see-through (a green or
    /// blue screen), so passthrough or the background shows there.
    pub chroma_key: bool,
    /// The colour to remove, sRGB 0..1.
    pub key_color: [f32; 3],
    /// How far (in chroma) from the key colour a pixel still disappears.
    pub key_similarity: f32,
    /// Width of the soft edge beyond that.
    pub key_smoothness: f32,
    /// How much key-coloured fringe to take out of what stays.
    pub key_spill: f32,
    /// Whether this video keeps its own chroma key rather than following the
    /// global one (Settings > Passthrough). `None` for settings saved before
    /// the choice existed: those keep their own if they had the key on.
    #[serde(default)]
    pub key_own: Option<bool>,
}

impl Default for ViewSettings {
    fn default() -> Self {
        ViewSettings {
            yaw: 0.0,
            pitch: 0.0,
            roll: 0.0,
            zoom: 1.0,
            ipd_offset: 0.0,
            vertical_align: 0.0,
            rotation_align: 0.0,
            lens_k1: 0.0,
            lens_k2: 0.0,
            swap_eyes: false,
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            gamma: 1.0,
            sharpen: 0.0,
            screen_distance: 4.0,
            screen_width: 6.0,
            screen_curvature: 0.0,
            subtitle_distance: 2.5,
            chroma_key: false,
            key_color: [0.08, 0.04, 1.0],
            key_similarity: 0.4,
            key_smoothness: 0.08,
            key_spill: 0.1,
            key_own: Some(false),
        }
    }
}

impl ViewSettings {
    /// Linear blend, used for keyframed settings.
    pub fn lerp(&self, other: &ViewSettings, t: f32) -> ViewSettings {
        let t = t.clamp(0.0, 1.0);
        let l = |a: f32, b: f32| a + (b - a) * t;
        ViewSettings {
            yaw: l(self.yaw, other.yaw),
            pitch: l(self.pitch, other.pitch),
            roll: l(self.roll, other.roll),
            zoom: l(self.zoom, other.zoom),
            ipd_offset: l(self.ipd_offset, other.ipd_offset),
            vertical_align: l(self.vertical_align, other.vertical_align),
            rotation_align: l(self.rotation_align, other.rotation_align),
            lens_k1: l(self.lens_k1, other.lens_k1),
            lens_k2: l(self.lens_k2, other.lens_k2),
            swap_eyes: if t < 0.5 {
                self.swap_eyes
            } else {
                other.swap_eyes
            },
            brightness: l(self.brightness, other.brightness),
            contrast: l(self.contrast, other.contrast),
            saturation: l(self.saturation, other.saturation),
            gamma: l(self.gamma, other.gamma),
            sharpen: l(self.sharpen, other.sharpen),
            screen_distance: l(self.screen_distance, other.screen_distance),
            screen_width: l(self.screen_width, other.screen_width),
            screen_curvature: l(self.screen_curvature, other.screen_curvature),
            subtitle_distance: l(self.subtitle_distance, other.subtitle_distance),
            chroma_key: if t < 0.5 {
                self.chroma_key
            } else {
                other.chroma_key
            },
            key_color: [
                l(self.key_color[0], other.key_color[0]),
                l(self.key_color[1], other.key_color[1]),
                l(self.key_color[2], other.key_color[2]),
            ],
            key_similarity: l(self.key_similarity, other.key_similarity),
            key_smoothness: l(self.key_smoothness, other.key_smoothness),
            key_spill: l(self.key_spill, other.key_spill),
            key_own: if t < 0.5 { self.key_own } else { other.key_own },
        }
    }

    /// Whether the chroma key is this video's own (see [`Self::key_own`]).
    pub fn own_key(&self) -> bool {
        self.key_own.unwrap_or(self.chroma_key)
    }

    /// The chroma key settings: on, colour, similarity, smoothness, spill.
    pub fn key(&self) -> (bool, [f32; 3], f32, f32, f32) {
        (
            self.chroma_key,
            self.key_color,
            self.key_similarity,
            self.key_smoothness,
            self.key_spill,
        )
    }

    /// Copies the chroma key settings of `from`.
    pub fn set_key(&mut self, from: &ViewSettings) {
        (
            self.chroma_key,
            self.key_color,
            self.key_similarity,
            self.key_smoothness,
            self.key_spill,
        ) = from.key();
    }

    /// These settings with `global`'s chroma key, unless the video has its own.
    pub fn with_global_key(mut self, global: &ViewSettings) -> ViewSettings {
        if !self.own_key() {
            self.set_key(global);
        }
        self
    }
}

/// Settings that change over the video's timeline.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Keyframes {
    /// (time in seconds, settings), sorted by time.
    pub frames: Vec<(f64, ViewSettings)>,
}

impl Keyframes {
    pub fn insert(&mut self, time: f64, settings: ViewSettings) {
        match self
            .frames
            .iter()
            .position(|(t, _)| (*t - time).abs() < 1e-3)
        {
            Some(i) => self.frames[i].1 = settings,
            None => {
                self.frames.push((time, settings));
                self.frames.sort_by(|a, b| a.0.total_cmp(&b.0));
            }
        }
    }

    /// Settings at `time`, interpolated; `base` when there are no keyframes.
    pub fn at(&self, time: f64, base: &ViewSettings) -> ViewSettings {
        match self.frames.len() {
            0 => *base,
            _ => {
                let i = self.frames.partition_point(|(t, _)| *t <= time);
                if i == 0 {
                    self.frames[0].1
                } else if i == self.frames.len() {
                    self.frames[i - 1].1
                } else {
                    let (t0, a) = &self.frames[i - 1];
                    let (t1, b) = &self.frames[i];
                    a.lerp(b, ((time - t0) / (t1 - t0)) as f32)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframes_interpolate_and_clamp() {
        let base = ViewSettings::default();
        let mut k = Keyframes::default();
        assert_eq!(k.at(5.0, &base), base);
        k.insert(10.0, ViewSettings { zoom: 2.0, ..base });
        k.insert(0.0, ViewSettings { zoom: 1.0, ..base });
        assert_eq!(k.frames[0].0, 0.0);
        assert!((k.at(5.0, &base).zoom - 1.5).abs() < 1e-6);
        assert_eq!(k.at(-1.0, &base).zoom, 1.0);
        assert_eq!(k.at(99.0, &base).zoom, 2.0);
        k.insert(10.0, ViewSettings { zoom: 3.0, ..base });
        assert_eq!(k.frames.len(), 2);
        assert_eq!(k.at(10.0, &base).zoom, 3.0);
    }

    #[test]
    fn partial_json_uses_defaults() {
        let v: ViewSettings = serde_json::from_str(r#"{"zoom":1.2}"#).unwrap();
        assert_eq!(v.zoom, 1.2);
        assert_eq!(v.contrast, 1.0);
    }

    #[test]
    fn videos_follow_the_global_key_unless_they_have_their_own() {
        let global = ViewSettings {
            chroma_key: true,
            key_color: [0.0, 0.0, 1.0],
            ..Default::default()
        };
        let video = ViewSettings::default();
        let shown = video.with_global_key(&global);
        assert!(shown.chroma_key);
        assert_eq!(shown.key_color, [0.0, 0.0, 1.0]);

        let own = ViewSettings {
            key_own: Some(true),
            ..video
        };
        assert!(!own.with_global_key(&global).chroma_key);

        // Saved before the choice existed: a keyed video keeps its key, an
        // unkeyed one follows the global setting.
        let old: ViewSettings =
            serde_json::from_str(r#"{"chroma_key":true,"key_color":[1,0,1]}"#).unwrap();
        assert_eq!(old.with_global_key(&global).key_color, [1.0, 0.0, 1.0]);
        let old: ViewSettings = serde_json::from_str(r#"{"zoom":1.2}"#).unwrap();
        assert!(old.with_global_key(&global).chroma_key);
    }
}
