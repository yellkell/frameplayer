//! Codec configuration records and bitstream helpers.
//!
//! * [`annexb`]: `avcC`/`hvcC` parsing and length-prefixed → Annex-B
//!   conversion (needed for V4L2 stateful decoders).
//! * [`Av1Config`] / [`Vp9Config`]: `av1C` / `vpcC` records (bit depth and
//!   profile for decoder selection) and AV1 OBU helpers.
//! * [`BitstreamFilter`]: per-track conversion of container packets into
//!   what a hardware decoder consumes.

pub mod annexb;

use crate::bytes::Bytes;
use crate::error::{Result, VideoError};
use crate::packet::{CodecId, TrackDesc};
use annexb::{AnnexBConverter, AvcConfig, HevcConfig};

/// Parsed `av1C` (AV1CodecConfigurationRecord).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Av1Config {
    pub seq_profile: u8,
    pub seq_level_idx: u8,
    pub tier: u8,
    pub bit_depth: u8,
    pub monochrome: bool,
    pub chroma_subsampling_x: bool,
    pub chroma_subsampling_y: bool,
    /// Configuration OBUs (normally the sequence header), low-overhead format.
    pub config_obus: Vec<u8>,
}

impl Av1Config {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut b = Bytes::new(data);
        let m = b.u8()?;
        if m & 0x80 == 0 {
            return Err(VideoError::invalid("av1C marker bit not set"));
        }
        let p = b.u8()?;
        let f = b.u8()?;
        let _delay = b.u8()?;
        let high = f & 0x40 != 0;
        let twelve = f & 0x20 != 0;
        Ok(Av1Config {
            seq_profile: p >> 5,
            seq_level_idx: p & 0x1f,
            tier: f >> 7,
            bit_depth: if twelve {
                12
            } else if high {
                10
            } else {
                8
            },
            monochrome: f & 0x10 != 0,
            chroma_subsampling_x: f & 0x08 != 0,
            chroma_subsampling_y: f & 0x04 != 0,
            config_obus: b.rest().to_vec(),
        })
    }
}

/// Parsed `vpcC` (VPCodecConfigurationRecord, version 1). Accepts the box
/// payload with or without its 4-byte FullBox header.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Vp9Config {
    pub profile: u8,
    pub level: u8,
    pub bit_depth: u8,
    pub chroma_subsampling: u8,
    pub full_range: bool,
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
}

impl Vp9Config {
    pub fn parse(data: &[u8]) -> Result<Self> {
        // FullBox header present when the payload is 12+ bytes and starts with version 1.
        let body = if data.len() >= 12 && data[0] == 1 && data[1..4] == [0, 0, 0] {
            &data[4..]
        } else {
            data
        };
        let mut b = Bytes::new(body);
        let profile = b.u8()?;
        let level = b.u8()?;
        let x = b.u8()?;
        Ok(Vp9Config {
            profile,
            level,
            bit_depth: x >> 4,
            chroma_subsampling: (x >> 1) & 7,
            full_range: x & 1 != 0,
            colour_primaries: b.u8()?,
            transfer_characteristics: b.u8()?,
            matrix_coefficients: b.u8()?,
        })
    }
}

/// Read an AV1 `leb128` value; returns `(value, bytes consumed)`.
pub fn read_leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &b) in data.iter().enumerate().take(8) {
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// OBU types present in a low-overhead AV1 temporal unit.
pub fn av1_obu_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut p = 0;
    while p < data.len() {
        let h = data[p];
        let t = (h >> 3) & 0xf;
        let ext = h & 0x04 != 0;
        let has_size = h & 0x02 != 0;
        let mut q = p + 1 + ext as usize;
        types.push(t);
        if !has_size {
            break; // the OBU extends to the end
        }
        let Some((size, n)) = read_leb128(data.get(q..).unwrap_or(&[])) else {
            break;
        };
        q += n;
        p = q + size as usize;
    }
    types
}

pub const OBU_SEQUENCE_HEADER: u8 = 1;

/// Converts container packets into the bitstream a hardware decoder takes.
#[derive(Debug, Clone)]
pub enum BitstreamFilter {
    /// Length-prefixed H.264/HEVC → Annex-B with in-band parameter sets.
    AnnexB(AnnexBConverter),
    /// AV1: prepend the `av1C` sequence header to keyframes lacking one.
    Av1 { config_obus: Vec<u8> },
    /// VP8/VP9 frames (and anything else) pass through unchanged.
    Passthrough,
}

impl BitstreamFilter {
    pub fn for_track(track: &TrackDesc) -> Result<Self> {
        let cp = &track.codec_private;
        Ok(match track.codec {
            // Annex-B extradata (MPEG-TS via ffmpeg) means parameter sets are in band.
            CodecId::H264 if !cp.is_empty() && !annexb::is_annexb(cp) => {
                BitstreamFilter::AnnexB(AnnexBConverter::from_avcc(cp)?)
            }
            CodecId::Hevc if !cp.is_empty() && !annexb::is_annexb(cp) => {
                BitstreamFilter::AnnexB(AnnexBConverter::from_hvcc(cp)?)
            }
            CodecId::Av1 if cp.len() > 4 => BitstreamFilter::Av1 {
                config_obus: Av1Config::parse(cp)
                    .map(|c| c.config_obus)
                    .unwrap_or_default(),
            },
            _ => BitstreamFilter::Passthrough,
        })
    }

    /// Filter one packet's payload into `out`.
    pub fn apply(&self, data: &[u8], keyframe: bool, out: &mut Vec<u8>) -> Result<()> {
        match self {
            BitstreamFilter::AnnexB(c) => c.convert(data, keyframe, out),
            BitstreamFilter::Av1 { config_obus } => {
                out.clear();
                if keyframe
                    && !config_obus.is_empty()
                    && !av1_obu_types(data).contains(&OBU_SEQUENCE_HEADER)
                {
                    out.extend_from_slice(config_obus);
                }
                out.extend_from_slice(data);
                Ok(())
            }
            BitstreamFilter::Passthrough => {
                out.clear();
                out.extend_from_slice(data);
                Ok(())
            }
        }
    }
}

/// Best-effort bit depth from a track's codec configuration.
pub fn bit_depth_from_config(codec: &CodecId, codec_private: &[u8]) -> Option<u8> {
    match codec {
        CodecId::Hevc => HevcConfig::parse(codec_private)
            .ok()
            .map(|c| c.bit_depth_luma),
        CodecId::H264 => AvcConfig::parse(codec_private)
            .ok()
            .map(|c| c.bit_depth_luma),
        CodecId::Av1 => Av1Config::parse(codec_private).ok().map(|c| c.bit_depth),
        CodecId::Vp9 => Vp9Config::parse(codec_private)
            .ok()
            .map(|c| c.bit_depth)
            .filter(|&d| d >= 8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av1c_parse() {
        // marker|version=1, profile 0 level 8, tier 0 high_bitdepth=1, 4:2:0 (x=1,y=1).
        let rec = [0x81, 0x08, 0x4c, 0x00, 0x0a, 0x0b, 0x00, 0x00];
        let c = Av1Config::parse(&rec).unwrap();
        assert_eq!(c.seq_level_idx, 8);
        assert_eq!(c.bit_depth, 10);
        assert!(c.chroma_subsampling_x && c.chroma_subsampling_y);
        assert_eq!(c.config_obus, vec![0x0a, 0x0b, 0x00, 0x00]);
    }

    #[test]
    fn vpcc_parse_with_fullbox_header() {
        let rec = [1, 0, 0, 0, 2, 31, 0xa2, 9, 16, 9, 0, 0];
        let c = Vp9Config::parse(&rec).unwrap();
        assert_eq!(
            (c.profile, c.level, c.bit_depth, c.chroma_subsampling),
            (2, 31, 10, 1)
        );
        assert_eq!(c.transfer_characteristics, 16);
    }

    #[test]
    fn leb128_and_obus() {
        assert_eq!(read_leb128(&[0x96, 0x01]), Some((150, 2)));
        // Temporal delimiter (type 2, size 0) + frame OBU (type 6, size 2).
        let tu = [0x12, 0x00, 0x32, 0x02, 0xaa, 0xbb];
        assert_eq!(av1_obu_types(&tu), vec![2, 6]);
    }

    #[test]
    fn av1_filter_prepends_sequence_header() {
        let seq = vec![0x0a, 0x01, 0x00];
        let f = BitstreamFilter::Av1 {
            config_obus: seq.clone(),
        };
        let tu = [0x12, 0x00, 0x32, 0x01, 0xaa];
        let mut out = Vec::new();
        f.apply(&tu, true, &mut out).unwrap();
        assert!(out.starts_with(&seq));
        f.apply(&tu, false, &mut out).unwrap();
        assert_eq!(out, tu);
        // Keyframe already carrying a sequence header: unchanged.
        let tu2 = [0x0a, 0x01, 0x00, 0x32, 0x01, 0xaa];
        f.apply(&tu2, true, &mut out).unwrap();
        assert_eq!(out, tu2);
    }
}
