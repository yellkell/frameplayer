//! Spherical-video metadata → [`Projection`] / [`StereoMode`].
//!
//! Supported signalling:
//! * Google spatial-media **v2** (ISO-BMFF): `st3d` (stereo mode) and
//!   `sv3d` → `proj` → `prhd` + `equi` / `cbmp` (projection).
//! * Google spatial-media **v1**: a `uuid` box
//!   `ffcc8263-f855-4a93-8814-587a02521fdd` holding GSpherical RDF/XML.
//! * Matroska: `Video/StereoMode` and `Video/Projection/ProjectionType`
//!   (+ `ProjectionPrivate`, which carries an `equi`/`cbmp` payload).

use crate::bytes::{find_box, Bytes};
use fp_core::{Projection, StereoMode};

/// v1 spherical metadata UUID.
pub const SPHERICAL_V1_UUID: [u8; 16] = [
    0xff, 0xcc, 0x82, 0x63, 0xf8, 0x55, 0x4a, 0x93, 0x88, 0x14, 0x58, 0x7a, 0x02, 0x52, 0x1f, 0xdd,
];

/// Projection/stereo found in container metadata.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SphericalInfo {
    pub projection: Option<Projection>,
    pub stereo: Option<StereoMode>,
    /// Pose from `prhd` / Matroska ProjectionPose*, degrees.
    pub yaw_deg: f32,
    pub pitch_deg: f32,
    pub roll_deg: f32,
}

impl SphericalInfo {
    /// Fill unset fields from `other`.
    pub fn merge(&mut self, other: SphericalInfo) {
        if self.projection.is_none() {
            self.projection = other.projection;
            self.yaw_deg = other.yaw_deg;
            self.pitch_deg = other.pitch_deg;
            self.roll_deg = other.roll_deg;
        }
        if self.stereo.is_none() {
            self.stereo = other.stereo;
        }
    }
}

/// `st3d` payload (FullBox header included).
pub fn parse_st3d(payload: &[u8]) -> Option<StereoMode> {
    match *payload.get(4)? {
        0 => Some(StereoMode::Mono),
        1 => Some(StereoMode::Ou),
        2 => Some(StereoMode::Sbs),
        _ => None, // 3 = stereo-custom (per-eye mesh), unsupported
    }
}

/// `equi` payload (FullBox header included) → horizontal FOV.
pub fn parse_equi(payload: &[u8]) -> Option<Projection> {
    let mut b = Bytes::new(payload);
    b.skip(4).ok()?;
    let _top = b.u32().ok()?;
    let _bottom = b.u32().ok()?;
    let left = b.u32().ok()? as f64 / 4_294_967_296.0;
    let right = b.u32().ok()? as f64 / 4_294_967_296.0;
    let fov = (360.0 * (1.0 - left - right)).clamp(1.0, 360.0);
    // Snap the common values.
    let fov = if (fov - 180.0).abs() < 1.0 {
        180.0
    } else if fov > 359.0 {
        360.0
    } else {
        fov
    };
    Some(Projection::Equirect {
        h_fov_deg: fov as f32,
    })
}

/// `sv3d` payload → projection and pose.
pub fn parse_sv3d(payload: &[u8]) -> SphericalInfo {
    let mut info = SphericalInfo::default();
    let Some(proj) = find_box(payload, b"proj") else {
        return info;
    };
    if let Some(prhd) = find_box(proj, b"prhd") {
        let mut b = Bytes::new(prhd);
        if b.skip(4).is_ok() {
            let fx = |v: i32| v as f32 / 65536.0;
            info.yaw_deg = b.i32().map(fx).unwrap_or(0.0);
            info.pitch_deg = b.i32().map(fx).unwrap_or(0.0);
            info.roll_deg = b.i32().map(fx).unwrap_or(0.0);
        }
    }
    if let Some(equi) = find_box(proj, b"equi") {
        info.projection = parse_equi(equi);
    } else if find_box(proj, b"cbmp").is_some() {
        // Cubemap: YouTube's equi-angular cubemap is the only cubemap
        // flavour we render.
        info.projection = Some(Projection::Eac);
    }
    info
}

fn xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    // Match `<GSpherical:Tag>value</GSpherical:Tag>` with any namespace prefix.
    let lower = xml;
    let mut search = 0;
    while let Some(i) = lower[search..].find('<') {
        let start = search + i + 1;
        let end = lower[start..].find('>')? + start;
        let name = &lower[start..end];
        let local = name.rsplit(':').next().unwrap_or(name);
        if local.eq_ignore_ascii_case(tag) && !name.starts_with('/') {
            let close = lower[end + 1..].find('<')? + end + 1;
            return Some(lower[end + 1..close].trim());
        }
        search = end;
    }
    None
}

/// GSpherical v1 RDF/XML.
pub fn parse_spherical_v1_xml(xml: &str) -> SphericalInfo {
    let mut info = SphericalInfo::default();
    let spherical = xml_tag(xml, "Spherical")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let proj = xml_tag(xml, "ProjectionType").unwrap_or("");
    if spherical && proj.eq_ignore_ascii_case("equirectangular") {
        let num = |t: &str| xml_tag(xml, t).and_then(|v| v.parse::<f64>().ok());
        let fov = match (
            num("CroppedAreaImageWidthPixels"),
            num("FullPanoWidthPixels"),
        ) {
            (Some(c), Some(f)) if f > 0.0 => (360.0 * c / f).clamp(1.0, 360.0),
            _ => 360.0,
        };
        let fov = if (fov - 180.0).abs() < 1.0 {
            180.0
        } else {
            fov
        };
        info.projection = Some(Projection::Equirect {
            h_fov_deg: fov as f32,
        });
        info.yaw_deg = num("InitialViewHeadingDegrees").unwrap_or(0.0) as f32;
        info.pitch_deg = num("InitialViewPitchDegrees").unwrap_or(0.0) as f32;
        info.roll_deg = num("InitialViewRollDegrees").unwrap_or(0.0) as f32;
    }
    info.stereo = match xml_tag(xml, "StereoMode").map(|s| s.to_ascii_lowercase()) {
        Some(s) if s == "left-right" => Some(StereoMode::Sbs),
        Some(s) if s == "top-bottom" => Some(StereoMode::Ou),
        Some(s) if s == "mono" => Some(StereoMode::Mono),
        _ => None,
    };
    info
}

/// Matroska `StereoMode` element value. Right-eye-first variants map to the
/// same packing (fp-core has no per-track swap flag; see crate docs).
pub fn mkv_stereo_mode(v: u64) -> Option<StereoMode> {
    match v {
        0 => Some(StereoMode::Mono),
        1 | 11 => Some(StereoMode::Sbs),
        2 | 3 => Some(StereoMode::Ou),
        _ => None,
    }
}

/// True for Matroska stereo modes that store the right eye first.
pub fn mkv_stereo_right_first(v: u64) -> bool {
    matches!(v, 2 | 11)
}

/// Matroska `ProjectionType` + `ProjectionPrivate`.
pub fn mkv_projection(projection_type: u64, private: &[u8]) -> Option<Projection> {
    match projection_type {
        1 => {
            if private.len() >= 20 {
                parse_equi(private)
            } else {
                Some(Projection::EQUIRECT_360)
            }
        }
        2 => Some(Projection::Eac),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mkbox(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    pub fn equi_payload(left: f64, right: f64) -> Vec<u8> {
        let mut p = vec![0, 0, 0, 0];
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&((left * 4_294_967_296.0) as u32).to_be_bytes());
        p.extend_from_slice(&((right * 4_294_967_296.0) as u32).to_be_bytes());
        p
    }

    #[test]
    fn st3d_modes() {
        assert_eq!(parse_st3d(&[0, 0, 0, 0, 2]), Some(StereoMode::Sbs));
        assert_eq!(parse_st3d(&[0, 0, 0, 0, 1]), Some(StereoMode::Ou));
        assert_eq!(parse_st3d(&[0, 0, 0, 0, 0]), Some(StereoMode::Mono));
        assert_eq!(parse_st3d(&[0, 0, 0, 0, 3]), None);
    }

    #[test]
    fn sv3d_equirect_180() {
        let mut prhd = vec![0, 0, 0, 0];
        prhd.extend_from_slice(&(90i32 * 65536).to_be_bytes());
        prhd.extend_from_slice(&0i32.to_be_bytes());
        prhd.extend_from_slice(&0i32.to_be_bytes());
        let proj = [
            mkbox(b"prhd", &prhd),
            mkbox(b"equi", &equi_payload(0.25, 0.25)),
        ]
        .concat();
        let sv3d = [
            mkbox(b"svhd", &[0, 0, 0, 0, b'x', 0]),
            mkbox(b"proj", &proj),
        ]
        .concat();
        let info = parse_sv3d(&sv3d);
        assert_eq!(info.projection, Some(Projection::EQUIRECT_180));
        assert_eq!(info.yaw_deg, 90.0);
        let proj = mkbox(b"equi", &equi_payload(0.0, 0.0));
        assert_eq!(
            parse_sv3d(&mkbox(b"proj", &proj)).projection,
            Some(Projection::EQUIRECT_360)
        );
        let proj = mkbox(b"cbmp", &[0; 12]);
        assert_eq!(
            parse_sv3d(&mkbox(b"proj", &proj)).projection,
            Some(Projection::Eac)
        );
    }

    #[test]
    fn v1_xml() {
        let xml = r#"<?xml version="1.0"?><rdf:SphericalVideo xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:GSpherical="http://ns.google.com/videos/1.0/spherical/">
<GSpherical:Spherical>true</GSpherical:Spherical><GSpherical:Stitched>true</GSpherical:Stitched>
<GSpherical:StitchingSoftware>Spherical Metadata Tool</GSpherical:StitchingSoftware>
<GSpherical:ProjectionType>equirectangular</GSpherical:ProjectionType>
<GSpherical:StereoMode>left-right</GSpherical:StereoMode>
<GSpherical:CroppedAreaImageWidthPixels>2048</GSpherical:CroppedAreaImageWidthPixels>
<GSpherical:FullPanoWidthPixels>4096</GSpherical:FullPanoWidthPixels></rdf:SphericalVideo>"#;
        let info = parse_spherical_v1_xml(xml);
        assert_eq!(info.projection, Some(Projection::EQUIRECT_180));
        assert_eq!(info.stereo, Some(StereoMode::Sbs));
        let info =
            parse_spherical_v1_xml("<x><GSpherical:Spherical>false</GSpherical:Spherical></x>");
        assert_eq!(info.projection, None);
    }

    #[test]
    fn matroska_elements() {
        assert_eq!(mkv_stereo_mode(1), Some(StereoMode::Sbs));
        assert_eq!(mkv_stereo_mode(3), Some(StereoMode::Ou));
        assert!(mkv_stereo_right_first(11));
        assert_eq!(mkv_projection(1, &[]), Some(Projection::EQUIRECT_360));
        assert_eq!(
            mkv_projection(1, &equi_payload(0.25, 0.25)),
            Some(Projection::EQUIRECT_180)
        );
        assert_eq!(mkv_projection(0, &[]), None);
    }
}
