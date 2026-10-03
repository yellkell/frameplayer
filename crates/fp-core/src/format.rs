//! Projection and stereo layout of a video, and detection of both from file
//! names (DeoVR / HereSphere conventions) and container metadata.

use serde::{Deserialize, Serialize};

/// How the image maps onto the viewer's field of view.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Projection {
    /// A rectangular screen in front of the viewer (2D or 3D movies).
    Flat,
    /// Equirectangular: `h_fov` degrees wide (180 or 360), `v_fov` tall (180).
    Equirect { h_fov: f32, v_fov: f32 },
    /// Equidistant fisheye with the given full field of view in degrees.
    /// Used by most dual-fisheye VR cameras (MKX200, MKX220, Canon RF 5.2mm).
    Fisheye { fov: f32 },
    /// YouTube equi-angular cubemap, 360 (6 faces) or 180 (front half).
    Eac { h_fov: f32 },
}

impl Projection {
    pub const EQUIRECT_180: Projection = Projection::Equirect {
        h_fov: 180.0,
        v_fov: 180.0,
    };
    pub const EQUIRECT_360: Projection = Projection::Equirect {
        h_fov: 360.0,
        v_fov: 180.0,
    };

    pub fn fisheye(fov: f32) -> Projection {
        Projection::Fisheye { fov }
    }

    /// Short label for UI and file-name suffixes.
    pub fn label(&self) -> String {
        match *self {
            Projection::Flat => "Flat".into(),
            Projection::Equirect { h_fov, .. } => format!("{}°", h_fov.round()),
            Projection::Fisheye { fov } => format!("Fisheye {}°", fov.round()),
            Projection::Eac { h_fov } => format!("EAC {}°", h_fov.round()),
        }
    }

    /// Projections offered in the UI, in display order.
    pub fn presets() -> Vec<(&'static str, Projection)> {
        vec![
            ("Flat screen", Projection::Flat),
            ("180°", Projection::EQUIRECT_180),
            ("360°", Projection::EQUIRECT_360),
            ("Fisheye 180°", Projection::fisheye(180.0)),
            ("Fisheye 190° (Canon RF 5.2)", Projection::fisheye(190.0)),
            ("Fisheye 200° (MKX200)", Projection::fisheye(200.0)),
            ("Fisheye 220° (MKX220/VRCA220)", Projection::fisheye(220.0)),
            ("EAC 360° (YouTube)", Projection::Eac { h_fov: 360.0 }),
            ("EAC 180°", Projection::Eac { h_fov: 180.0 }),
        ]
    }
}

/// How the two eyes are packed in each frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StereoLayout {
    #[default]
    Mono,
    /// Left eye on the left half.
    SideBySide,
    /// Left eye on the top half.
    TopBottom,
}

impl StereoLayout {
    pub fn label(&self) -> &'static str {
        match self {
            StereoLayout::Mono => "Mono",
            StereoLayout::SideBySide => "Side by side",
            StereoLayout::TopBottom => "Top/bottom",
        }
    }
}

/// Complete format of a video.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoFormat {
    pub projection: Projection,
    pub stereo: StereoLayout,
    /// The file packs the right eye first (`_RL`, `_BT`).
    #[serde(default)]
    pub eyes_swapped: bool,
}

impl Default for VideoFormat {
    fn default() -> Self {
        VideoFormat {
            projection: Projection::Flat,
            stereo: StereoLayout::Mono,
            eyes_swapped: false,
        }
    }
}

impl VideoFormat {
    pub const fn new(projection: Projection, stereo: StereoLayout) -> Self {
        VideoFormat {
            projection,
            stereo,
            eyes_swapped: false,
        }
    }

    pub fn label(&self) -> String {
        match self.stereo {
            StereoLayout::Mono => format!("{} mono", self.projection.label()),
            s => format!("{} {}", self.projection.label(), s.label()),
        }
    }
}

/// Where a detected format came from, strongest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// The user chose it for this file.
    User,
    /// Spherical / stereo-3D metadata in the container.
    Metadata,
    /// Tokens in the file name.
    FileName,
    /// Guessed from the frame aspect ratio.
    AspectRatio,
    /// Nothing known: plain flat 2D.
    Default,
}

impl Evidence {
    /// Where the format came from, for the UI.
    pub fn label(&self) -> &'static str {
        match self {
            Evidence::User => "your choice",
            Evidence::Metadata => "from file metadata",
            Evidence::FileName => "from the file name",
            Evidence::AspectRatio => "guessed from the shape",
            Evidence::Default => "default",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DetectedFormat {
    pub format: VideoFormat,
    pub evidence: Evidence,
}

/// Splits a file name into upper-case tokens on any non-alphanumeric
/// character, keeping the extension out.
fn tokens(name: &str) -> Vec<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = match base.rfind('.') {
        Some(i) if i > 0 && base.len() - i <= 5 => &base[..i],
        _ => base,
    };
    stem.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_uppercase())
        .collect()
}

/// Detects projection and stereo layout from a file name.
///
/// Follows the conventions DeoVR and HereSphere document: `_LR`/`_SBS`/`_3DH`
/// for side by side, `_TB`/`_OU`/`_3DV` for top/bottom, `_180`/`_360`,
/// `_FISHEYE`, `_FISHEYE190`, `_MKX200`, `_MKX220`, `_RF52`, `_VRCA220`,
/// `_EAC`, `_FLAT`/`_2D`/`_MONO`. Returns `None` when the name says nothing.
pub fn detect_from_name(name: &str) -> Option<VideoFormat> {
    let toks = tokens(name);
    let has = |t: &str| toks.iter().any(|x| x == t);
    let mut projection = None;
    let mut stereo = None;
    let mut swapped = false;

    for t in &toks {
        match t.as_str() {
            "LR" | "SBS" | "3DH" | "HSBS" | "FSBS" => stereo = Some(StereoLayout::SideBySide),
            "RL" => {
                stereo = Some(StereoLayout::SideBySide);
                swapped = true;
            }
            "TB" | "OU" | "3DV" | "HOU" | "FOU" | "HTB" | "FTB" | "OVERUNDER" | "TOPBOTTOM" => {
                stereo = Some(StereoLayout::TopBottom)
            }
            "BT" => {
                stereo = Some(StereoLayout::TopBottom);
                swapped = true;
            }
            "MONO" | "2D" => stereo = Some(StereoLayout::Mono),
            "180" | "180X180" | "DOME" => projection = Some(Projection::EQUIRECT_180),
            "360" | "360X180" | "SPHERE" => projection = Some(Projection::EQUIRECT_360),
            "FLAT" => projection = Some(Projection::Flat),
            "EAC" | "EAC360" => projection = Some(Projection::Eac { h_fov: 360.0 }),
            "EAC180" => projection = Some(Projection::Eac { h_fov: 180.0 }),
            "MKX200" => projection = Some(Projection::fisheye(200.0)),
            "MKX220" | "VRCA220" => projection = Some(Projection::fisheye(220.0)),
            "RF52" => projection = Some(Projection::fisheye(190.0)),
            "FISHEYE" => projection = Some(Projection::fisheye(180.0)),
            _ => {
                if let Some(deg) = t
                    .strip_prefix("FISHEYE")
                    .and_then(|d| d.parse::<f32>().ok())
                    && (120.0..=270.0).contains(&deg)
                {
                    projection = Some(Projection::fisheye(deg));
                }
            }
        }
    }

    // A 180/360 tag on its own implies stereo SBS for 180 (the norm) and mono
    // for 360, unless the name said otherwise.
    if projection.is_none() && stereo.is_none() {
        return None;
    }
    // "VR" plus a stereo tag and no projection means the common 180 format.
    let projection = projection.unwrap_or(
        if stereo.is_some_and(|s| s != StereoLayout::Mono) && has("VR") {
            Projection::EQUIRECT_180
        } else {
            Projection::Flat
        },
    );
    let stereo = stereo.unwrap_or(match projection {
        Projection::Equirect { h_fov, .. } if h_fov <= 180.0 => StereoLayout::SideBySide,
        Projection::Fisheye { .. } => StereoLayout::SideBySide,
        Projection::Eac { h_fov } if h_fov <= 180.0 => StereoLayout::SideBySide,
        _ => StereoLayout::Mono,
    });
    Some(VideoFormat {
        projection,
        stereo,
        eyes_swapped: swapped,
    })
}

/// Guesses a format from the frame size when nothing else is known. Only
/// confident cases: 2:1 is mono 360, 1:1 halves are stereo 180.
pub fn guess_from_aspect(width: u32, height: u32) -> Option<VideoFormat> {
    if width == 0 || height == 0 {
        return None;
    }
    let ar = width as f32 / height as f32;
    // 3840x1920 and similar: SBS 180 (two square eyes) or mono 360 (2:1).
    // The two are indistinguishable by shape; VR players default to SBS 180
    // for high resolutions because it is far more common for VR video.
    if (ar - 2.0).abs() < 0.02 && width >= 3840 {
        return Some(VideoFormat::new(
            Projection::EQUIRECT_180,
            StereoLayout::SideBySide,
        ));
    }
    if (ar - 2.0).abs() < 0.02 {
        return Some(VideoFormat::new(
            Projection::EQUIRECT_360,
            StereoLayout::Mono,
        ));
    }
    if (ar - 1.0).abs() < 0.02 && width >= 2880 {
        return Some(VideoFormat::new(
            Projection::EQUIRECT_360,
            StereoLayout::TopBottom,
        ));
    }
    None
}

/// Spherical / stereo metadata read from the container (Google Spatial
/// Media, Matroska `StereoMode`, FFmpeg side data).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ContainerHints {
    pub projection: Option<Projection>,
    pub stereo: Option<StereoLayout>,
}

/// Combines every source of evidence. `user` wins, then container metadata,
/// then the file name, then the aspect ratio.
pub fn resolve(
    user: Option<VideoFormat>,
    hints: ContainerHints,
    name: &str,
    width: u32,
    height: u32,
) -> DetectedFormat {
    if let Some(format) = user {
        return DetectedFormat {
            format,
            evidence: Evidence::User,
        };
    }
    let from_name = detect_from_name(name);
    if hints.projection.is_some() || hints.stereo.is_some() {
        let projection = hints
            .projection
            .or(from_name.map(|f| f.projection))
            .unwrap_or(Projection::Flat);
        let stereo = hints
            .stereo
            .or(from_name.map(|f| f.stereo))
            .unwrap_or_default();
        let eyes_swapped = from_name.is_some_and(|f| f.eyes_swapped);
        return DetectedFormat {
            format: VideoFormat {
                projection,
                stereo,
                eyes_swapped,
            },
            evidence: Evidence::Metadata,
        };
    }
    if let Some(format) = from_name {
        return DetectedFormat {
            format,
            evidence: Evidence::FileName,
        };
    }
    if let Some(format) = guess_from_aspect(width, height) {
        return DetectedFormat {
            format,
            evidence: Evidence::AspectRatio,
        };
    }
    DetectedFormat {
        format: VideoFormat::default(),
        evidence: Evidence::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Projection as P;
    use StereoLayout as S;

    fn d(name: &str) -> Option<(P, S, bool)> {
        detect_from_name(name).map(|f| (f.projection, f.stereo, f.eyes_swapped))
    }

    #[test]
    fn deovr_style_names() {
        assert_eq!(
            d("Scene_180_LR.mp4"),
            Some((P::EQUIRECT_180, S::SideBySide, false))
        );
        assert_eq!(
            d("scene-180x180_3dh.mp4"),
            Some((P::EQUIRECT_180, S::SideBySide, false))
        );
        assert_eq!(
            d("Trip_360_TB.mkv"),
            Some((P::EQUIRECT_360, S::TopBottom, false))
        );
        assert_eq!(
            d("Trip_360_mono.mp4"),
            Some((P::EQUIRECT_360, S::Mono, false))
        );
        assert_eq!(d("Trip_360.mp4"), Some((P::EQUIRECT_360, S::Mono, false)));
        assert_eq!(
            d("clip_180.mp4"),
            Some((P::EQUIRECT_180, S::SideBySide, false))
        );
        assert_eq!(
            d("Movie.3D.HSBS.1080p.mkv"),
            Some((P::Flat, S::SideBySide, false))
        );
        assert_eq!(d("Movie_FLAT_OU.mp4"), Some((P::Flat, S::TopBottom, false)));
        assert_eq!(d("Movie.mkv"), None);
        assert_eq!(d("Holiday 2024.mp4"), None);
    }

    #[test]
    fn swapped_eyes() {
        assert_eq!(
            d("x_180_RL.mp4"),
            Some((P::EQUIRECT_180, S::SideBySide, true))
        );
        assert_eq!(
            d("x_360_BT.mp4"),
            Some((P::EQUIRECT_360, S::TopBottom, true))
        );
    }

    #[test]
    fn fisheye_family() {
        assert_eq!(
            d("a_MKX200.mp4"),
            Some((P::fisheye(200.0), S::SideBySide, false))
        );
        assert_eq!(
            d("a_mkx220_LR.mp4"),
            Some((P::fisheye(220.0), S::SideBySide, false))
        );
        assert_eq!(
            d("a_VRCA220.mp4"),
            Some((P::fisheye(220.0), S::SideBySide, false))
        );
        assert_eq!(
            d("a_RF52.mp4"),
            Some((P::fisheye(190.0), S::SideBySide, false))
        );
        assert_eq!(
            d("a_FISHEYE190.mp4"),
            Some((P::fisheye(190.0), S::SideBySide, false))
        );
        assert_eq!(
            d("a_fisheye.mp4"),
            Some((P::fisheye(180.0), S::SideBySide, false))
        );
        assert_eq!(d("a_FISHEYE9999.mp4"), None);
    }

    #[test]
    fn eac() {
        assert_eq!(
            d("yt_EAC.webm"),
            Some((P::Eac { h_fov: 360.0 }, S::Mono, false))
        );
        assert_eq!(
            d("yt_EAC180_LR.webm"),
            Some((P::Eac { h_fov: 180.0 }, S::SideBySide, false))
        );
    }

    #[test]
    fn extension_and_paths_ignored() {
        assert_eq!(d("/media/vr/180/notes.txt"), None);
        assert_eq!(
            d("C:\\vr\\Scene_LR_180.MP4"),
            Some((P::EQUIRECT_180, S::SideBySide, false))
        );
    }

    #[test]
    fn aspect_guess() {
        assert_eq!(guess_from_aspect(5760, 2880).unwrap().stereo, S::SideBySide);
        assert_eq!(
            guess_from_aspect(2048, 1024).unwrap().projection,
            P::EQUIRECT_360
        );
        assert_eq!(guess_from_aspect(4096, 4096).unwrap().stereo, S::TopBottom);
        assert_eq!(guess_from_aspect(1920, 1080), None);
    }

    #[test]
    fn resolve_precedence() {
        let user = VideoFormat::new(P::Flat, S::Mono);
        assert_eq!(
            resolve(Some(user), Default::default(), "a_180_LR.mp4", 0, 0).evidence,
            Evidence::User
        );
        let meta = ContainerHints {
            projection: Some(P::EQUIRECT_360),
            stereo: None,
        };
        let r = resolve(None, meta, "a_TB.mp4", 4096, 4096);
        assert_eq!(
            (r.evidence, r.format.projection, r.format.stereo),
            (Evidence::Metadata, P::EQUIRECT_360, S::TopBottom)
        );
        assert_eq!(
            resolve(None, Default::default(), "a_180_LR.mp4", 0, 0).evidence,
            Evidence::FileName
        );
        assert_eq!(
            resolve(None, Default::default(), "a.mp4", 5760, 2880).evidence,
            Evidence::AspectRatio
        );
        assert_eq!(
            resolve(None, Default::default(), "a.mp4", 1920, 1080).format,
            VideoFormat::default()
        );
    }

    #[test]
    fn serde_roundtrip() {
        let f = VideoFormat::new(P::fisheye(200.0), S::SideBySide);
        let j = serde_json::to_string(&f).unwrap();
        assert_eq!(serde_json::from_str::<VideoFormat>(&j).unwrap(), f);
        assert!(j.contains("\"kind\":\"fisheye\""), "{j}");
    }
}
