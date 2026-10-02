//! What is in a media file: streams, duration, chapters, and the format hints
//! (spherical projection, stereo packing, HDR) that drive the renderer.

use crate::input::Input;
use crate::q2d;
use fp_core::format::{ContainerHints, Projection, StereoLayout};
use fp_ffmpeg_sys as ff;
use std::ffi::{CStr, CString};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Video,
    Audio,
    Subtitle,
    Other,
}

/// HDR transfer function of a video stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Transfer {
    #[default]
    Sdr,
    Pq,
    Hlg,
}

#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub index: usize,
    pub kind: StreamKind,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    // video
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub bit_depth: u32,
    pub transfer: Transfer,
    /// Display rotation in degrees (from a display matrix), 0 when none.
    pub rotation: f64,
    // audio
    pub sample_rate: u32,
    pub channels: u32,
    /// First-order (or higher) ambisonic audio (AmbiX / ACN-SN3D).
    pub ambisonic: bool,
}

#[derive(Clone, Debug)]
pub struct Chapter {
    pub start: f64,
    pub end: f64,
    pub title: String,
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub name: String,
    pub container: String,
    pub duration: f64,
    pub streams: Vec<StreamInfo>,
    pub chapters: Vec<Chapter>,
    pub hints: ContainerHints,
    pub video: Option<usize>,
    pub audio: Option<usize>,
    pub title: Option<String>,
}

impl MediaInfo {
    pub fn video_stream(&self) -> Option<&StreamInfo> {
        self.video
            .and_then(|i| self.streams.iter().find(|s| s.index == i))
    }
    pub fn audio_streams(&self) -> impl Iterator<Item = &StreamInfo> {
        self.streams.iter().filter(|s| s.kind == StreamKind::Audio)
    }
    pub fn subtitle_streams(&self) -> impl Iterator<Item = &StreamInfo> {
        self.streams
            .iter()
            .filter(|s| s.kind == StreamKind::Subtitle)
    }
}

pub(crate) fn dict_get(dict: *mut ff::AVDictionary, key: &str) -> Option<String> {
    let k = CString::new(key).ok()?;
    // SAFETY: av_dict_get tolerates a null dict; returned entry is owned by it.
    unsafe {
        let e = ff::av_dict_get(dict, k.as_ptr(), std::ptr::null(), 0);
        if e.is_null() || (*e).value.is_null() {
            None
        } else {
            Some(CStr::from_ptr((*e).value).to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
        }
    }
}

fn side_data(par: &ff::AVCodecParameters, kind: ff::AVPacketSideDataType) -> Option<&[u8]> {
    if par.coded_side_data.is_null() {
        return None;
    }
    // SAFETY: coded_side_data holds nb_coded_side_data entries.
    let all =
        unsafe { std::slice::from_raw_parts(par.coded_side_data, par.nb_coded_side_data as usize) };
    all.iter()
        .find(|sd| sd.type_ == kind && !sd.data.is_null())
        // SAFETY: data is valid for size bytes while the stream lives.
        .map(|sd| unsafe { std::slice::from_raw_parts(sd.data, sd.size) })
}

/// Maps FFmpeg spherical side data to a projection.
fn spherical(par: &ff::AVCodecParameters) -> Option<Projection> {
    let bytes = side_data(par, ff::AV_PKT_DATA_SPHERICAL)?;
    if bytes.len() < std::mem::size_of::<ff::AVSphericalMapping>() {
        return None;
    }
    // SAFETY: size checked; FFmpeg stores an AVSphericalMapping here.
    let m = unsafe { &*(bytes.as_ptr() as *const ff::AVSphericalMapping) };
    match m.projection {
        ff::AV_SPHERICAL_EQUIRECTANGULAR => Some(Projection::EQUIRECT_360),
        ff::AV_SPHERICAL_HALF_EQUIRECTANGULAR => Some(Projection::EQUIRECT_180),
        ff::AV_SPHERICAL_EQUIRECTANGULAR_TILE => {
            // Bounds are 0.32 fixed-point fractions cropped from each side.
            let crop = (m.bound_left as f64 + m.bound_right as f64) / 4_294_967_296.0;
            let h_fov = (360.0 * (1.0 - crop)).round() as f32;
            Some(Projection::Equirect {
                h_fov: h_fov.clamp(90.0, 360.0),
                v_fov: 180.0,
            })
        }
        ff::AV_SPHERICAL_CUBEMAP => Some(Projection::Eac { h_fov: 360.0 }),
        ff::AV_SPHERICAL_FISHEYE => Some(Projection::fisheye(180.0)),
        ff::AV_SPHERICAL_RECTILINEAR => Some(Projection::Flat),
        _ => None,
    }
}

fn stereo3d(par: &ff::AVCodecParameters, metadata: *mut ff::AVDictionary) -> Option<StereoLayout> {
    if let Some(bytes) = side_data(par, ff::AV_PKT_DATA_STEREO3D) {
        if bytes.len() >= std::mem::size_of::<ff::AVStereo3D>() {
            // SAFETY: size checked; FFmpeg stores an AVStereo3D here.
            let s = unsafe { &*(bytes.as_ptr() as *const ff::AVStereo3D) };
            match s.type_ {
                ff::AV_STEREO3D_SIDEBYSIDE | ff::AV_STEREO3D_SIDEBYSIDE_QUINCUNX => {
                    return Some(StereoLayout::SideBySide);
                }
                ff::AV_STEREO3D_TOPBOTTOM => return Some(StereoLayout::TopBottom),
                ff::AV_STEREO3D_2D => return Some(StereoLayout::Mono),
                _ => {}
            }
        }
    }
    // Matroska StereoMode as exported in stream metadata.
    match dict_get(metadata, "stereo_mode")?.as_str() {
        "left_right" | "right_left" => Some(StereoLayout::SideBySide),
        "top_bottom" | "bottom_top" => Some(StereoLayout::TopBottom),
        "mono" => Some(StereoLayout::Mono),
        _ => None,
    }
}

fn rotation(par: &ff::AVCodecParameters) -> f64 {
    match side_data(par, ff::AV_PKT_DATA_DISPLAYMATRIX) {
        Some(b) if b.len() >= 36 => {
            // SAFETY: a display matrix is 9 int32 values.
            let r = unsafe { ff::av_display_rotation_get(b.as_ptr() as *const i32) };
            if r.is_nan() { 0.0 } else { -r }
        }
        _ => 0.0,
    }
}

pub fn read_info(input: &Input) -> MediaInfo {
    // SAFETY: input.fmt is open for the duration of this call.
    let fmt = unsafe { &*input.fmt };
    let container = if fmt.iformat.is_null() {
        String::new()
    } else {
        // SAFETY: iformat->name is a static string.
        unsafe {
            CStr::from_ptr((*fmt.iformat).name)
                .to_string_lossy()
                .into_owned()
        }
    };
    let duration = if fmt.duration > 0 {
        fmt.duration as f64 / ff::AV_TIME_BASE as f64
    } else {
        0.0
    };
    let mut hints = ContainerHints::default();
    let mut streams = Vec::new();
    for (index, &st) in input.streams().iter().enumerate() {
        // SAFETY: stream pointers are valid while the input is open.
        let st = unsafe { &*st };
        let par = unsafe { &*st.codecpar };
        // SAFETY: avcodec_get_name returns a static string.
        let codec = unsafe {
            CStr::from_ptr(ff::avcodec_get_name(par.codec_id))
                .to_string_lossy()
                .into_owned()
        };
        let kind = match par.codec_type {
            ff::AVMEDIA_TYPE_VIDEO
                if st.disposition & ff::AV_DISPOSITION_ATTACHED_PIC as i32 == 0 =>
            {
                StreamKind::Video
            }
            ff::AVMEDIA_TYPE_AUDIO => StreamKind::Audio,
            ff::AVMEDIA_TYPE_SUBTITLE => StreamKind::Subtitle,
            _ => StreamKind::Other,
        };
        let mut s = StreamInfo {
            index,
            kind,
            codec,
            language: dict_get(st.metadata, "language"),
            title: dict_get(st.metadata, "title"),
            default: st.disposition & ff::AV_DISPOSITION_DEFAULT as i32 != 0,
            width: 0,
            height: 0,
            fps: 0.0,
            bit_depth: 8,
            transfer: Transfer::Sdr,
            rotation: 0.0,
            sample_rate: 0,
            channels: 0,
            ambisonic: false,
        };
        match kind {
            StreamKind::Video => {
                s.width = par.width.max(0) as u32;
                s.height = par.height.max(0) as u32;
                s.fps = q2d(st.avg_frame_rate).max(q2d(st.r_frame_rate).min(240.0));
                // SAFETY: pixdesc lookup on a format value; null when unknown.
                let desc = unsafe { ff::av_pix_fmt_desc_get(par.format) };
                if !desc.is_null() {
                    s.bit_depth = unsafe { (*desc).comp[0].depth } as u32;
                }
                s.transfer = match par.color_trc {
                    ff::AVCOL_TRC_SMPTE2084 => Transfer::Pq,
                    ff::AVCOL_TRC_ARIB_STD_B67 => Transfer::Hlg,
                    _ => Transfer::Sdr,
                };
                s.rotation = rotation(par);
                if hints.projection.is_none() {
                    hints.projection = spherical(par);
                }
                if hints.stereo.is_none() {
                    hints.stereo = stereo3d(par, st.metadata);
                }
            }
            StreamKind::Audio => {
                s.sample_rate = par.sample_rate.max(0) as u32;
                s.channels = par.ch_layout.nb_channels.max(0) as u32;
                s.ambisonic = par.ch_layout.order == ff::AV_CHANNEL_ORDER_AMBISONIC
                    || s.title
                        .as_deref()
                        .is_some_and(|t| t.to_lowercase().contains("ambisonic"));
            }
            _ => {}
        }
        streams.push(s);
    }
    let chapters = if fmt.chapters.is_null() {
        Vec::new()
    } else {
        // SAFETY: chapters has nb_chapters valid pointers.
        unsafe { std::slice::from_raw_parts(fmt.chapters, fmt.nb_chapters as usize) }
            .iter()
            .map(|&c| {
                // SAFETY: chapter pointers are valid while open.
                let c = unsafe { &*c };
                let tb = q2d(c.time_base);
                Chapter {
                    start: c.start as f64 * tb,
                    end: c.end as f64 * tb,
                    title: dict_get(c.metadata, "title").unwrap_or_default(),
                }
            })
            .collect()
    };
    MediaInfo {
        name: input.name.clone(),
        container,
        duration,
        video: input.best_stream(ff::AVMEDIA_TYPE_VIDEO).filter(|i| {
            streams
                .get(*i)
                .is_some_and(|s: &StreamInfo| s.kind == StreamKind::Video)
        }),
        audio: input.best_stream(ff::AVMEDIA_TYPE_AUDIO),
        title: dict_get(fmt.metadata, "title"),
        streams,
        chapters,
        hints,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::source::FileSource;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    pub(crate) fn open(name: &str) -> Input {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name);
        let src = Arc::new(FileSource::open(&path).expect("fixture"));
        Input::open(src, name, Arc::new(AtomicBool::new(false))).expect("open")
    }

    #[test]
    fn h264_mp4() {
        let info = read_info(&open("h264_aac_180_LR.mp4"));
        assert_eq!(info.container, "mov,mp4,m4a,3gp,3g2,mj2");
        assert!((info.duration - 2.0).abs() < 0.1, "{}", info.duration);
        let v = info.video_stream().unwrap();
        assert_eq!(
            (v.codec.as_str(), v.width, v.height, v.bit_depth),
            ("h264", 640, 320, 8)
        );
        assert!((v.fps - 30.0).abs() < 0.01);
        let a = info.audio_streams().next().unwrap();
        assert_eq!(
            (a.codec.as_str(), a.sample_rate, a.channels, a.ambisonic),
            ("aac", 48000, 2, false)
        );
    }

    #[test]
    fn hevc10_mkv_stereo_metadata_and_subtitles() {
        let info = read_info(&open("hevc10_tb.mkv"));
        let v = info.video_stream().unwrap();
        assert_eq!((v.codec.as_str(), v.bit_depth), ("hevc", 10));
        assert_eq!(info.hints.stereo, Some(StereoLayout::TopBottom));
        let s = info.subtitle_streams().next().unwrap();
        assert_eq!(s.codec, "subrip");
        assert_eq!(s.language.as_deref(), Some("eng"));
        assert!(info.audio.is_none());
    }

    #[test]
    fn webm_av1_and_vp9() {
        let a = read_info(&open("av1_opus.webm"));
        assert_eq!(a.video_stream().unwrap().codec, "av1");
        assert_eq!(a.audio_streams().next().unwrap().codec, "opus");
        let b = read_info(&open("vp9.webm"));
        assert_eq!(b.video_stream().unwrap().codec, "vp9");
    }

    #[test]
    fn garbage_fails_cleanly() {
        let src = Arc::new(fp_core::source::MemorySource(vec![0u8; 4096]));
        assert!(Input::open(src, "junk.bin", Arc::new(AtomicBool::new(false))).is_err());
    }
}
