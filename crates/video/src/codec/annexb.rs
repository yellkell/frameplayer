//! H.264 / HEVC bitstream-format conversion.
//!
//! MP4 and Matroska store NAL units length-prefixed (`avcC` / `hvcC`
//! "AVCC" format) with parameter sets out of band. V4L2 stateful decoders
//! (and most hardware) want Annex-B: start-code delimited NAL units with the
//! VPS/SPS/PPS in band before every random-access point. This module parses
//! the configuration records and performs that conversion.

use crate::bytes::Bytes;
use crate::error::{Result, VideoError};

pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalCodec {
    H264,
    Hevc,
}

impl NalCodec {
    pub fn nal_type(self, first_byte: u8) -> u8 {
        match self {
            NalCodec::H264 => first_byte & 0x1f,
            NalCodec::Hevc => (first_byte >> 1) & 0x3f,
        }
    }

    pub fn is_param_set(self, nal_type: u8) -> bool {
        match self {
            NalCodec::H264 => matches!(nal_type, 7 | 8),
            NalCodec::Hevc => matches!(nal_type, 32..=34),
        }
    }
}

/// Parsed `avcC` (AVCDecoderConfigurationRecord).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AvcConfig {
    pub profile: u8,
    pub level: u8,
    pub length_size: u8,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
    pub bit_depth_luma: u8,
}

impl AvcConfig {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut b = Bytes::new(data);
        let version = b.u8()?;
        if version != 1 {
            return Err(VideoError::invalid(format!("avcC version {version}")));
        }
        let profile = b.u8()?;
        let _compat = b.u8()?;
        let level = b.u8()?;
        let length_size = (b.u8()? & 3) + 1;
        let n_sps = b.u8()? & 0x1f;
        let mut sps = Vec::new();
        for _ in 0..n_sps {
            let len = b.u16()? as usize;
            sps.push(b.take(len)?.to_vec());
        }
        let n_pps = b.u8()?;
        let mut pps = Vec::new();
        for _ in 0..n_pps {
            let len = b.u16()? as usize;
            pps.push(b.take(len)?.to_vec());
        }
        let mut bit_depth_luma = 8;
        if matches!(profile, 100 | 110 | 122 | 144) && b.remaining() >= 4 {
            let _chroma = b.u8()? & 3;
            bit_depth_luma = (b.u8()? & 7) + 8;
        }
        Ok(AvcConfig {
            profile,
            level,
            length_size,
            sps,
            pps,
            bit_depth_luma,
        })
    }
}

/// Parsed `hvcC` (HEVCDecoderConfigurationRecord).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HevcConfig {
    pub profile_idc: u8,
    pub tier: u8,
    pub level_idc: u8,
    pub chroma_format: u8,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub length_size: u8,
    /// `(nal_unit_type, nal)` in record order (VPS, SPS, PPS, SEI…).
    pub arrays: Vec<(u8, Vec<u8>)>,
}

impl HevcConfig {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut b = Bytes::new(data);
        let version = b.u8()?;
        if version != 1 && version != 0 {
            return Err(VideoError::invalid(format!("hvcC version {version}")));
        }
        let p = b.u8()?;
        let tier = (p >> 5) & 1;
        let profile_idc = p & 0x1f;
        b.skip(4 + 6)?; // compatibility flags, constraint flags
        let level_idc = b.u8()?;
        b.skip(2 + 1)?; // min_spatial_segmentation, parallelismType
        let chroma_format = b.u8()? & 3;
        let bit_depth_luma = (b.u8()? & 7) + 8;
        let bit_depth_chroma = (b.u8()? & 7) + 8;
        b.skip(2)?; // avgFrameRate
        let length_size = (b.u8()? & 3) + 1;
        let n_arrays = b.u8()?;
        let mut arrays = Vec::new();
        for _ in 0..n_arrays {
            let t = b.u8()? & 0x3f;
            let n = b.u16()?;
            for _ in 0..n {
                let len = b.u16()? as usize;
                arrays.push((t, b.take(len)?.to_vec()));
            }
        }
        Ok(HevcConfig {
            profile_idc,
            tier,
            level_idc,
            chroma_format,
            bit_depth_luma,
            bit_depth_chroma,
            length_size,
            arrays,
        })
    }
}

/// Converts length-prefixed access units to Annex-B, inserting parameter
/// sets before keyframes that lack them.
#[derive(Debug, Clone)]
pub struct AnnexBConverter {
    codec: NalCodec,
    length_size: usize,
    /// Parameter sets (VPS/SPS/PPS) in decode order.
    param_sets: Vec<Vec<u8>>,
}

impl AnnexBConverter {
    pub fn from_avcc(record: &[u8]) -> Result<Self> {
        let c = AvcConfig::parse(record)?;
        Ok(AnnexBConverter {
            codec: NalCodec::H264,
            length_size: c.length_size as usize,
            param_sets: c.sps.into_iter().chain(c.pps).collect(),
        })
    }

    pub fn from_hvcc(record: &[u8]) -> Result<Self> {
        let c = HevcConfig::parse(record)?;
        let mut ps: Vec<(u8, Vec<u8>)> = c
            .arrays
            .into_iter()
            .filter(|(t, _)| (32..=34).contains(t))
            .collect();
        // VPS (32) → SPS (33) → PPS (34).
        ps.sort_by_key(|(t, _)| *t);
        Ok(AnnexBConverter {
            codec: NalCodec::Hevc,
            length_size: c.length_size as usize,
            param_sets: ps.into_iter().map(|(_, n)| n).collect(),
        })
    }

    pub fn codec(&self) -> NalCodec {
        self.codec
    }

    pub fn length_size(&self) -> usize {
        self.length_size
    }

    /// The out-of-band parameter sets as one Annex-B blob.
    pub fn param_sets_annexb(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for ps in &self.param_sets {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(ps);
        }
        out
    }

    /// Convert one access unit. If `keyframe` and the unit carries no
    /// in-band parameter sets, the stored ones are prepended.
    pub fn convert(&self, data: &[u8], keyframe: bool, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        if is_annexb(data) {
            // Already start-code delimited (e.g. from an MPEG-TS demuxer).
            if keyframe && !self.has_param_sets_annexb(data) {
                out.extend_from_slice(&self.param_sets_annexb());
            }
            out.extend_from_slice(data);
            return Ok(());
        }
        let nals = split_length_prefixed(data, self.length_size)?;
        let has_ps = nals
            .iter()
            .any(|n| !n.is_empty() && self.codec.is_param_set(self.codec.nal_type(n[0])));
        out.reserve(data.len() + 64);
        let mut ps_written = false;
        for nal in nals {
            if nal.is_empty() {
                continue;
            }
            let t = self.codec.nal_type(nal[0]);
            // Keep access-unit delimiters first; inject parameter sets after them.
            let is_aud = matches!((self.codec, t), (NalCodec::H264, 9) | (NalCodec::Hevc, 35));
            if keyframe && !has_ps && !ps_written && !is_aud {
                out.extend_from_slice(&self.param_sets_annexb());
                ps_written = true;
            }
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
        }
        Ok(())
    }

    fn has_param_sets_annexb(&self, data: &[u8]) -> bool {
        split_annexb(data)
            .any(|n| !n.is_empty() && self.codec.is_param_set(self.codec.nal_type(n[0])))
    }
}

/// True if `data` begins with a 3- or 4-byte start code.
pub fn is_annexb(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

/// Split a length-prefixed access unit into NAL payloads.
pub fn split_length_prefixed(data: &[u8], length_size: usize) -> Result<Vec<&[u8]>> {
    if !(1..=4).contains(&length_size) {
        return Err(VideoError::invalid(format!(
            "NAL length size {length_size}"
        )));
    }
    let mut out = Vec::new();
    let mut p = 0;
    while p < data.len() {
        if p + length_size > data.len() {
            return Err(VideoError::invalid("truncated NAL length"));
        }
        let len = data[p..p + length_size]
            .iter()
            .fold(0usize, |a, &b| (a << 8) | b as usize);
        p += length_size;
        if p + len > data.len() {
            return Err(VideoError::invalid(format!(
                "NAL of {len} bytes overruns packet"
            )));
        }
        out.push(&data[p..p + len]);
        p += len;
    }
    Ok(out)
}

/// Iterate the NAL units of an Annex-B stream (without start codes).
pub fn split_annexb(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| {
            // Strip the start code (and a leading zero of a 4-byte one).
            let mut e = s - 3;
            if e > 0 && data[e - 1] == 0 {
                e -= 1;
            }
            e
        })
        .chain(std::iter::once(data.len()))
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .map(move |(s, e)| &data[s..e.max(s)])
}

/// Convert an Annex-B access unit to length-prefixed form (4-byte lengths).
pub fn annexb_to_length_prefixed(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    for nal in split_annexb(data) {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal hvcC with one VPS, SPS, PPS.
    pub fn sample_hvcc() -> Vec<u8> {
        let mut v = vec![
            1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 93, 0xf0, 0, 0xfc, 0xfd, 0xfa, 0xfa, 0, 0,
            0x0f,
        ];
        v.push(3); // arrays
        for (t, nal) in [
            (32u8, vec![0x40, 0x01, 0xaa]),
            (33, vec![0x42, 0x01, 0xbb, 0xbc]),
            (34, vec![0x44, 0x01, 0xcc]),
        ] {
            v.push(0x80 | t);
            v.extend_from_slice(&1u16.to_be_bytes());
            v.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            v.extend_from_slice(&nal);
        }
        v
    }

    pub fn sample_avcc() -> Vec<u8> {
        let sps = [0x67, 0x64, 0x00, 0x1f, 0xac];
        let pps = [0x68, 0xee, 0x3c, 0x80];
        let mut v = vec![1, 0x64, 0x00, 0x1f, 0xff, 0xe1];
        v.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        v.extend_from_slice(&sps);
        v.push(1);
        v.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        v.extend_from_slice(&pps);
        v.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0x00]); // chroma 4:2:0, 8-bit
        v
    }

    #[test]
    fn parse_hvcc() {
        let c = HevcConfig::parse(&sample_hvcc()).unwrap();
        assert_eq!(c.profile_idc, 1);
        assert_eq!(c.level_idc, 93);
        assert_eq!(c.length_size, 4);
        assert_eq!(c.bit_depth_luma, 10);
        assert_eq!(c.arrays.len(), 3);
        assert_eq!(c.arrays[1].0, 33);
    }

    #[test]
    fn parse_avcc() {
        let c = AvcConfig::parse(&sample_avcc()).unwrap();
        assert_eq!((c.profile, c.level, c.length_size), (100, 31, 4));
        assert_eq!(c.sps.len(), 1);
        assert_eq!(c.pps[0], vec![0x68, 0xee, 0x3c, 0x80]);
        assert_eq!(c.bit_depth_luma, 8);
    }

    #[test]
    fn hevc_keyframe_gets_param_sets() {
        let conv = AnnexBConverter::from_hvcc(&sample_hvcc()).unwrap();
        // One IDR slice NAL (type 19 → first byte 0x26).
        let au = [0, 0, 0, 3, 0x26, 0x01, 0xaf];
        let mut out = Vec::new();
        conv.convert(&au, true, &mut out).unwrap();
        let nals: Vec<&[u8]> = split_annexb(&out).collect();
        assert_eq!(nals.len(), 4);
        assert_eq!(nals[0], &[0x40, 0x01, 0xaa]);
        assert_eq!(nals[1], &[0x42, 0x01, 0xbb, 0xbc]);
        assert_eq!(nals[2], &[0x44, 0x01, 0xcc]);
        assert_eq!(nals[3], &[0x26, 0x01, 0xaf]);
        assert!(out.starts_with(&START_CODE));
        // Non-keyframes are converted without parameter sets.
        conv.convert(&[0, 0, 0, 3, 0x02, 0x01, 0x11], false, &mut out)
            .unwrap();
        assert_eq!(out, vec![0, 0, 0, 1, 0x02, 0x01, 0x11]);
    }

    #[test]
    fn inband_param_sets_not_duplicated() {
        let conv = AnnexBConverter::from_avcc(&sample_avcc()).unwrap();
        let mut au = Vec::new();
        for nal in [&[0x67u8, 0x42][..], &[0x68, 0x01], &[0x65, 0x88, 0x84]] {
            au.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            au.extend_from_slice(nal);
        }
        let mut out = Vec::new();
        conv.convert(&au, true, &mut out).unwrap();
        assert_eq!(split_annexb(&out).count(), 3);
    }

    #[test]
    fn aud_stays_first() {
        let conv = AnnexBConverter::from_avcc(&sample_avcc()).unwrap();
        let au = [0, 0, 0, 2, 0x09, 0xf0, 0, 0, 0, 2, 0x65, 0x88];
        let mut out = Vec::new();
        conv.convert(&au, true, &mut out).unwrap();
        let nals: Vec<&[u8]> = split_annexb(&out).collect();
        assert_eq!(nals[0], &[0x09, 0xf0]);
        assert_eq!(nals[1][0], 0x67);
        assert_eq!(nals[2][0], 0x68);
        assert_eq!(nals[3], &[0x65, 0x88]);
    }

    #[test]
    fn two_byte_lengths_and_errors() {
        let conv = AnnexBConverter {
            codec: NalCodec::H264,
            length_size: 2,
            param_sets: vec![],
        };
        let mut out = Vec::new();
        conv.convert(&[0, 2, 0x41, 0x9a], false, &mut out).unwrap();
        assert_eq!(out, vec![0, 0, 0, 1, 0x41, 0x9a]);
        assert!(conv.convert(&[0, 9, 0x41], false, &mut out).is_err());
    }

    #[test]
    fn annexb_roundtrip() {
        let lp = [0, 0, 0, 2, 0x41, 0x9a, 0, 0, 0, 3, 0x01, 0x02, 0x03];
        let conv = AnnexBConverter {
            codec: NalCodec::H264,
            length_size: 4,
            param_sets: vec![],
        };
        let mut ab = Vec::new();
        conv.convert(&lp, false, &mut ab).unwrap();
        assert_eq!(annexb_to_length_prefixed(&ab), lp.to_vec());
        // Already-Annex-B input passes through.
        let mut again = Vec::new();
        conv.convert(&ab, false, &mut again).unwrap();
        assert_eq!(again, ab);
    }
}
