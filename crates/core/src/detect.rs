//! Projection / stereo auto-detection from filename tokens.
//!
//! The rules follow the conventions DeoVR, HereSphere and SLR/XBVR scenes
//! use in practice: tokens are matched case-insensitively against the file
//! stem split on any non-alphanumeric character, so `Scene_180_LR.mp4`,
//! `scene.180.sbs.mkv` and `Scene-MKX200.mp4` all work. Container metadata
//! (when present) wins over filename tokens; a per-file user override wins
//! over both. That precedence is applied by the caller via [`merge`].

use crate::projection::{FisheyeLens, Projection, StereoMode};

/// What a filename suggests. Fields are `None` when the name says nothing.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Detected {
    pub projection: Option<Projection>,
    pub stereo: Option<StereoMode>,
    pub swap_eyes: bool,
}

/// Split a path's file stem into lowercase alphanumeric tokens.
fn tokens(name: &str) -> Vec<String> {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = match file.rfind('.') {
        Some(i) if i > 0 => &file[..i],
        _ => file,
    };
    stem.split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| !t.is_empty()).map(|t| t.to_ascii_lowercase()).collect()
}

/// Detect projection and stereo layout from a file name or path.
pub fn from_filename(name: &str) -> Detected {
    let toks = tokens(name);
    let mut d = Detected::default();

    for t in &toks {
        let t = t.as_str();
        // Stereo layout.
        match t {
            "lr" | "sbs" | "hsbs" | "fsbs" | "3dh" | "sidebyside" => d.stereo = d.stereo.or(Some(StereoMode::Sbs)),
            "rl" => {
                d.stereo = d.stereo.or(Some(StereoMode::Sbs));
                d.swap_eyes = true;
            }
            "tb" | "ou" | "hou" | "fou" | "htab" | "tab" | "3dv" | "overunder" | "topbottom" => d.stereo = d.stereo.or(Some(StereoMode::Ou)),
            "bt" => {
                d.stereo = d.stereo.or(Some(StereoMode::Ou));
                d.swap_eyes = true;
            }
            "mono" | "2d" => d.stereo = d.stereo.or(Some(StereoMode::Mono)),
            _ => {}
        }

        // Projection. Specific lens tokens beat generic ones, so only fill if
        // nothing more specific was seen yet, and let lens tokens overwrite.
        let lens = match t {
            "mkx200" => Some(Projection::fisheye(FisheyeLens::Mkx200)),
            "mkx220" => Some(Projection::fisheye(FisheyeLens::Mkx220)),
            "rf52" => Some(Projection::fisheye(FisheyeLens::CanonRf52)),
            "vrca220" => Some(Projection::fisheye_fov(220.0)),
            "fisheye" => Some(Projection::fisheye_fov(180.0)),
            "fisheye190" => Some(Projection::fisheye_fov(190.0)),
            "fisheye200" => Some(Projection::fisheye_fov(200.0)),
            "fisheye220" => Some(Projection::fisheye_fov(220.0)),
            "f180" => Some(Projection::fisheye_fov(180.0)),
            _ => None,
        };
        if let Some(p) = lens {
            let more_specific = matches!(&p, Projection::Fisheye { lens, fov_deg, .. } if *lens != FisheyeLens::Generic || *fov_deg != 180.0);
            if d.projection.as_ref().map_or(true, |cur| !matches!(cur, Projection::Fisheye { .. }) || more_specific) {
                d.projection = Some(p);
            }
            continue;
        }
        let generic = match t {
            "180" | "vr180" | "180x180" | "dome" => Some(Projection::EQUIRECT_180),
            "360" | "vr360" | "360x180" | "mono360" => Some(Projection::EQUIRECT_360),
            "eac" | "eac360" => Some(Projection::Eac),
            "flat" | "screen" => Some(Projection::FLAT_DEFAULT),
            _ => None,
        };
        if let Some(p) = generic {
            if d.projection.is_none() {
                d.projection = Some(p);
            }
        }
    }

    // A stereo token alone ("_LR") on VR-site naming implies 180° equirect,
    // which is what DeoVR assumes too.
    if d.projection.is_none() && matches!(d.stereo, Some(StereoMode::Sbs | StereoMode::Ou)) {
        d.projection = Some(Projection::EQUIRECT_180);
    }
    // "3dh" style tokens without "180"/"360" usually mean a flat 3D movie.
    if toks.iter().any(|t| matches!(t.as_str(), "3dh" | "3dv" | "half" | "hsbs" | "fsbs" | "hou" | "fou" | "htab")) && !toks.iter().any(|t| t.contains("180") || t.contains("360")) {
        d.projection = Some(Projection::FLAT_DEFAULT);
    }
    d
}

/// Resolve final settings. Precedence: user override > container metadata >
/// filename > defaults (flat mono).
pub fn merge(
    user: Option<(Projection, StereoMode, bool)>,
    container: (Option<Projection>, Option<StereoMode>),
    filename: &Detected,
) -> (Projection, StereoMode, bool) {
    if let Some(u) = user {
        return u;
    }
    let projection = container.0.or_else(|| filename.projection.clone()).unwrap_or_default();
    let stereo = container.1.or(filename.stereo).unwrap_or_default();
    (projection, stereo, filename.swap_eyes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(n: &str) -> (Option<Projection>, Option<StereoMode>, bool) {
        let d = from_filename(n);
        (d.projection, d.stereo, d.swap_eyes)
    }

    #[test]
    fn common_vr_names() {
        assert_eq!(det("Scene_180_LR.mp4"), (Some(Projection::EQUIRECT_180), Some(StereoMode::Sbs), false));
        assert_eq!(det("/nas/vr/clip.360.TB.mkv"), (Some(Projection::EQUIRECT_360), Some(StereoMode::Ou), false));
        assert_eq!(det("thing_LR.mp4"), (Some(Projection::EQUIRECT_180), Some(StereoMode::Sbs), false));
        assert_eq!(det("thing_RL_180.mp4"), (Some(Projection::EQUIRECT_180), Some(StereoMode::Sbs), true));
    }

    #[test]
    fn fisheye_names() {
        assert_eq!(det("foo-MKX200-LR.mp4").0, Some(Projection::fisheye(FisheyeLens::Mkx200)));
        assert_eq!(det("foo_FISHEYE190_sbs.mp4").0, Some(Projection::fisheye_fov(190.0)));
        assert_eq!(det("foo_180_FISHEYE190_sbs.mp4").0, Some(Projection::fisheye_fov(190.0)));
        assert_eq!(det("foo_fisheye_mkx220.mp4").0, Some(Projection::fisheye(FisheyeLens::Mkx220)));
        assert_eq!(det("foo_RF52_LR.mp4").0, Some(Projection::fisheye(FisheyeLens::CanonRf52)));
    }

    #[test]
    fn flat_3d_movie() {
        assert_eq!(det("Movie.2019.3D.HSBS.3DH.mkv"), (Some(Projection::FLAT_DEFAULT), Some(StereoMode::Sbs), false));
        assert_eq!(det("Movie (2010) Half-OU.mkv"), (Some(Projection::FLAT_DEFAULT), Some(StereoMode::Ou), false));
    }

    #[test]
    fn plain_video() {
        assert_eq!(det("holiday.mp4"), (None, None, false));
        // Extension and directory names never count.
        assert_eq!(det("/videos/360/holiday.mp4"), (None, None, false));
    }

    #[test]
    fn merge_precedence() {
        let fname = from_filename("x_180_LR.mp4");
        let (p, s, _) = merge(None, (Some(Projection::EQUIRECT_360), None), &fname);
        assert_eq!(p, Projection::EQUIRECT_360);
        assert_eq!(s, StereoMode::Sbs);
        let (p, s, sw) = merge(Some((Projection::FLAT_DEFAULT, StereoMode::Mono, true)), (None, None), &fname);
        assert_eq!((p, s, sw), (Projection::FLAT_DEFAULT, StereoMode::Mono, true));
        let (p, s, _) = merge(None, (None, None), &from_filename("a.mp4"));
        assert_eq!((p, s), (Projection::FLAT_DEFAULT, StereoMode::Mono));
    }
}
