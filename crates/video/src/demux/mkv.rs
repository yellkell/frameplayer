//! Pure-Rust Matroska / WebM demuxer.
//!
//! Header elements (`Info`, `Tracks`, `Chapters`, `Cues`, located directly
//! or through `SeekHead`) are read into memory and parsed; clusters are then
//! streamed with a flat element scan, which also copes with unknown-size
//! (live-written) clusters. Supports `SimpleBlock` and `BlockGroup`
//! (`ReferenceBlock` → non-key), Xiph / fixed / EBML lacing, header-stripping
//! content compression, `Video/Colour` (PQ/HLG, bit depth), `StereoMode`
//! and `Projection` metadata. Seeking uses `Cues` when present and otherwise
//! a lazily built cluster index; either way it lands on the first video
//! keyframe at or before the target.

use crate::codec::bit_depth_from_config;
use crate::demux::Demuxer;
use crate::error::{Result, VideoError};
use crate::input::{BufferedInput, MediaInput};
use crate::packet::{
    media_info_from_tracks, AudioParams, CodecId, Packet, TrackDesc, TrackKind, VideoParams,
};
use crate::spherical::{mkv_projection, mkv_stereo_mode};
use fp_core::media::Chapter;
use fp_core::{MediaInfo, MediaTime};
use std::collections::VecDeque;
use std::io::{Seek, SeekFrom};

// Element IDs (with length marker bits, as written in files).
const EBML: u32 = 0x1A45DFA3;
const DOC_TYPE: u32 = 0x4282;
const SEGMENT: u32 = 0x18538067;
const SEEK_HEAD: u32 = 0x114D9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549A966;
const TIMECODE_SCALE: u32 = 0x2AD7B1;
const DURATION: u32 = 0x4489;
const TRACKS: u32 = 0x1654AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const FLAG_DEFAULT: u32 = 0x88;
const DEFAULT_DURATION: u32 = 0x23E383;
const NAME: u32 = 0x536E;
const LANGUAGE: u32 = 0x22B59C;
const LANGUAGE_BCP47: u32 = 0x22B59D;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const STEREO_MODE: u32 = 0x53B8;
const COLOUR: u32 = 0x55B0;
const BITS_PER_CHANNEL: u32 = 0x55B2;
const TRANSFER_CHARACTERISTICS: u32 = 0x55BA;
const PROJECTION: u32 = 0x7670;
const PROJECTION_TYPE: u32 = 0x7671;
const PROJECTION_PRIVATE: u32 = 0x7672;
const AUDIO: u32 = 0xE1;
const SAMPLING_FREQUENCY: u32 = 0xB5;
const CHANNELS: u32 = 0x9F;
const BIT_DEPTH: u32 = 0x6264;
const CONTENT_ENCODINGS: u32 = 0x6D80;
const CONTENT_ENCODING: u32 = 0x6240;
const CONTENT_COMPRESSION: u32 = 0x5034;
const CONTENT_COMP_ALGO: u32 = 0x4254;
const CONTENT_COMP_SETTINGS: u32 = 0x4255;
const CHAPTERS: u32 = 0x1043A770;
const EDITION_ENTRY: u32 = 0x45B9;
const CHAPTER_ATOM: u32 = 0xB6;
const CHAPTER_TIME_START: u32 = 0x91;
const CHAPTER_DISPLAY: u32 = 0x80;
const CHAP_STRING: u32 = 0x85;
const CUES: u32 = 0x1C53BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;
const CLUSTER: u32 = 0x1F43B675;
const TIMECODE: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const BLOCK_DURATION: u32 = 0x9B;
const REFERENCE_BLOCK: u32 = 0xFB;

/// Parse a variable-length integer from a slice: `(value, len, all_ones)`.
fn vint(data: &[u8], keep_marker: bool) -> Option<(u64, usize, bool)> {
    let first = *data.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || data.len() < len {
        return None;
    }
    let mut v = if keep_marker {
        first as u64
    } else {
        (first as u64) & (0xff >> len)
    };
    let mut ones = (first as u64) & (0xff >> len) == (0xff >> len);
    for &b in &data[1..len] {
        v = (v << 8) | b as u64;
        ones &= b == 0xff;
    }
    Some((v, len, ones))
}

/// Iterate `(id, payload)` children of an in-memory master element.
fn children(data: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    let mut p = 0usize;
    std::iter::from_fn(move || {
        let (id, il, _) = vint(data.get(p..)?, true)?;
        let (size, sl, unknown) = vint(data.get(p + il..)?, false)?;
        let start = p + il + sl;
        let end = if unknown {
            data.len()
        } else {
            start.checked_add(size as usize)?.min(data.len())
        };
        p = end;
        Some((id as u32, &data[start..end]))
    })
}

fn uint(data: &[u8]) -> u64 {
    data.iter().take(8).fold(0u64, |a, &b| (a << 8) | b as u64)
}

fn float(data: &[u8]) -> f64 {
    match data.len() {
        4 => f32::from_be_bytes(data.try_into().unwrap()) as f64,
        8 => f64::from_be_bytes(data.try_into().unwrap()),
        _ => 0.0,
    }
}

fn string(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

#[derive(Debug, Clone)]
struct MkvTrack {
    desc: TrackDesc,
    default_duration_ns: u64,
    strip_prefix: Vec<u8>,
    enabled: bool,
}

#[derive(Debug, Clone, Copy)]
struct CuePoint {
    time: u64,
    track: u64,
    cluster_pos: u64,
}

pub struct MkvDemuxer {
    input: BufferedInput<Box<dyn MediaInput>>,
    doc_type: String,
    timecode_scale: u64,
    segment_start: u64,
    segment_end: Option<u64>,
    first_cluster: u64,
    tracks: Vec<MkvTrack>,
    descs: Vec<TrackDesc>,
    info: MediaInfo,
    cues: Vec<CuePoint>,
    cluster_index: Option<Vec<(u64, u64)>>,
    // Streaming state.
    pos: u64,
    cluster_tc: u64,
    pending: VecDeque<Packet>,
    skip_until_key: Option<u32>,
}

/// Header of an element read from the stream.
struct ElemHeader {
    id: u32,
    size: Option<u64>,
    header_len: u64,
}

fn read_header(r: &mut dyn MediaInput) -> Result<Option<ElemHeader>> {
    let mut b = [0u8; 12];
    let n = super::read_up_to(r, &mut b[..1])?;
    if n == 0 {
        return Ok(None);
    }
    let il = b[0].leading_zeros() as usize + 1;
    if il > 4 {
        return Err(VideoError::invalid("bad EBML id"));
    }
    if super::read_up_to(r, &mut b[1..il])? < il - 1 {
        return Ok(None);
    }
    if super::read_up_to(r, &mut b[il..il + 1])? == 0 {
        return Ok(None);
    }
    let sl = b[il].leading_zeros() as usize + 1;
    if sl > 8 {
        return Err(VideoError::invalid("bad EBML size"));
    }
    if super::read_up_to(r, &mut b[il + 1..il + sl])? < sl - 1 {
        return Ok(None);
    }
    let (id, _, _) = vint(&b[..il], true).ok_or_else(|| VideoError::invalid("bad EBML id"))?;
    let (size, _, unknown) =
        vint(&b[il..il + sl], false).ok_or_else(|| VideoError::invalid("bad EBML size"))?;
    Ok(Some(ElemHeader {
        id: id as u32,
        size: (!unknown).then_some(size),
        header_len: (il + sl) as u64,
    }))
}

fn read_body(r: &mut dyn MediaInput, size: u64) -> Result<Vec<u8>> {
    if size > 256 * 1024 * 1024 {
        return Err(VideoError::invalid(format!("element of {size} bytes")));
    }
    let mut v = vec![0u8; size as usize];
    r.read_exact(&mut v)?;
    Ok(v)
}

fn parse_track(entry: &[u8]) -> Option<MkvTrack> {
    let mut number = 0u64;
    let mut ttype = 0u64;
    let mut codec_id = String::new();
    let mut private = Vec::new();
    let mut name = None;
    let mut lang = None;
    let mut default = true;
    let mut default_duration_ns = 0u64;
    let mut strip_prefix = Vec::new();
    let mut video = VideoParams::default();
    let mut audio = AudioParams {
        channels: 1,
        sample_rate: 8000,
        ..Default::default()
    };
    let mut has_video_bits = false;
    for (id, d) in children(entry) {
        match id {
            TRACK_NUMBER => number = uint(d),
            TRACK_TYPE => ttype = uint(d),
            FLAG_DEFAULT => default = uint(d) != 0,
            DEFAULT_DURATION => default_duration_ns = uint(d),
            NAME => name = Some(string(d)),
            LANGUAGE => lang = lang.or(Some(string(d))),
            LANGUAGE_BCP47 => lang = Some(string(d)),
            CODEC_ID => codec_id = string(d),
            CODEC_PRIVATE => private = d.to_vec(),
            VIDEO => {
                for (vid, vd) in children(d) {
                    match vid {
                        PIXEL_WIDTH => video.width = uint(vd) as u32,
                        PIXEL_HEIGHT => video.height = uint(vd) as u32,
                        STEREO_MODE => video.stereo = mkv_stereo_mode(uint(vd)),
                        COLOUR => {
                            for (cid, cd) in children(vd) {
                                match cid {
                                    BITS_PER_CHANNEL => {
                                        video.bit_depth = uint(cd) as u8;
                                        has_video_bits = video.bit_depth > 0;
                                    }
                                    TRANSFER_CHARACTERISTICS => {
                                        video.transfer = super::mp4::transfer_from_code(uint(cd));
                                    }
                                    _ => {}
                                }
                            }
                        }
                        PROJECTION => {
                            let mut ptype = 0;
                            let mut ppriv: &[u8] = &[];
                            for (pid, pd) in children(vd) {
                                match pid {
                                    PROJECTION_TYPE => ptype = uint(pd),
                                    PROJECTION_PRIVATE => ppriv = pd,
                                    _ => {}
                                }
                            }
                            video.projection = mkv_projection(ptype, ppriv);
                        }
                        _ => {}
                    }
                }
            }
            AUDIO => {
                for (aid, ad) in children(d) {
                    match aid {
                        SAMPLING_FREQUENCY => audio.sample_rate = float(ad) as u32,
                        CHANNELS => audio.channels = uint(ad) as u16,
                        BIT_DEPTH => audio.bits_per_sample = uint(ad) as u16,
                        _ => {}
                    }
                }
            }
            CONTENT_ENCODINGS => {
                for (_, enc) in children(d).filter(|(i, _)| *i == CONTENT_ENCODING) {
                    for (_, comp) in children(enc).filter(|(i, _)| *i == CONTENT_COMPRESSION) {
                        let mut algo = 0;
                        let mut settings = Vec::new();
                        for (cid, cd) in children(comp) {
                            match cid {
                                CONTENT_COMP_ALGO => algo = uint(cd),
                                CONTENT_COMP_SETTINGS => settings = cd.to_vec(),
                                _ => {}
                            }
                        }
                        if algo == 3 {
                            strip_prefix = settings;
                        } else {
                            tracing::warn!(
                                "track {number}: unsupported content compression {algo}"
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let kind = match ttype {
        1 => TrackKind::Video,
        2 => TrackKind::Audio,
        0x11 => TrackKind::Subtitle,
        _ => TrackKind::Other,
    };
    let bits = audio.bits_per_sample.max(16) as u8;
    let codec = match codec_id.as_str() {
        "V_MPEG4/ISO/AVC" => CodecId::H264,
        "V_MPEGH/ISO/HEVC" => CodecId::Hevc,
        "V_VP8" => CodecId::Vp8,
        "V_VP9" => CodecId::Vp9,
        "V_AV1" => CodecId::Av1,
        "A_OPUS" => CodecId::Opus,
        "A_VORBIS" => CodecId::Vorbis,
        "A_FLAC" => CodecId::Flac,
        "A_AC3" => CodecId::Ac3,
        "A_EAC3" => CodecId::Eac3,
        "A_MPEG/L3" => CodecId::Mp3,
        "A_PCM/INT/LIT" => CodecId::Pcm {
            bits,
            float: false,
            big_endian: false,
        },
        "A_PCM/INT/BIG" => CodecId::Pcm {
            bits,
            float: false,
            big_endian: true,
        },
        "A_PCM/FLOAT/IEEE" => CodecId::Pcm {
            bits: bits.max(32),
            float: true,
            big_endian: false,
        },
        s if s.starts_with("A_AAC") => CodecId::Aac,
        "S_TEXT/UTF8" | "S_TEXT/ASCII" => CodecId::SubRip,
        "S_TEXT/ASS" | "S_TEXT/SSA" | "S_ASS" | "S_SSA" => CodecId::Ass,
        "S_TEXT/WEBVTT" | "D_WEBVTT/SUBTITLES" => CodecId::WebVtt,
        "S_HDMV/PGS" => CodecId::Pgs,
        other => CodecId::Unknown(other.to_string()),
    };
    let mut desc = TrackDesc::new(number as u32, kind, codec.clone());
    desc.codec_private = private;
    desc.name = name.filter(|s| !s.is_empty());
    desc.language = lang.filter(|l| !l.is_empty() && l != "und");
    desc.default = default;
    match kind {
        TrackKind::Video => {
            if !has_video_bits {
                video.bit_depth = bit_depth_from_config(&codec, &desc.codec_private).unwrap_or(8);
            }
            if default_duration_ns > 0 {
                video.fps = (1e9 / default_duration_ns as f64 * 1000.0).round() / 1000.0;
            }
            desc.video = Some(video);
        }
        TrackKind::Audio => {
            if codec == CodecId::Aac {
                if let Some((sr, ch)) = super::mp4::aac_asc_info(&desc.codec_private) {
                    if sr > 0 {
                        audio.sample_rate = sr;
                    }
                    if ch > 0 {
                        audio.channels = ch;
                    }
                }
            }
            desc.audio = Some(audio);
        }
        _ => {}
    }
    Some(MkvTrack {
        desc,
        default_duration_ns,
        strip_prefix,
        enabled: true,
    })
}

fn parse_chapters(data: &[u8]) -> Vec<Chapter> {
    let mut out = Vec::new();
    // First edition only.
    if let Some((_, ed)) = children(data).find(|(id, _)| *id == EDITION_ENTRY) {
        for (_, atom) in children(ed).filter(|(id, _)| *id == CHAPTER_ATOM) {
            let mut start = 0u64;
            let mut title = String::new();
            for (id, d) in children(atom) {
                match id {
                    CHAPTER_TIME_START => start = uint(d),
                    CHAPTER_DISPLAY if title.is_empty() => {
                        if let Some((_, s)) = children(d).find(|(i, _)| *i == CHAP_STRING) {
                            title = string(s);
                        }
                    }
                    _ => {}
                }
            }
            out.push(Chapter {
                start: MediaTime((start / 1000) as i64),
                title,
            });
        }
    }
    out.sort_by_key(|c| c.start);
    out
}

fn parse_cues(data: &[u8]) -> Vec<CuePoint> {
    let mut out = Vec::new();
    for (_, cp) in children(data).filter(|(id, _)| *id == CUE_POINT) {
        let mut time = 0;
        for (id, d) in children(cp) {
            match id {
                CUE_TIME => time = uint(d),
                CUE_TRACK_POSITIONS => {
                    let mut track = 0;
                    let mut pos = None;
                    for (pid, pd) in children(d) {
                        match pid {
                            CUE_TRACK => track = uint(pd),
                            CUE_CLUSTER_POSITION => pos = Some(uint(pd)),
                            _ => {}
                        }
                    }
                    if let Some(cluster_pos) = pos {
                        out.push(CuePoint {
                            time,
                            track,
                            cluster_pos,
                        });
                    }
                }
                _ => {}
            }
        }
    }
    out.sort_by_key(|c| c.time);
    out
}

/// Split a (Simple)Block payload into frames according to its lacing.
fn unlace(data: &[u8], lacing: u8) -> Result<Vec<&[u8]>> {
    if lacing == 0 {
        return Ok(vec![data]);
    }
    let bad = || VideoError::invalid("bad lacing");
    let count = *data.first().ok_or_else(bad)? as usize + 1;
    let mut p = 1usize;
    let mut sizes = Vec::with_capacity(count);
    match lacing {
        1 => {
            for _ in 0..count - 1 {
                let mut s = 0usize;
                loop {
                    let b = *data.get(p).ok_or_else(bad)?;
                    p += 1;
                    s += b as usize;
                    if b != 255 {
                        break;
                    }
                }
                sizes.push(s);
            }
        }
        2 => {
            let each = (data.len() - 1) / count;
            sizes = vec![each; count - 1];
        }
        3 => {
            let (first, l, _) = vint(data.get(p..).ok_or_else(bad)?, false).ok_or_else(bad)?;
            p += l;
            sizes.push(first as usize);
            let mut prev = first as i64;
            for _ in 1..count - 1 {
                let (raw, l, _) = vint(data.get(p..).ok_or_else(bad)?, false).ok_or_else(bad)?;
                p += l;
                let bias = (1i64 << (7 * l - 1)) - 1;
                prev += raw as i64 - bias;
                if prev < 0 {
                    return Err(bad());
                }
                sizes.push(prev as usize);
            }
        }
        _ => return Err(bad()),
    }
    let used: usize = sizes.iter().sum();
    if p + used > data.len() {
        return Err(bad());
    }
    sizes.push(data.len() - p - used);
    let mut frames = Vec::with_capacity(count);
    for s in sizes {
        frames.push(&data[p..p + s]);
        p += s;
    }
    Ok(frames)
}

impl MkvDemuxer {
    pub fn open(input: Box<dyn MediaInput>) -> Result<Self> {
        let mut input = BufferedInput::new(input);
        input.seek(SeekFrom::Start(0))?;
        let h = read_header(&mut input)?.ok_or_else(|| VideoError::invalid("empty file"))?;
        if h.id != EBML {
            return Err(VideoError::invalid("not an EBML file"));
        }
        let ebml = read_body(&mut input, h.size.unwrap_or(0))?;
        let doc_type = children(&ebml)
            .find(|(id, _)| *id == DOC_TYPE)
            .map(|(_, d)| string(d))
            .unwrap_or_default();
        if doc_type != "matroska" && doc_type != "webm" {
            return Err(VideoError::Unsupported(format!(
                "EBML doc type {doc_type:?}"
            )));
        }
        let seg = read_header(&mut input)?.ok_or_else(|| VideoError::invalid("no segment"))?;
        if seg.id != SEGMENT {
            return Err(VideoError::invalid("expected Segment"));
        }
        let segment_start = input.stream_position()?;
        let segment_end = seg.size.map(|s| segment_start + s);
        let mut timecode_scale = 1_000_000u64;
        let mut duration_ticks = None;
        let mut tracks = Vec::new();
        let mut chapters = Vec::new();
        let mut cues = Vec::new();
        let mut seek_cues = None;
        let mut seek_chapters = None;
        let mut first_cluster = None;
        let mut pos = segment_start;
        loop {
            if segment_end.is_some_and(|e| pos >= e) {
                break;
            }
            input.seek(SeekFrom::Start(pos))?;
            let Some(h) = read_header(&mut input)? else {
                break;
            };
            let body_at = pos + h.header_len;
            if h.id == CLUSTER {
                first_cluster = Some(pos);
                break;
            }
            let Some(size) = h.size else {
                return Err(VideoError::invalid("unknown-size header element"));
            };
            match h.id {
                SEEK_HEAD | INFO | TRACKS | CHAPTERS | CUES => {
                    let body = read_body(&mut input, size)?;
                    match h.id {
                        SEEK_HEAD => {
                            for (_, s) in children(&body).filter(|(id, _)| *id == SEEK) {
                                let mut sid = 0;
                                let mut spos = 0;
                                for (id, d) in children(s) {
                                    match id {
                                        SEEK_ID => sid = uint(d) as u32,
                                        SEEK_POSITION => spos = uint(d),
                                        _ => {}
                                    }
                                }
                                match sid {
                                    CUES => seek_cues = Some(segment_start + spos),
                                    CHAPTERS => seek_chapters = Some(segment_start + spos),
                                    _ => {}
                                }
                            }
                        }
                        INFO => {
                            for (id, d) in children(&body) {
                                match id {
                                    TIMECODE_SCALE => timecode_scale = uint(d).max(1),
                                    DURATION => duration_ticks = Some(float(d)),
                                    _ => {}
                                }
                            }
                        }
                        TRACKS => {
                            tracks = children(&body)
                                .filter(|(id, _)| *id == TRACK_ENTRY)
                                .filter_map(|(_, e)| parse_track(e))
                                .collect();
                        }
                        CHAPTERS => chapters = parse_chapters(&body),
                        CUES => cues = parse_cues(&body),
                        _ => unreachable!(),
                    }
                }
                _ => {}
            }
            pos = body_at + size;
        }
        let first_cluster = first_cluster.unwrap_or(pos);
        let mut load = |at: Option<u64>, want: u32| -> Option<Vec<u8>> {
            let at = at?;
            input.seek(SeekFrom::Start(at)).ok()?;
            let h = read_header(&mut input).ok()??;
            (h.id == want)
                .then(|| read_body(&mut input, h.size?).ok())
                .flatten()
        };
        if cues.is_empty() {
            if let Some(b) = load(seek_cues, CUES) {
                cues = parse_cues(&b);
            }
        }
        if chapters.is_empty() {
            if let Some(b) = load(seek_chapters, CHAPTERS) {
                chapters = parse_chapters(&b);
            }
        }
        if tracks.is_empty() {
            return Err(VideoError::invalid("Matroska file without tracks"));
        }
        let duration =
            duration_ticks.map(|t| MediaTime((t * timecode_scale as f64 / 1000.0) as i64));
        let descs: Vec<TrackDesc> = tracks.iter().map(|t: &MkvTrack| t.desc.clone()).collect();
        let info = media_info_from_tracks(&doc_type, duration, &descs, chapters);
        Ok(MkvDemuxer {
            input,
            doc_type,
            timecode_scale,
            segment_start,
            segment_end,
            first_cluster,
            tracks,
            descs,
            info,
            cues,
            cluster_index: None,
            pos: first_cluster,
            cluster_tc: 0,
            pending: VecDeque::new(),
            skip_until_key: None,
        })
    }

    fn ticks_to_time(&self, ticks: i64) -> MediaTime {
        MediaTime((ticks as i128 * self.timecode_scale as i128 / 1000) as i64)
    }

    fn time_to_ticks(&self, t: MediaTime) -> i64 {
        (t.0 as i128 * 1000 / self.timecode_scale as i128) as i64
    }

    fn primary_video(&self) -> Option<u32> {
        self.tracks
            .iter()
            .find(|t| t.desc.kind == TrackKind::Video && t.enabled)
            .map(|t| t.desc.id)
    }

    fn handle_block(
        &mut self,
        data: &[u8],
        simple: bool,
        has_reference: bool,
        block_duration: Option<u64>,
    ) -> Result<()> {
        let (track_no, tl, _) =
            vint(data, false).ok_or_else(|| VideoError::invalid("bad block track"))?;
        if data.len() < tl + 3 {
            return Err(VideoError::invalid("short block"));
        }
        let Some(track) = self.tracks.iter().find(|t| t.desc.id as u64 == track_no) else {
            return Ok(());
        };
        if !track.enabled {
            return Ok(());
        }
        let rel = i16::from_be_bytes([data[tl], data[tl + 1]]) as i64;
        let flags = data[tl + 2];
        let keyframe = if simple {
            flags & 0x80 != 0
        } else {
            !has_reference
        };
        let lacing = (flags >> 1) & 3;
        let frames = unlace(&data[tl + 3..], lacing)?;
        let base_ticks = self.cluster_tc as i64 + rel;
        let base = self.ticks_to_time(base_ticks);
        let dd = MediaTime((track.default_duration_ns / 1000) as i64);
        let duration = match block_duration {
            Some(d) => self.ticks_to_time(d as i64),
            None => dd,
        };
        let frame_dur = if frames.len() > 1 && block_duration.is_some() {
            MediaTime(duration.0 / frames.len() as i64)
        } else {
            dd
        };
        let id = track.desc.id;
        let prefix = track.strip_prefix.clone();
        let n = frames.len();
        for (i, f) in frames.into_iter().enumerate() {
            let pts = base + MediaTime(frame_dur.0 * i as i64);
            let mut payload = Vec::with_capacity(prefix.len() + f.len());
            payload.extend_from_slice(&prefix);
            payload.extend_from_slice(f);
            self.pending.push_back(Packet {
                track: id,
                pts,
                dts: pts,
                duration: if n == 1 { duration } else { frame_dur },
                keyframe,
                data: payload,
            });
        }
        Ok(())
    }

    /// Scan forward until at least one packet is pending or EOF.
    fn fill(&mut self) -> Result<bool> {
        while self.pending.is_empty() {
            if self.segment_end.is_some_and(|e| self.pos >= e) {
                return Ok(false);
            }
            self.input.seek(SeekFrom::Start(self.pos))?;
            let Some(h) = read_header(&mut self.input)? else {
                return Ok(false);
            };
            let body_at = self.pos + h.header_len;
            match h.id {
                CLUSTER | SEGMENT => {
                    // Step into the cluster; its children follow.
                    self.pos = body_at;
                }
                TIMECODE => {
                    let b = read_body(&mut self.input, h.size.unwrap_or(0))?;
                    self.cluster_tc = uint(&b);
                    self.pos = body_at + h.size.unwrap_or(0);
                }
                SIMPLE_BLOCK => {
                    let size = h
                        .size
                        .ok_or_else(|| VideoError::invalid("unknown-size block"))?;
                    let b = read_body(&mut self.input, size)?;
                    self.pos = body_at + size;
                    if let Err(e) = self.handle_block(&b, true, false, None) {
                        tracing::warn!("skipping bad SimpleBlock at {}: {e}", body_at);
                    }
                }
                BLOCK_GROUP => {
                    let size = h
                        .size
                        .ok_or_else(|| VideoError::invalid("unknown-size block group"))?;
                    let b = read_body(&mut self.input, size)?;
                    self.pos = body_at + size;
                    let mut block = None;
                    let mut has_ref = false;
                    let mut dur = None;
                    for (id, d) in children(&b) {
                        match id {
                            BLOCK => block = Some(d),
                            REFERENCE_BLOCK => has_ref = true,
                            BLOCK_DURATION => dur = Some(uint(d)),
                            _ => {}
                        }
                    }
                    if let Some(bl) = block {
                        if let Err(e) = self.handle_block(bl, false, has_ref, dur) {
                            tracing::warn!("skipping bad Block at {}: {e}", body_at);
                        }
                    }
                }
                _ => match h.size {
                    Some(s) => self.pos = body_at + s,
                    None => self.pos = body_at,
                },
            }
            // Drop packets until the video keyframe after a seek.
            if let Some(vid) = self.skip_until_key {
                while let Some(p) = self.pending.front() {
                    if p.track == vid && p.keyframe {
                        self.skip_until_key = None;
                        break;
                    }
                    self.pending.pop_front();
                }
            }
        }
        Ok(true)
    }

    /// Cluster `(timecode, file position)` list, built by scanning cluster
    /// headers when the file has no Cues.
    fn cluster_index(&mut self) -> Result<Vec<(u64, u64)>> {
        if let Some(ix) = &self.cluster_index {
            return Ok(ix.clone());
        }
        let mut ix = Vec::new();
        let mut pos = self.first_cluster;
        loop {
            if self.segment_end.is_some_and(|e| pos >= e) {
                break;
            }
            self.input.seek(SeekFrom::Start(pos))?;
            let Some(h) = read_header(&mut self.input)? else {
                break;
            };
            let body_at = pos + h.header_len;
            if h.id == CLUSTER {
                if let Some(ch) = read_header(&mut self.input)? {
                    if ch.id == TIMECODE {
                        let b = read_body(&mut self.input, ch.size.unwrap_or(0))?;
                        ix.push((uint(&b), pos));
                    }
                }
            }
            match h.size {
                Some(s) => pos = body_at + s,
                None => break, // unknown-size cluster: cannot skip
            }
        }
        self.cluster_index = Some(ix.clone());
        Ok(ix)
    }

    fn position_at(&mut self, pos: u64) {
        self.pos = pos;
        self.cluster_tc = 0;
        self.pending.clear();
    }
}

impl Demuxer for MkvDemuxer {
    fn format_name(&self) -> &str {
        &self.doc_type
    }

    fn tracks(&self) -> &[TrackDesc] {
        &self.descs
    }

    fn media_info(&self) -> &MediaInfo {
        &self.info
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        if !self.fill()? {
            return Ok(None);
        }
        Ok(self.pending.pop_front())
    }

    fn seek(&mut self, target: MediaTime) -> Result<MediaTime> {
        let target_ticks = self.time_to_ticks(target).max(0) as u64;
        let video = self.primary_video();
        // Candidate cluster positions, latest first.
        let mut candidates: Vec<u64> = if !self.cues.is_empty() {
            let vt = video.map(|v| v as u64);
            let mut c: Vec<&CuePoint> = self
                .cues
                .iter()
                .filter(|c| vt.is_none_or(|v| c.track == v) && c.time <= target_ticks)
                .collect();
            if c.is_empty() {
                c = self.cues.iter().take(1).collect();
            }
            c.iter()
                .rev()
                .map(|c| self.segment_start + c.cluster_pos)
                .collect()
        } else {
            let ix = self.cluster_index()?;
            let mut c: Vec<u64> = ix
                .iter()
                .filter(|(tc, _)| *tc <= target_ticks)
                .map(|(_, p)| *p)
                .collect();
            c.reverse();
            c
        };
        candidates.push(self.first_cluster);
        candidates.dedup();
        for &pos in candidates.iter().take(4) {
            self.position_at(pos);
            let Some(vid) = video else {
                // Audio-only: drop packets before the target.
                while self.fill()? {
                    if self
                        .pending
                        .front()
                        .is_some_and(|p| p.pts + p.duration > target)
                    {
                        return Ok(self.pending.front().unwrap().pts);
                    }
                    self.pending.pop_front();
                }
                return Ok(target);
            };
            self.skip_until_key = Some(vid);
            if !self.fill()? {
                continue;
            }
            let key_pts = self.pending.front().map(|p| p.pts).unwrap_or(target);
            if key_pts <= target || pos == self.first_cluster {
                return Ok(key_pts);
            }
        }
        self.position_at(self.first_cluster);
        self.skip_until_key = video;
        Ok(MediaTime::ZERO)
    }

    fn set_track_enabled(&mut self, track: u32, enabled: bool) {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.desc.id == track) {
            t.enabled = enabled;
        }
    }

    fn keyframe_times(&self, track: u32) -> Option<Vec<MediaTime>> {
        let v: Vec<MediaTime> = self
            .cues
            .iter()
            .filter(|c| c.track == track as u64)
            .map(|c| self.ticks_to_time(c.time as i64))
            .collect();
        (!v.is_empty()).then_some(v)
    }
}

/// Test-only Matroska writer.
#[cfg(test)]
pub(crate) mod writer {
    pub fn id_bytes(id: u32) -> Vec<u8> {
        let b = id.to_be_bytes();
        let skip = b.iter().position(|&x| x != 0).unwrap_or(3);
        b[skip..].to_vec()
    }
    pub fn el(id: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = id_bytes(id);
        // 8-byte size vint.
        v.push(0x01);
        v.extend_from_slice(&(payload.len() as u64).to_be_bytes()[1..]);
        v.extend_from_slice(payload);
        v
    }
    pub fn el_uint(id: u32, v: u64) -> Vec<u8> {
        el(id, &v.to_be_bytes())
    }
    pub fn el_str(id: u32, s: &str) -> Vec<u8> {
        el(id, s.as_bytes())
    }
    pub fn el_f64(id: u32, v: f64) -> Vec<u8> {
        el(id, &v.to_be_bytes())
    }
    /// SimpleBlock payload for track `n` (< 127).
    pub fn simple_block(track: u8, rel: i16, key: bool, lacing: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0x80 | track];
        v.extend_from_slice(&rel.to_be_bytes());
        v.push(if key { 0x80 } else { 0 } | (lacing << 1));
        v.extend_from_slice(body);
        el(super::SIMPLE_BLOCK, &v)
    }
}

#[cfg(test)]
mod tests {
    use super::writer::*;
    use super::*;
    use fp_core::{Projection, StereoMode};
    use std::io::Cursor;

    fn build(with_cues: bool) -> Vec<u8> {
        let ebml = el(
            EBML,
            &[el_uint(0x4286, 1), el_str(DOC_TYPE, "webm")].concat(),
        );
        let info = el(
            INFO,
            &[el_uint(TIMECODE_SCALE, 1_000_000), el_f64(DURATION, 4000.0)].concat(),
        );
        let mut equi = vec![0u8; 12];
        equi.extend_from_slice(&0x4000_0000u32.to_be_bytes());
        equi.extend_from_slice(&0x4000_0000u32.to_be_bytes());
        let video = el(
            VIDEO,
            &[
                el_uint(PIXEL_WIDTH, 5760),
                el_uint(PIXEL_HEIGHT, 2880),
                el_uint(STEREO_MODE, 1),
                el(
                    COLOUR,
                    &[
                        el_uint(BITS_PER_CHANNEL, 10),
                        el_uint(TRANSFER_CHARACTERISTICS, 18),
                    ]
                    .concat(),
                ),
                el(
                    PROJECTION,
                    &[el_uint(PROJECTION_TYPE, 1), el(PROJECTION_PRIVATE, &equi)].concat(),
                ),
            ]
            .concat(),
        );
        let t1 = el(
            TRACK_ENTRY,
            &[
                el_uint(TRACK_NUMBER, 1),
                el_uint(TRACK_TYPE, 1),
                el_str(CODEC_ID, "V_VP9"),
                el_uint(DEFAULT_DURATION, 40_000_000),
                video,
            ]
            .concat(),
        );
        let t2 = el(
            TRACK_ENTRY,
            &[
                el_uint(TRACK_NUMBER, 2),
                el_uint(TRACK_TYPE, 2),
                el_str(CODEC_ID, "A_OPUS"),
                el_str(LANGUAGE, "eng"),
                el_uint(DEFAULT_DURATION, 20_000_000),
                el(
                    AUDIO,
                    &[el_f64(SAMPLING_FREQUENCY, 48_000.0), el_uint(CHANNELS, 2)].concat(),
                ),
                el(
                    CONTENT_ENCODINGS,
                    &el(
                        CONTENT_ENCODING,
                        &el(
                            CONTENT_COMPRESSION,
                            &[
                                el_uint(CONTENT_COMP_ALGO, 3),
                                el(CONTENT_COMP_SETTINGS, &[0xfc]),
                            ]
                            .concat(),
                        ),
                    ),
                ),
            ]
            .concat(),
        );
        let t3 = el(
            TRACK_ENTRY,
            &[
                el_uint(TRACK_NUMBER, 3),
                el_uint(TRACK_TYPE, 0x11),
                el_str(CODEC_ID, "S_TEXT/UTF8"),
                el_str(NAME, "English"),
            ]
            .concat(),
        );
        let tracks = el(TRACKS, &[t1, t2, t3].concat());
        let chapters = el(
            CHAPTERS,
            &el(
                EDITION_ENTRY,
                &[
                    el(
                        CHAPTER_ATOM,
                        &[
                            el_uint(CHAPTER_TIME_START, 0),
                            el(CHAPTER_DISPLAY, &el_str(CHAP_STRING, "Start")),
                        ]
                        .concat(),
                    ),
                    el(
                        CHAPTER_ATOM,
                        &[
                            el_uint(CHAPTER_TIME_START, 2_000_000_000),
                            el(CHAPTER_DISPLAY, &el_str(CHAP_STRING, "Two")),
                        ]
                        .concat(),
                    ),
                ]
                .concat(),
            ),
        );
        // 4 clusters of 1 s: 25 video frames each, keyframe at start of each
        // cluster; one Xiph-laced pair of audio frames per 40 ms.
        let mut clusters = Vec::new();
        for c in 0..4u64 {
            let mut body = el_uint(TIMECODE, c * 1000);
            for f in 0..25i16 {
                let key = f == 0;
                if f == 3 {
                    // A BlockGroup with a ReferenceBlock (non-key).
                    let mut b = vec![0x81];
                    b.extend_from_slice(&(f * 40).to_be_bytes());
                    b.push(0);
                    b.extend_from_slice(&[c as u8, f as u8]);
                    body.extend(el(
                        BLOCK_GROUP,
                        &[el(BLOCK, &b), el(REFERENCE_BLOCK, &[0xd8])].concat(),
                    ));
                } else {
                    body.extend(simple_block(1, f * 40, key, 0, &[c as u8, f as u8]));
                }
                // Audio: two 20 ms frames, Xiph-laced, sizes 3 and 2.
                body.extend(simple_block(
                    2,
                    f * 40,
                    true,
                    1,
                    &[1, 3, 0xa1, 0xa2, 0xa3, 0xb1, 0xb2],
                ));
            }
            if c == 0 {
                body.extend(simple_block(3, 500, true, 0, b"Hello"));
            }
            clusters.push(el(CLUSTER, &body));
        }
        // Segment children: Info, Tracks, Chapters, [Cues], clusters.
        let pre = [info, tracks, chapters].concat();
        let mut cues_el = Vec::new();
        if with_cues {
            // Two passes: positions depend on the Cues size, which is fixed
            // since all sizes are 8-byte vints.
            let make = |positions: &[u64]| {
                let pts: Vec<u8> = positions
                    .iter()
                    .enumerate()
                    .flat_map(|(i, &p)| {
                        el(
                            CUE_POINT,
                            &[
                                el_uint(CUE_TIME, i as u64 * 1000),
                                el(
                                    CUE_TRACK_POSITIONS,
                                    &[el_uint(CUE_TRACK, 1), el_uint(CUE_CLUSTER_POSITION, p)]
                                        .concat(),
                                ),
                            ]
                            .concat(),
                        )
                    })
                    .collect();
                el(CUES, &pts)
            };
            let len = make(&[0, 0, 0, 0]).len();
            let mut p = (pre.len() + len) as u64;
            let mut positions = Vec::new();
            for c in &clusters {
                positions.push(p);
                p += c.len() as u64;
            }
            cues_el = make(&positions);
        }
        let seg_body = [pre, cues_el, clusters.concat()].concat();
        [ebml, el(SEGMENT, &seg_body)].concat()
    }

    #[test]
    fn header_and_metadata() {
        let d = MkvDemuxer::open(Box::new(Cursor::new(build(true)))).unwrap();
        assert_eq!(d.format_name(), "webm");
        let info = d.media_info();
        assert_eq!(info.duration, Some(MediaTime::from_secs_f64(4.0)));
        let v = &info.video[0];
        assert_eq!(
            (v.width, v.height, v.bit_depth, v.fps),
            (5760, 2880, 10, 25.0)
        );
        assert_eq!(v.transfer, fp_core::ColorTransfer::Hlg);
        assert_eq!(v.signalled_projection, Some(Projection::EQUIRECT_180));
        assert_eq!(v.signalled_stereo, Some(StereoMode::Sbs));
        assert_eq!(info.audio[0].language.as_deref(), Some("eng"));
        assert_eq!(info.subtitles[0].format, "srt");
        assert_eq!(info.chapters.len(), 2);
        assert_eq!(info.chapters[1].start, MediaTime::from_secs_f64(2.0));
        assert_eq!(d.keyframe_times(1).unwrap().len(), 4);
    }

    #[test]
    fn packets_lacing_and_keyframes() {
        let mut d = MkvDemuxer::open(Box::new(Cursor::new(build(true)))).unwrap();
        let pkts: Vec<Packet> = std::iter::from_fn(|| d.read_packet().unwrap()).collect();
        let video: Vec<&Packet> = pkts.iter().filter(|p| p.track == 1).collect();
        let audio: Vec<&Packet> = pkts.iter().filter(|p| p.track == 2).collect();
        assert_eq!(video.len(), 100);
        assert_eq!(audio.len(), 200);
        assert!(
            video[0].keyframe && !video[1].keyframe && !video[3].keyframe && video[25].keyframe
        );
        assert_eq!(video[26].pts, MediaTime::from_millis(1040));
        assert_eq!(video[3].data, vec![0, 3]);
        // Header stripping prefix restored, laced frames get DefaultDuration steps.
        assert_eq!(audio[0].data, vec![0xfc, 0xa1, 0xa2, 0xa3]);
        assert_eq!(audio[1].data, vec![0xfc, 0xb1, 0xb2]);
        assert_eq!(audio[1].pts, MediaTime::from_millis(20));
        let sub = pkts.iter().find(|p| p.track == 3).unwrap();
        assert_eq!(sub.data, b"Hello");
        assert_eq!(sub.pts, MediaTime::from_millis(500));
    }

    #[test]
    fn seek_with_and_without_cues() {
        for cues in [true, false] {
            let mut d = MkvDemuxer::open(Box::new(Cursor::new(build(cues)))).unwrap();
            let k = d.seek(MediaTime::from_secs_f64(2.5)).unwrap();
            assert_eq!(k, MediaTime::from_secs_f64(2.0), "cues={cues}");
            let p = d.read_packet().unwrap().unwrap();
            assert!(p.track == 1 && p.keyframe && p.pts == k);
            let k = d.seek(MediaTime::from_secs_f64(0.3)).unwrap();
            assert_eq!(k, MediaTime::ZERO);
        }
    }

    #[test]
    fn ebml_lacing() {
        // 3 frames: sizes 4, 6 (diff +2), rest.
        let mut data = vec![2u8];
        data.push(0x84); // first size 4
        data.push(0xbf + 2); // signed diff +2 (bias 63)
        data.extend_from_slice(&[1; 4]);
        data.extend_from_slice(&[2; 6]);
        data.extend_from_slice(&[3; 5]);
        let f = unlace(&data, 3).unwrap();
        assert_eq!(f.iter().map(|x| x.len()).collect::<Vec<_>>(), vec![4, 6, 5]);
        // Fixed lacing.
        let data = [1u8, 9, 9, 8, 8];
        let f = unlace(&data, 2).unwrap();
        assert_eq!(f, vec![&[9, 9][..], &[8, 8][..]]);
        assert!(unlace(&[5u8, 255], 1).is_err());
    }

    #[test]
    fn vint_and_ints() {
        assert_eq!(vint(&[0x81], false), Some((1, 1, false)));
        assert_eq!(vint(&[0x40, 0x02], false), Some((2, 2, false)));
        assert_eq!(vint(&[0xff], false), Some((127, 1, true)));
        assert_eq!(
            vint(&[0x1a, 0x45, 0xdf, 0xa3], true).unwrap().0,
            EBML as u64
        );
        assert_eq!(uint(&[0x01, 0x00]), 256);
    }
}
