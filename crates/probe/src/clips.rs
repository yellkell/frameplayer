//! Tiny test clips embedded in the binary (see `assets/README.md` for how
//! they were made). All are MP4 so fp-video's pure-Rust demuxer reads them.

use fp_video::CodecId;

/// One embedded clip.
#[derive(Debug, Clone, Copy)]
pub struct Clip {
    pub name: &'static str,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub frames: u32,
    pub bytes: &'static [u8],
}

/// Codecs we test (kept separate from `CodecId` so it is `Copy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    H264,
    Vp9,
    Av1,
}

impl Codec {
    pub fn id(self) -> CodecId {
        match self {
            Codec::Hevc => CodecId::Hevc,
            Codec::H264 => CodecId::H264,
            Codec::Vp9 => CodecId::Vp9,
            Codec::Av1 => CodecId::Av1,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Codec::Hevc => "HEVC",
            Codec::H264 => "H.264",
            Codec::Vp9 => "VP9",
            Codec::Av1 => "AV1",
        }
    }
}

pub const CLIPS: &[Clip] = &[
    Clip {
        name: "hevc_256x256",
        codec: Codec::Hevc,
        width: 256,
        height: 256,
        bit_depth: 8,
        frames: 16,
        bytes: include_bytes!("../assets/hevc_256x256.mp4"),
    },
    Clip {
        name: "h264_256x256",
        codec: Codec::H264,
        width: 256,
        height: 256,
        bit_depth: 8,
        frames: 16,
        bytes: include_bytes!("../assets/h264_256x256.mp4"),
    },
    Clip {
        name: "vp9_256x256",
        codec: Codec::Vp9,
        width: 256,
        height: 256,
        bit_depth: 8,
        frames: 16,
        bytes: include_bytes!("../assets/vp9_256x256.mp4"),
    },
    Clip {
        name: "av1_256x256",
        codec: Codec::Av1,
        width: 256,
        height: 256,
        bit_depth: 8,
        frames: 16,
        bytes: include_bytes!("../assets/av1_256x256.mp4"),
    },
    Clip {
        name: "hevc10_3840x1920",
        codec: Codec::Hevc,
        width: 3840,
        height: 1920,
        bit_depth: 10,
        frames: 4,
        bytes: include_bytes!("../assets/hevc10_3840x1920.mp4"),
    },
    Clip {
        name: "hevc10_7680x3840",
        codec: Codec::Hevc,
        width: 7680,
        height: 3840,
        bit_depth: 10,
        frames: 2,
        bytes: include_bytes!("../assets/hevc10_7680x3840.mp4"),
    },
];

pub fn by_name(name: &str) -> Option<&'static Clip> {
    CLIPS.iter().find(|c| c.name == name)
}

/// Demuxer over an embedded clip.
pub fn open(clip: &Clip) -> fp_video::Result<Box<dyn fp_video::Demuxer>> {
    fp_video::open_demuxer(Box::new(std::io::Cursor::new(clip.bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_video::TrackKind;

    #[test]
    fn every_clip_demuxes_with_fp_video() {
        for clip in CLIPS {
            let mut d = open(clip).unwrap_or_else(|e| panic!("{}: {e}", clip.name));
            let t = d
                .default_track(TrackKind::Video)
                .expect("video track")
                .clone();
            assert_eq!(t.codec, clip.codec.id(), "{}", clip.name);
            let v = t.video.clone().unwrap();
            assert_eq!(
                (v.width, v.height),
                (clip.width, clip.height),
                "{}",
                clip.name
            );
            assert!(
                !t.codec_private.is_empty(),
                "{} has no codec config",
                clip.name
            );
            let req = fp_video::DecoderRequest::from_track(&t);
            assert_eq!(req.bit_depth, clip.bit_depth, "{}", clip.name);
            let mut n = 0;
            let mut first_key = None;
            while let Some(p) = d.read_packet().unwrap() {
                if p.track == t.id {
                    first_key.get_or_insert(p.keyframe);
                    n += 1;
                }
            }
            assert_eq!(n, clip.frames, "{}", clip.name);
            assert_eq!(first_key, Some(true), "{}", clip.name);
        }
        assert!(by_name("av1_256x256").is_some());
        let total: usize = CLIPS.iter().map(|c| c.bytes.len()).sum();
        assert!(total < 600 << 10, "embedded clips grew to {total} bytes");
    }
}
