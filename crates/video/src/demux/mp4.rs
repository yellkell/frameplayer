//! Pure-Rust ISO-BMFF / QuickTime demuxer.
//!
//! Parses `moov` (`mvhd`, `trak`/`tkhd`/`edts`/`elst`/`mdia`/`mdhd`/`hdlr`/
//! `minf`/`stbl` with `stsd`/`stts`/`ctts`/`stsc`/`stsz`/`stz2`/`stco`/`co64`/
//! `stss`), fragmented files (`mvex`/`trex`, `moof`/`traf`/`tfhd`/`tfdt`/
//! `trun`), Nero (`chpl`) and QuickTime (`tref/chap`) chapters, codec
//! configuration (`avcC`, `hvcC`, `av1C`, `vpcC`, `esds`, `dOps`), colour
//! (`colr` nclx → PQ/HLG), spherical metadata (v1 `uuid`, v2 `st3d`/`sv3d`)
//! and spatial audio (`SA3D`).
//!
//! The whole sample table is expanded into memory at open (≈32 bytes per
//! sample), which makes seeking and keyframe lookup trivial. Packets are
//! returned in timestamp order across enabled tracks.

use crate::bytes::{find_box, iter_boxes, BoxRef, Bytes};
use crate::codec::{bit_depth_from_config, Av1Config};
use crate::demux::Demuxer;
use crate::error::{Result, VideoError};
use crate::input::{BufferedInput, MediaInput};
use crate::packet::{
    media_info_from_tracks, AudioParams, CodecId, Packet, TrackDesc, TrackKind, VideoParams,
};
use crate::spherical::{
    parse_spherical_v1_xml, parse_st3d, parse_sv3d, SphericalInfo, SPHERICAL_V1_UUID,
};
use fp_core::media::{Chapter, ColorTransfer};
use fp_core::{MediaInfo, MediaTime};
use std::io::{Read, Seek, SeekFrom};

#[derive(Debug, Clone, Copy)]
struct Sample {
    offset: u64,
    size: u32,
    duration: u32,
    dts: i64,
    cts: i32,
    sync: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct Trex {
    duration: u32,
    size: u32,
    flags: u32,
}

#[derive(Debug)]
struct Mp4Track {
    desc: TrackDesc,
    timescale: u32,
    samples: Vec<Sample>,
    cursor: usize,
    enabled: bool,
    /// Media time of the first edit (subtracted from timestamps).
    edit_media_time: i64,
    /// Initial empty edit (µs, added to timestamps).
    edit_delay_us: i64,
    trex: Trex,
    chapter_refs: Vec<u32>,
}

impl Mp4Track {
    fn to_us(&self, t: i64) -> MediaTime {
        MediaTime::from_timebase(t - self.edit_media_time, 1, self.timescale as i32)
            + MediaTime(self.edit_delay_us)
    }
    fn pts(&self, s: &Sample) -> MediaTime {
        self.to_us(s.dts + s.cts as i64)
    }
    fn dts(&self, s: &Sample) -> MediaTime {
        self.to_us(s.dts)
    }
}

pub struct Mp4Demuxer {
    input: BufferedInput<Box<dyn MediaInput>>,
    tracks: Vec<Mp4Track>,
    descs: Vec<TrackDesc>,
    info: MediaInfo,
    brand: String,
}

/// Read a top-level box header at the current position: `(kind, header_len, total_size)`.
fn read_box_header(
    r: &mut dyn MediaInput,
    file_size: Option<u64>,
    at: u64,
) -> Result<Option<([u8; 4], u64, u64)>> {
    let mut h = [0u8; 8];
    let n = super::read_up_to(r, &mut h)?;
    if n < 8 {
        return Ok(None);
    }
    let size32 = u32::from_be_bytes(h[..4].try_into().unwrap()) as u64;
    let kind: [u8; 4] = h[4..8].try_into().unwrap();
    let (hdr, size) = match size32 {
        1 => {
            let mut l = [0u8; 8];
            r.read_exact(&mut l)?;
            (16, u64::from_be_bytes(l))
        }
        0 => (8, file_size.map(|s| s - at).unwrap_or(u64::MAX)),
        s => (8, s),
    };
    if size < hdr {
        return Err(VideoError::invalid(format!(
            "box {} with size {size}",
            String::from_utf8_lossy(&kind)
        )));
    }
    Ok(Some((kind, hdr, size)))
}

fn fullbox(payload: &[u8]) -> Result<(u8, u32, Bytes<'_>)> {
    let mut b = Bytes::new(payload);
    let v = b.u8()?;
    let f = b.u24()?;
    Ok((v, f, b))
}

fn lang_from_mdhd(code: u16) -> Option<String> {
    if code == 0 || code == 0x7fff || code == 0x55c4 {
        return None; // unset / "und"
    }
    let c = |s: u16| (((code >> s) & 0x1f) as u8 + 0x60) as char;
    Some([c(10), c(5), c(0)].iter().collect())
}

impl Mp4Demuxer {
    pub fn open(input: Box<dyn MediaInput>) -> Result<Self> {
        let mut input = BufferedInput::new(input);
        let file_size = input.size();
        input.seek(SeekFrom::Start(0))?;
        let mut pos = 0u64;
        let mut moov: Option<Vec<u8>> = None;
        let mut moofs: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut brand = String::from("mp4");
        let mut saw_mdat = false;
        loop {
            input.seek(SeekFrom::Start(pos))?;
            let Some((kind, hdr, size)) = read_box_header(&mut input, file_size, pos)? else {
                break;
            };
            match &kind {
                b"ftyp" => {
                    let mut b = vec![0u8; (size - hdr).min(64) as usize];
                    input.read_exact(&mut b)?;
                    if b.len() >= 4 {
                        brand = String::from_utf8_lossy(&b[..4]).trim().to_string();
                    }
                }
                b"moov" => {
                    let mut b = vec![0u8; (size - hdr) as usize];
                    input.read_exact(&mut b)?;
                    moov = Some(b);
                }
                b"moof" => {
                    let mut b = vec![0u8; (size - hdr) as usize];
                    input.read_exact(&mut b)?;
                    moofs.push((pos, b));
                }
                b"mdat" => saw_mdat = true,
                _ => {}
            }
            if size == u64::MAX {
                break;
            }
            pos += size;
            if file_size.is_some_and(|s| pos >= s) {
                break;
            }
        }
        let moov = moov.ok_or_else(|| VideoError::invalid("no moov box"))?;
        if !saw_mdat && moofs.is_empty() {
            tracing::warn!("mp4 without mdat");
        }
        let (mut tracks, movie_duration, mut chapters) = parse_moov(&moov)?;
        for (moof_pos, moof) in &moofs {
            parse_moof(&mut tracks, *moof_pos, moof)?;
        }
        // Derive fps / durations from the sample tables.
        for t in &mut tracks {
            if let (Some(first), Some(last)) =
                (t.samples.first().copied(), t.samples.last().copied())
            {
                let span = (last.dts + last.duration as i64 - first.dts).max(1);
                let dur = MediaTime::from_timebase(span, 1, t.timescale as i32);
                if t.desc.duration.is_none_or(|d| d.0 <= 0) {
                    t.desc.duration = Some(dur);
                }
                if let Some(v) = &mut t.desc.video {
                    v.fps = t.samples.len() as f64 / dur.as_secs_f64().max(1e-9);
                    v.fps = (v.fps * 1000.0).round() / 1000.0;
                }
            }
        }
        // QuickTime chapter tracks.
        let chap_ids: Vec<u32> = tracks.iter().flat_map(|t| t.chapter_refs.clone()).collect();
        if chapters.is_empty() && !chap_ids.is_empty() {
            for t in tracks.iter().filter(|t| chap_ids.contains(&t.desc.id)) {
                for s in &t.samples {
                    if let Ok(data) = crate::input::read_at(&mut input, s.offset, s.size as usize) {
                        if data.len() >= 2 {
                            let len = u16::from_be_bytes([data[0], data[1]]) as usize;
                            let title =
                                String::from_utf8_lossy(&data[2..(2 + len).min(data.len())])
                                    .into_owned();
                            chapters.push(Chapter {
                                start: t.pts(s),
                                title,
                            });
                        }
                    }
                }
            }
        }
        tracks.retain(|t| !chap_ids.contains(&t.desc.id));
        chapters.sort_by_key(|c| c.start);
        let descs: Vec<TrackDesc> = tracks.iter().map(|t| t.desc.clone()).collect();
        let duration = movie_duration
            .filter(|d| d.0 > 0)
            .or_else(|| descs.iter().filter_map(|t| t.duration).max());
        let info = media_info_from_tracks(&format!("mp4:{brand}"), duration, &descs, chapters);
        Ok(Mp4Demuxer {
            input,
            tracks,
            descs,
            info,
            brand,
        })
    }

    pub fn brand(&self) -> &str {
        &self.brand
    }

    fn primary_video(&self) -> Option<usize> {
        self.tracks
            .iter()
            .position(|t| t.desc.kind == TrackKind::Video && t.enabled && !t.samples.is_empty())
    }
}

fn parse_moov(moov: &[u8]) -> Result<(Vec<Mp4Track>, Option<MediaTime>, Vec<Chapter>)> {
    let mut movie_timescale = 1000u32;
    let mut duration = None;
    if let Some(mvhd) = find_box(moov, b"mvhd") {
        let (v, _, mut b) = fullbox(mvhd)?;
        let (ts, d) = if v == 1 {
            b.skip(16)?;
            (b.u32()?, b.u64()?)
        } else {
            b.skip(8)?;
            (b.u32()?, b.u32()? as u64)
        };
        movie_timescale = ts.max(1);
        if d != u64::MAX && d != u32::MAX as u64 {
            duration = Some(MediaTime::from_timebase(
                d as i64,
                1,
                movie_timescale as i32,
            ));
        }
    }
    let mut trex = Vec::new();
    if let Some(mvex) = find_box(moov, b"mvex") {
        for b in iter_boxes(mvex).filter(|b| &b.kind == b"trex") {
            let (_, _, mut r) = fullbox(b.payload)?;
            let id = r.u32()?;
            let _sdi = r.u32()?;
            trex.push((
                id,
                Trex {
                    duration: r.u32()?,
                    size: r.u32()?,
                    flags: r.u32()?,
                },
            ));
        }
    }
    let mut tracks = Vec::new();
    for trak in iter_boxes(moov).filter(|b| &b.kind == b"trak") {
        match parse_trak(trak.payload, movie_timescale) {
            Ok(Some(mut t)) => {
                if let Some((_, x)) = trex.iter().find(|(id, _)| *id == t.desc.id) {
                    t.trex = *x;
                }
                tracks.push(t);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("skipping unparsable track: {e}"),
        }
    }
    let mut chapters = Vec::new();
    if let Some(udta) = find_box(moov, b"udta") {
        if let Some(chpl) = find_box(udta, b"chpl") {
            chapters = parse_chpl(chpl).unwrap_or_default();
        }
    }
    Ok((tracks, duration, chapters))
}

fn parse_chpl(p: &[u8]) -> Result<Vec<Chapter>> {
    let (v, _, mut b) = fullbox(p)?;
    if v != 0 {
        b.skip(4)?;
    }
    let n = b.u8()?;
    let mut out = Vec::new();
    for _ in 0..n {
        let start = b.u64()? as i64; // 100 ns units
        let len = b.u8()? as usize;
        let title = String::from_utf8_lossy(b.take(len)?).into_owned();
        out.push(Chapter {
            start: MediaTime(start / 10),
            title,
        });
    }
    Ok(out)
}

fn parse_trak(trak: &[u8], movie_timescale: u32) -> Result<Option<Mp4Track>> {
    let tkhd = find_box(trak, b"tkhd").ok_or_else(|| VideoError::invalid("trak without tkhd"))?;
    let (v, _flags, mut b) = fullbox(tkhd)?;
    let id = if v == 1 {
        b.skip(16)?;
        b.u32()?
    } else {
        b.skip(8)?;
        b.u32()?
    };
    let mdia = find_box(trak, b"mdia").ok_or_else(|| VideoError::invalid("trak without mdia"))?;
    let hdlr = find_box(mdia, b"hdlr").ok_or_else(|| VideoError::invalid("mdia without hdlr"))?;
    let handler: [u8; 4] = hdlr
        .get(8..12)
        .and_then(|s| s.try_into().ok())
        .unwrap_or(*b"????");
    let kind = match &handler {
        b"vide" => TrackKind::Video,
        b"soun" => TrackKind::Audio,
        b"subt" | b"text" | b"sbtl" => TrackKind::Subtitle,
        _ => return Ok(None),
    };
    let mdhd = find_box(mdia, b"mdhd").ok_or_else(|| VideoError::invalid("mdia without mdhd"))?;
    let (v, _, mut b) = fullbox(mdhd)?;
    let (timescale, mdur) = if v == 1 {
        b.skip(16)?;
        (b.u32()?, b.u64()?)
    } else {
        b.skip(8)?;
        (b.u32()?, b.u32()? as u64)
    };
    let timescale = timescale.max(1);
    let language = b.u16().ok().and_then(lang_from_mdhd);
    let minf = find_box(mdia, b"minf").ok_or_else(|| VideoError::invalid("mdia without minf"))?;
    let stbl = find_box(minf, b"stbl").ok_or_else(|| VideoError::invalid("minf without stbl"))?;
    let stsd = find_box(stbl, b"stsd").ok_or_else(|| VideoError::invalid("stbl without stsd"))?;

    // Track-level spherical v1 (uuid box directly in trak).
    let mut sph = SphericalInfo::default();
    for bx in iter_boxes(trak) {
        if bx.uuid == Some(SPHERICAL_V1_UUID) {
            sph.merge(parse_spherical_v1_xml(&String::from_utf8_lossy(bx.payload)));
        }
    }

    let mut desc = parse_stsd(stsd, id, kind, &mut sph)?;
    desc.language = language;
    if mdur != u64::MAX && mdur != u32::MAX as u64 && mdur > 0 {
        desc.duration = Some(MediaTime::from_timebase(mdur as i64, 1, timescale as i32));
    }
    if let Some(udta) = find_box(trak, b"udta") {
        if let Some(name) = find_box(udta, b"name") {
            desc.name = Some(
                String::from_utf8_lossy(name)
                    .trim_end_matches('\0')
                    .to_string(),
            );
        }
    }
    if let Some(v) = &mut desc.video {
        v.projection = sph.projection.clone();
        v.stereo = sph.stereo;
    }

    // Edit list.
    let mut edit_media_time = 0i64;
    let mut edit_delay_us = 0i64;
    if let Some(elst) = find_box(trak, b"edts").and_then(|e| find_box(e, b"elst")) {
        let (v, _, mut b) = fullbox(elst)?;
        let n = b.u32()?;
        for _ in 0..n {
            let (seg, mt) = if v == 1 {
                (b.u64()?, b.i64()?)
            } else {
                (b.u32()? as u64, b.i32()? as i64)
            };
            b.skip(4)?;
            if mt == -1 {
                edit_delay_us += MediaTime::from_timebase(seg as i64, 1, movie_timescale as i32).0;
            } else {
                edit_media_time = mt;
                break;
            }
        }
    }
    let mut chapter_refs = Vec::new();
    if let Some(tref) = find_box(trak, b"tref") {
        if let Some(chap) = find_box(tref, b"chap") {
            let mut b = Bytes::new(chap);
            while let Ok(id) = b.u32() {
                chapter_refs.push(id);
            }
        }
    }
    let samples = build_samples(stbl)?;
    Ok(Some(Mp4Track {
        desc,
        timescale,
        samples,
        cursor: 0,
        enabled: true,
        edit_media_time,
        edit_delay_us,
        trex: Trex::default(),
        chapter_refs,
    }))
}

fn parse_stsd(stsd: &[u8], id: u32, kind: TrackKind, sph: &mut SphericalInfo) -> Result<TrackDesc> {
    let (_, _, mut b) = fullbox(stsd)?;
    let _count = b.u32()?;
    let entry: BoxRef = iter_boxes(b.rest())
        .next()
        .ok_or_else(|| VideoError::invalid("empty stsd"))?;
    let fourcc = entry.kind;
    let p = entry.payload;
    match kind {
        TrackKind::Video => {
            let codec = match &fourcc {
                b"avc1" | b"avc3" => CodecId::H264,
                b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => CodecId::Hevc,
                b"av01" => CodecId::Av1,
                b"vp09" => CodecId::Vp9,
                b"vp08" => CodecId::Vp8,
                other => CodecId::Unknown(String::from_utf8_lossy(other).into()),
            };
            let mut d = TrackDesc::new(id, kind, codec.clone());
            let mut b = Bytes::new(p);
            b.skip(24)?;
            let width = b.u16()? as u32;
            let height = b.u16()? as u32;
            let children = p.get(78..).unwrap_or(&[]);
            let mut v = VideoParams {
                width,
                height,
                bit_depth: 8,
                ..Default::default()
            };
            for c in iter_boxes(children) {
                match &c.kind {
                    b"avcC" | b"hvcC" | b"av1C" => d.codec_private = c.payload.to_vec(),
                    b"vpcC" => d.codec_private = c.payload.to_vec(),
                    b"colr" if c.payload.len() >= 10 && &c.payload[..4] == b"nclx" => {
                        let tc = u16::from_be_bytes([c.payload[6], c.payload[7]]);
                        v.transfer = transfer_from_code(tc as u64);
                    }
                    b"st3d" => sph.stereo = sph.stereo.or(parse_st3d(c.payload)),
                    b"sv3d" => {
                        let s = parse_sv3d(c.payload);
                        if s.projection.is_some() {
                            let stereo = sph.stereo;
                            *sph = s;
                            sph.stereo = stereo;
                        }
                    }
                    b"uuid" if c.uuid == Some(SPHERICAL_V1_UUID) => {
                        sph.merge(parse_spherical_v1_xml(&String::from_utf8_lossy(c.payload)));
                    }
                    _ => {}
                }
            }
            if let Some(bd) = bit_depth_from_config(&codec, &d.codec_private) {
                v.bit_depth = bd;
            }
            if codec == CodecId::Vp9 {
                if let Ok(c) = crate::codec::Vp9Config::parse(&d.codec_private) {
                    if v.transfer == ColorTransfer::Sdr {
                        v.transfer = transfer_from_code(c.transfer_characteristics as u64);
                    }
                }
            }
            if codec == CodecId::Av1 {
                if let Ok(c) = Av1Config::parse(&d.codec_private) {
                    v.bit_depth = c.bit_depth;
                }
            }
            d.video = Some(v);
            Ok(d)
        }
        TrackKind::Audio => parse_audio_entry(id, fourcc, p),
        TrackKind::Subtitle => {
            let codec = match &fourcc {
                b"tx3g" | b"text" => CodecId::MovText,
                b"wvtt" => CodecId::WebVtt,
                other => CodecId::Unknown(String::from_utf8_lossy(other).into()),
            };
            Ok(TrackDesc::new(id, kind, codec))
        }
        TrackKind::Other => Ok(TrackDesc::new(id, kind, CodecId::Unknown("other".into()))),
    }
}

pub(crate) fn transfer_from_code(tc: u64) -> ColorTransfer {
    match tc {
        16 => ColorTransfer::Pq,
        18 => ColorTransfer::Hlg,
        _ => ColorTransfer::Sdr,
    }
}

fn parse_audio_entry(id: u32, fourcc: [u8; 4], p: &[u8]) -> Result<TrackDesc> {
    let mut b = Bytes::new(p);
    b.skip(8)?;
    let version = b.u16()?;
    b.skip(6)?;
    let mut channels = b.u16()?;
    let mut bits = b.u16()?;
    b.skip(4)?;
    let mut rate = b.u32()? >> 16;
    let mut lpcm_flags = 0u32;
    let children_at = match version {
        1 => 28 + 16,
        2 => {
            // QuickTime v2: sizeOfStructOnly, rate (f64), channels, …
            let mut v2 = Bytes::new(&p[28..]);
            let _struct_size = v2.u32()?;
            rate = f64::from_bits(v2.u64()?) as u32;
            channels = v2.u32()? as u16;
            let _ = v2.u32()?;
            bits = v2.u32()? as u16;
            lpcm_flags = v2.u32()?;
            28 + 36
        }
        _ => 28,
    };
    let children = p.get(children_at..).unwrap_or(&[]);
    let mut little_endian_hint = false;
    let codec = match &fourcc {
        b"mp4a" => CodecId::Aac,
        b"Opus" => CodecId::Opus,
        b"ac-3" => CodecId::Ac3,
        b"ec-3" => CodecId::Eac3,
        b"fLaC" => CodecId::Flac,
        b".mp3" => CodecId::Mp3,
        b"sowt" => CodecId::Pcm {
            bits: 16,
            float: false,
            big_endian: false,
        },
        b"twos" => CodecId::Pcm {
            bits: bits.max(8) as u8,
            float: false,
            big_endian: true,
        },
        b"in24" => CodecId::Pcm {
            bits: 24,
            float: false,
            big_endian: true,
        },
        b"in32" => CodecId::Pcm {
            bits: 32,
            float: false,
            big_endian: true,
        },
        b"fl32" => CodecId::Pcm {
            bits: 32,
            float: true,
            big_endian: true,
        },
        b"fl64" => CodecId::Pcm {
            bits: 64,
            float: true,
            big_endian: true,
        },
        b"lpcm" => CodecId::Pcm {
            bits: bits as u8,
            float: lpcm_flags & 1 != 0,
            big_endian: lpcm_flags & 2 != 0,
        },
        b"ipcm" | b"fpcm" => CodecId::Pcm {
            bits: bits as u8,
            float: &fourcc == b"fpcm",
            big_endian: true,
        },
        other => CodecId::Unknown(String::from_utf8_lossy(other).into()),
    };
    let mut d = TrackDesc::new(id, TrackKind::Audio, codec);
    let mut ap = AudioParams {
        sample_rate: rate,
        channels,
        bits_per_sample: bits,
        ambisonic: None,
    };
    for c in iter_boxes(children) {
        match &c.kind {
            b"esds" => {
                if let Some((oti, asc)) = parse_esds(c.payload) {
                    if matches!(oti, 0x69 | 0x6b) {
                        d.codec = CodecId::Mp3;
                    }
                    if let Some((sr, ch)) = aac_asc_info(&asc) {
                        if sr > 0 {
                            ap.sample_rate = sr;
                        }
                        if ch > 0 {
                            ap.channels = ch;
                        }
                    }
                    d.codec_private = asc;
                }
            }
            b"dOps" => {
                if let Some(head) = dops_to_opushead(c.payload) {
                    ap.channels = head[9] as u16;
                    ap.sample_rate = 48_000;
                    d.codec_private = head;
                }
            }
            b"SA3D" => {
                // version, ambisonic_type, order(u32), ordering, normalization, …
                let mut s = Bytes::new(c.payload);
                if let (Ok(_v), Ok(_t), Ok(order), Ok(ordering), Ok(norm)) =
                    (s.u8(), s.u8(), s.u32(), s.u8(), s.u8())
                {
                    // ordering 0 = ACN, normalization 0 = SN3D → AmbiX.
                    ap.ambisonic = Some((order.min(255) as u8, !(ordering == 0 && norm == 0)));
                }
            }
            b"enda" if c.payload.len() >= 2 => little_endian_hint = c.payload[1] == 1,
            b"pcmC" if c.payload.len() >= 6 => {
                let le = c.payload[4] & 1 != 0;
                let sz = c.payload[5];
                if let CodecId::Pcm { float, .. } = d.codec {
                    d.codec = CodecId::Pcm {
                        bits: sz,
                        float,
                        big_endian: !le,
                    };
                }
            }
            b"wave" => {
                // QuickTime: codec config nested in `wave`.
                if let Some(esds) = find_box(c.payload, b"esds") {
                    if let Some((_, asc)) = parse_esds(esds) {
                        d.codec_private = asc;
                    }
                }
                if let Some(enda) = find_box(c.payload, b"enda") {
                    little_endian_hint = enda.get(1) == Some(&1);
                }
            }
            _ => {}
        }
    }
    if little_endian_hint {
        if let CodecId::Pcm { bits, float, .. } = d.codec {
            d.codec = CodecId::Pcm {
                bits,
                float,
                big_endian: false,
            };
        }
    }
    d.audio = Some(ap);
    Ok(d)
}

/// `esds` → (objectTypeIndication, DecoderSpecificInfo).
fn parse_esds(p: &[u8]) -> Option<(u8, Vec<u8>)> {
    fn desc_len(b: &mut Bytes) -> Option<usize> {
        let mut len = 0usize;
        for _ in 0..4 {
            let c = b.u8().ok()?;
            len = (len << 7) | (c & 0x7f) as usize;
            if c & 0x80 == 0 {
                break;
            }
        }
        Some(len)
    }
    let mut b = Bytes::new(p.get(4..)?);
    if b.u8().ok()? != 0x03 {
        return None;
    }
    desc_len(&mut b)?;
    b.skip(2).ok()?;
    let flags = b.u8().ok()?;
    if flags & 0x80 != 0 {
        b.skip(2).ok()?;
    }
    if flags & 0x40 != 0 {
        let l = b.u8().ok()? as usize;
        b.skip(l).ok()?;
    }
    if flags & 0x20 != 0 {
        b.skip(2).ok()?;
    }
    if b.u8().ok()? != 0x04 {
        return None;
    }
    desc_len(&mut b)?;
    let oti = b.u8().ok()?;
    b.skip(12).ok()?;
    if b.u8().ok() != Some(0x05) {
        return Some((oti, Vec::new()));
    }
    let l = desc_len(&mut b)?;
    Some((oti, b.take(l).ok()?.to_vec()))
}

/// AudioSpecificConfig → (sample rate, channels).
pub fn aac_asc_info(asc: &[u8]) -> Option<(u32, u16)> {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let mut r = crate::bytes::BitReader::new(asc);
    let mut aot = r.read_bits(5).ok()?;
    if aot == 31 {
        aot = 32 + r.read_bits(6).ok()?;
    }
    let idx = r.read_bits(4).ok()?;
    let rate = if idx == 15 {
        r.read_bits(24).ok()?
    } else {
        *RATES.get(idx as usize)?
    };
    let ch_cfg = r.read_bits(4).ok()?;
    let channels = match ch_cfg {
        1..=6 => ch_cfg as u16,
        7 => 8,
        _ => 0,
    };
    let _ = aot;
    Some((rate, channels))
}

/// `dOps` → Ogg-style `OpusHead` (what decoders and Matroska use).
fn dops_to_opushead(p: &[u8]) -> Option<Vec<u8>> {
    let mut b = Bytes::new(p);
    let _version = b.u8().ok()?;
    let ch = b.u8().ok()?;
    let preskip = b.u16().ok()?;
    let rate = b.u32().ok()?;
    let gain = b.u16().ok()?;
    let family = b.u8().ok()?;
    let mut h = b"OpusHead".to_vec();
    h.push(1);
    h.push(ch);
    h.extend_from_slice(&preskip.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&gain.to_le_bytes());
    h.push(family);
    if family != 0 {
        h.extend_from_slice(b.rest());
    }
    Some(h)
}

fn build_samples(stbl: &[u8]) -> Result<Vec<Sample>> {
    // stts
    let mut durations: Vec<(u32, u32)> = Vec::new();
    if let Some(stts) = find_box(stbl, b"stts") {
        let (_, _, mut b) = fullbox(stts)?;
        let n = b.u32()?;
        for _ in 0..n {
            durations.push((b.u32()?, b.u32()?));
        }
    }
    let mut ctts: Vec<(u32, i32)> = Vec::new();
    if let Some(c) = find_box(stbl, b"ctts") {
        let (_, _, mut b) = fullbox(c)?;
        let n = b.u32()?;
        for _ in 0..n {
            ctts.push((b.u32()?, b.i32()?));
        }
    }
    // Sizes.
    let mut sizes: Vec<u32> = Vec::new();
    if let Some(stsz) = find_box(stbl, b"stsz") {
        let (_, _, mut b) = fullbox(stsz)?;
        let fixed = b.u32()?;
        let n = b.u32()?;
        if fixed != 0 {
            sizes = vec![fixed; n as usize];
        } else {
            sizes.reserve(n as usize);
            for _ in 0..n {
                sizes.push(b.u32()?);
            }
        }
    } else if let Some(stz2) = find_box(stbl, b"stz2") {
        let (_, _, mut b) = fullbox(stz2)?;
        let field = b.u32()? & 0xff;
        let n = b.u32()?;
        for i in 0..n {
            sizes.push(match field {
                4 => {
                    let byte = b.rest().first().copied().unwrap_or(0);
                    if i % 2 == 1 {
                        b.skip(1)?;
                        (byte & 0xf) as u32
                    } else {
                        (byte >> 4) as u32
                    }
                }
                8 => b.u8()? as u32,
                _ => b.u16()? as u32,
            });
        }
    }
    if sizes.is_empty() {
        return Ok(Vec::new());
    }
    // Chunk offsets.
    let mut chunks: Vec<u64> = Vec::new();
    if let Some(stco) = find_box(stbl, b"stco") {
        let (_, _, mut b) = fullbox(stco)?;
        let n = b.u32()?;
        for _ in 0..n {
            chunks.push(b.u32()? as u64);
        }
    } else if let Some(co64) = find_box(stbl, b"co64") {
        let (_, _, mut b) = fullbox(co64)?;
        let n = b.u32()?;
        for _ in 0..n {
            chunks.push(b.u64()?);
        }
    }
    let mut stsc: Vec<(u32, u32)> = Vec::new();
    if let Some(s) = find_box(stbl, b"stsc") {
        let (_, _, mut b) = fullbox(s)?;
        let n = b.u32()?;
        for _ in 0..n {
            let first = b.u32()?;
            let per = b.u32()?;
            let _sdi = b.u32()?;
            stsc.push((first, per));
        }
    }
    let sync: Option<Vec<u32>> = match find_box(stbl, b"stss") {
        Some(s) => {
            let (_, _, mut b) = fullbox(s)?;
            let n = b.u32()?;
            let mut v = Vec::with_capacity(n as usize);
            for _ in 0..n {
                v.push(b.u32()?);
            }
            Some(v)
        }
        None => None,
    };

    let total = sizes.len();
    let mut samples = Vec::with_capacity(total);
    // Offsets via stsc.
    let mut si = 0usize;
    'chunks: for (ci, &chunk_off) in chunks.iter().enumerate() {
        let chunk_no = ci as u32 + 1;
        let per = stsc
            .iter()
            .rev()
            .find(|(first, _)| *first <= chunk_no)
            .map_or(1, |(_, p)| *p);
        let mut off = chunk_off;
        for _ in 0..per {
            if si >= total {
                break 'chunks;
            }
            samples.push(Sample {
                offset: off,
                size: sizes[si],
                duration: 0,
                dts: 0,
                cts: 0,
                sync: sync.is_none(),
            });
            off += sizes[si] as u64;
            si += 1;
        }
    }
    // Timing.
    let mut dts = 0i64;
    let mut it = durations
        .iter()
        .flat_map(|&(n, d)| std::iter::repeat_n(d, n as usize));
    let mut last_d = 0u32;
    for s in samples.iter_mut() {
        let d = it.next().unwrap_or(last_d);
        last_d = d;
        s.dts = dts;
        s.duration = d;
        dts += d as i64;
    }
    let mut it = ctts
        .iter()
        .flat_map(|&(n, o)| std::iter::repeat_n(o, n as usize));
    for s in samples.iter_mut() {
        s.cts = it.next().unwrap_or(0);
    }
    if let Some(sync) = sync {
        for n in sync {
            if let Some(s) = samples.get_mut(n.saturating_sub(1) as usize) {
                s.sync = true;
            }
        }
    }
    Ok(samples)
}

fn parse_moof(tracks: &mut [Mp4Track], moof_pos: u64, moof: &[u8]) -> Result<()> {
    for traf in iter_boxes(moof).filter(|b| &b.kind == b"traf") {
        let tfhd = find_box(traf.payload, b"tfhd")
            .ok_or_else(|| VideoError::invalid("traf without tfhd"))?;
        let (_, flags, mut b) = fullbox(tfhd)?;
        let id = b.u32()?;
        let Some(t) = tracks.iter_mut().find(|t| t.desc.id == id) else {
            continue;
        };
        let mut base = moof_pos;
        if flags & 0x01 != 0 {
            base = b.u64()?;
        }
        if flags & 0x02 != 0 {
            b.u32()?;
        }
        let def_dur = if flags & 0x08 != 0 {
            b.u32()?
        } else {
            t.trex.duration
        };
        let def_size = if flags & 0x10 != 0 {
            b.u32()?
        } else {
            t.trex.size
        };
        let def_flags = if flags & 0x20 != 0 {
            b.u32()?
        } else {
            t.trex.flags
        };
        let mut dts = match find_box(traf.payload, b"tfdt") {
            Some(tfdt) => {
                let (v, _, mut b) = fullbox(tfdt)?;
                if v == 1 {
                    b.u64()? as i64
                } else {
                    b.u32()? as i64
                }
            }
            None => t.samples.last().map_or(0, |s| s.dts + s.duration as i64),
        };
        let mut next_offset: Option<u64> = None;
        for trun in iter_boxes(traf.payload).filter(|b| &b.kind == b"trun") {
            let (_, tf, mut r) = fullbox(trun.payload)?;
            let count = r.u32()?;
            let mut off = match (tf & 0x01 != 0, next_offset) {
                (true, _) => (base as i64 + r.i32()? as i64) as u64,
                (false, Some(o)) => o,
                (false, None) => base,
            };
            let first_flags = if tf & 0x04 != 0 { Some(r.u32()?) } else { None };
            for i in 0..count {
                let dur = if tf & 0x100 != 0 { r.u32()? } else { def_dur };
                let size = if tf & 0x200 != 0 { r.u32()? } else { def_size };
                let mut sflags = if tf & 0x400 != 0 { r.u32()? } else { def_flags };
                if i == 0 {
                    if let Some(f) = first_flags {
                        sflags = f;
                    }
                }
                let cts = if tf & 0x800 != 0 { r.i32()? } else { 0 };
                let non_sync = (sflags >> 16) & 1 != 0;
                t.samples.push(Sample {
                    offset: off,
                    size,
                    duration: dur,
                    dts,
                    cts,
                    sync: !non_sync,
                });
                off += size as u64;
                dts += dur as i64;
            }
            next_offset = Some(off);
        }
    }
    Ok(())
}

impl Demuxer for Mp4Demuxer {
    fn format_name(&self) -> &str {
        "mp4"
    }

    fn tracks(&self) -> &[TrackDesc] {
        &self.descs
    }

    fn media_info(&self) -> &MediaInfo {
        &self.info
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        let next = self
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.enabled && t.cursor < t.samples.len())
            .min_by_key(|(_, t)| {
                let s = &t.samples[t.cursor];
                (t.dts(s), s.offset)
            })
            .map(|(i, _)| i);
        let Some(i) = next else { return Ok(None) };
        let t = &mut self.tracks[i];
        let s = t.samples[t.cursor];
        t.cursor += 1;
        let (pts, dts, duration) = (
            t.pts(&s),
            t.dts(&s),
            MediaTime::from_timebase(s.duration as i64, 1, t.timescale as i32),
        );
        let id = t.desc.id;
        self.input.seek(SeekFrom::Start(s.offset))?;
        let mut data = vec![0u8; s.size as usize];
        self.input.read_exact(&mut data).map_err(|e| {
            VideoError::Invalid(format!("reading sample at {} (+{}): {e}", s.offset, s.size))
        })?;
        Ok(Some(Packet {
            track: id,
            pts,
            dts,
            duration,
            keyframe: s.sync,
            data,
        }))
    }

    fn seek(&mut self, target: MediaTime) -> Result<MediaTime> {
        let key_time = match self.primary_video() {
            Some(vi) => {
                let t = &self.tracks[vi];
                // Last sync sample whose pts ≤ target.
                let mut idx = 0;
                for (i, s) in t.samples.iter().enumerate() {
                    if s.sync && t.pts(s) <= target {
                        idx = i;
                    }
                    if t.dts(s) > target {
                        break;
                    }
                }
                let kt = t.pts(&t.samples[idx]);
                self.tracks[vi].cursor = idx;
                Some((vi, kt))
            }
            None => None,
        };
        let anchor = key_time.map_or(target, |(_, k)| k);
        for (i, t) in self.tracks.iter_mut().enumerate() {
            if key_time.is_some_and(|(vi, _)| vi == i) {
                continue;
            }
            // Last sample starting at or before the anchor.
            let p = t.samples.partition_point(|s| t.pts(s) <= anchor);
            let mut c = p.saturating_sub(1);
            // Keep non-video tracks decodable from a sync sample.
            while c > 0 && !t.samples[c].sync {
                c -= 1;
            }
            t.cursor = c;
        }
        Ok(anchor)
    }

    fn set_track_enabled(&mut self, track: u32, enabled: bool) {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.desc.id == track) {
            t.enabled = enabled;
        }
    }

    fn keyframe_times(&self, track: u32) -> Option<Vec<MediaTime>> {
        let t = self.tracks.iter().find(|t| t.desc.id == track)?;
        Some(
            t.samples
                .iter()
                .filter(|s| s.sync)
                .map(|s| t.pts(s))
                .collect(),
        )
    }
}

/// Test-only MP4 writer used by this crate's tests (demuxer, engine,
/// thumbnailer).
#[cfg(test)]
pub(crate) mod writer {
    pub fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }
    pub fn full(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![
            version,
            (flags >> 16) as u8,
            (flags >> 8) as u8,
            flags as u8,
        ];
        p.extend_from_slice(payload);
        bx(kind, &p)
    }
    fn be32(v: u32) -> [u8; 4] {
        v.to_be_bytes()
    }

    pub struct TrackSpec {
        pub id: u32,
        pub handler: [u8; 4],
        pub entry: Vec<u8>,
        pub timescale: u32,
        /// (data, duration, cts, sync)
        pub samples: Vec<(Vec<u8>, u32, i32, bool)>,
        pub extra_trak: Vec<u8>,
        pub elst_media_time: Option<i32>,
    }

    pub fn video_entry(fourcc: &[u8; 4], w: u16, h: u16, children: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; 24];
        p[7] = 1;
        p.extend_from_slice(&w.to_be_bytes());
        p.extend_from_slice(&h.to_be_bytes());
        p.extend_from_slice(&[0, 0x48, 0, 0, 0, 0x48, 0, 0, 0, 0, 0, 0, 0, 1]);
        p.extend_from_slice(&[0u8; 32]);
        p.extend_from_slice(&[0, 0x18, 0xff, 0xff]);
        assert_eq!(p.len(), 78);
        p.extend_from_slice(children);
        bx(fourcc, &p)
    }

    pub fn audio_entry(
        fourcc: &[u8; 4],
        ch: u16,
        bits: u16,
        rate: u32,
        children: &[u8],
    ) -> Vec<u8> {
        let mut p = vec![0u8; 8];
        p[7] = 1;
        p.extend_from_slice(&[0u8; 8]);
        p.extend_from_slice(&ch.to_be_bytes());
        p.extend_from_slice(&bits.to_be_bytes());
        p.extend_from_slice(&[0u8; 4]);
        p.extend_from_slice(&(rate << 16).to_be_bytes());
        p.extend_from_slice(children);
        bx(fourcc, &p)
    }

    /// Build a progressive MP4 (`ftyp` + `moov` + `mdat`), one chunk per sample.
    pub fn build(tracks: &[TrackSpec], udta: &[u8]) -> Vec<u8> {
        let ftyp = bx(b"ftyp", b"isom\0\0\x02\0isomiso2mp41");
        // Lay out mdat first to know offsets; moov size is computed in two passes.
        let build_moov = |mdat_data_start: u64| -> Vec<u8> {
            let mut moov = full(
                b"mvhd",
                0,
                0,
                &[&[0u8; 8][..], &be32(1000), &be32(0), &[0u8; 80]].concat(),
            );
            let mut off = mdat_data_start;
            for t in tracks {
                let n = t.samples.len() as u32;
                let mut stts = be32(n).to_vec();
                for s in &t.samples {
                    stts.extend_from_slice(&be32(1));
                    stts.extend_from_slice(&be32(s.1));
                }
                let mut ctts = be32(n).to_vec();
                for s in &t.samples {
                    ctts.extend_from_slice(&be32(1));
                    ctts.extend_from_slice(&s.2.to_be_bytes());
                }
                let mut stsz = be32(0).to_vec();
                stsz.extend_from_slice(&be32(n));
                let mut co64 = be32(n).to_vec();
                for s in &t.samples {
                    stsz.extend_from_slice(&be32(s.0.len() as u32));
                    co64.extend_from_slice(&off.to_be_bytes());
                    off += s.0.len() as u64;
                }
                let syncs: Vec<u32> = t
                    .samples
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.3)
                    .map(|(i, _)| i as u32 + 1)
                    .collect();
                let mut stss = be32(syncs.len() as u32).to_vec();
                for s in syncs {
                    stss.extend_from_slice(&be32(s));
                }
                let stbl = [
                    full(b"stsd", 0, 0, &[&be32(1)[..], &t.entry].concat()),
                    full(b"stts", 0, 0, &stts),
                    full(b"ctts", 0, 0, &ctts),
                    full(
                        b"stsc",
                        0,
                        0,
                        &[be32(1), be32(1), be32(1), be32(1)].concat(),
                    ),
                    full(b"stsz", 0, 0, &stsz),
                    full(b"co64", 0, 0, &co64),
                    full(b"stss", 0, 0, &stss),
                ]
                .concat();
                let mdhd = full(
                    b"mdhd",
                    0,
                    0,
                    &[
                        &[0u8; 8][..],
                        &be32(t.timescale),
                        &be32(0),
                        &[0x55, 0xc4, 0, 0],
                    ]
                    .concat(),
                );
                let hdlr = full(
                    b"hdlr",
                    0,
                    0,
                    &[&[0u8; 4][..], &t.handler, &[0u8; 12], b"h\0"].concat(),
                );
                let mdia = bx(
                    b"mdia",
                    &[mdhd, hdlr, bx(b"minf", &bx(b"stbl", &stbl))].concat(),
                );
                let tkhd = full(
                    b"tkhd",
                    0,
                    3,
                    &[&[0u8; 8][..], &be32(t.id), &[0u8; 68]].concat(),
                );
                let edts = t.elst_media_time.map_or(Vec::new(), |mt| {
                    bx(
                        b"edts",
                        &full(
                            b"elst",
                            0,
                            0,
                            &[be32(1), be32(0), mt.to_be_bytes(), be32(0x10000)].concat(),
                        ),
                    )
                });
                moov.extend(bx(
                    b"trak",
                    &[tkhd, edts, mdia, t.extra_trak.clone()].concat(),
                ));
            }
            if !udta.is_empty() {
                moov.extend(bx(b"udta", udta));
            }
            bx(b"moov", &moov)
        };
        let probe = build_moov(0);
        let data_start = (ftyp.len() + probe.len() + 8) as u64;
        let moov = build_moov(data_start);
        assert_eq!(moov.len(), probe.len());
        let mdat_payload: Vec<u8> = tracks
            .iter()
            .flat_map(|t| t.samples.iter().flat_map(|s| s.0.clone()))
            .collect();
        [ftyp, moov, bx(b"mdat", &mdat_payload)].concat()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::writer::*;
    use super::*;
    use crate::codec::annexb::tests::sample_hvcc;
    use fp_core::{Projection, StereoMode};
    use std::io::Cursor;

    fn spherical_children() -> Vec<u8> {
        let mut equi = vec![0u8; 4 + 8];
        equi.extend_from_slice(&0x4000_0000u32.to_be_bytes());
        equi.extend_from_slice(&0x4000_0000u32.to_be_bytes());
        let proj = bx(
            b"proj",
            &[full(b"prhd", 0, 0, &[0u8; 12]), bx(b"equi", &equi)].concat(),
        );
        [
            bx(b"hvcC", &sample_hvcc()),
            full(b"st3d", 0, 0, &[2]),
            bx(b"sv3d", &proj),
        ]
        .concat()
    }

    pub(crate) fn sample_file() -> Vec<u8> {
        // 30 fps video, keyframe every 10 frames, B-frame style cts offsets.
        let video = TrackSpec {
            id: 1,
            handler: *b"vide",
            entry: video_entry(b"hvc1", 3840, 1920, &spherical_children()),
            timescale: 30_000,
            samples: (0..60u32)
                .map(|i| (vec![i as u8; 100 + i as usize], 1001, 2002, i % 10 == 0))
                .collect(),
            extra_trak: Vec::new(),
            elst_media_time: Some(2002),
        };
        let asc = [0x12u8, 0x10]; // AAC-LC 44.1 kHz stereo
        let mut esds = vec![
            0x03, 25, 0, 1, 0, 0x04, 17, 0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x05, 2,
        ];
        esds.extend_from_slice(&asc);
        esds.extend_from_slice(&[0x06, 1, 2]);
        let sa3d = bx(
            b"SA3D",
            &[
                0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 1, 0, 2, 0, 3,
            ],
        );
        let audio = TrackSpec {
            id: 2,
            handler: *b"soun",
            entry: audio_entry(
                b"mp4a",
                4,
                16,
                44_100,
                &[full(b"esds", 0, 0, &esds), sa3d].concat(),
            ),
            timescale: 44_100,
            samples: (0..86u32)
                .map(|_| (vec![0xa0; 50], 1024, 0, true))
                .collect(),
            extra_trak: Vec::new(),
            elst_media_time: None,
        };
        let mut chpl = vec![0u8]; // reserved (version 1)
        chpl.extend_from_slice(&[0, 0, 0]);
        chpl.push(2);
        for (t, name) in [(0u64, "Intro"), (10_000_000u64, "Middle")] {
            chpl.extend_from_slice(&t.to_be_bytes());
            chpl.push(name.len() as u8);
            chpl.extend_from_slice(name.as_bytes());
        }
        let udta = full(b"chpl", 1, 0, &chpl);
        build(&[video, audio], &udta)
    }

    #[test]
    fn parses_tracks_metadata_and_chapters() {
        let d = Mp4Demuxer::open(Box::new(Cursor::new(sample_file()))).unwrap();
        let info = d.media_info();
        assert_eq!(info.video.len(), 1);
        let v = &info.video[0];
        assert_eq!((v.width, v.height, v.bit_depth), (3840, 1920, 10));
        assert_eq!(v.codec, fp_core::Codec::Hevc);
        assert!((v.fps - 29.97).abs() < 0.01, "{}", v.fps);
        assert_eq!(v.signalled_projection, Some(Projection::EQUIRECT_180));
        assert_eq!(v.signalled_stereo, Some(StereoMode::Sbs));
        let a = &info.audio[0];
        assert_eq!(
            (a.channels, a.sample_rate),
            (2, 44_100),
            "ASC overrides entry"
        );
        assert_eq!(a.ambisonic_order, Some(1));
        assert_eq!(d.tracks()[1].codec_private, vec![0x12, 0x10]);
        assert_eq!(info.chapters.len(), 2);
        assert_eq!(info.chapters[1].title, "Middle");
        assert_eq!(info.chapters[1].start, MediaTime::from_secs_f64(1.0));
    }

    #[test]
    fn packets_have_timestamps_and_keyframes() {
        let mut d = Mp4Demuxer::open(Box::new(Cursor::new(sample_file()))).unwrap();
        let mut video = Vec::new();
        let mut audio = 0;
        let mut last_dts = MediaTime(i64::MIN);
        while let Some(p) = d.read_packet().unwrap() {
            assert!(p.dts >= last_dts, "interleaved in dts order");
            last_dts = p.dts;
            if p.track == 1 {
                video.push(p);
            } else {
                audio += 1;
            }
        }
        assert_eq!(video.len(), 60);
        assert_eq!(audio, 86);
        // Edit list cancels the composition offset: first pts is 0.
        assert_eq!(video[0].pts, MediaTime::ZERO);
        assert_eq!(video[1].pts, MediaTime::from_timebase(1001, 1, 30_000));
        assert!(video[0].keyframe && !video[1].keyframe && video[10].keyframe);
        assert_eq!(video[5].data, vec![5u8; 105]);
    }

    #[test]
    fn seek_lands_on_previous_keyframe() {
        let mut d = Mp4Demuxer::open(Box::new(Cursor::new(sample_file()))).unwrap();
        let k = d.seek(MediaTime::from_secs_f64(1.2)).unwrap();
        // Keyframes every 10 frames at 29.97 fps: frame 30 is at 1.001 s.
        assert_eq!(k, MediaTime::from_timebase(30 * 1001, 1, 30_000));
        let first_video = std::iter::from_fn(|| d.read_packet().unwrap())
            .find(|p| p.track == 1)
            .unwrap();
        assert!(first_video.keyframe);
        assert_eq!(first_video.pts, k);
        let kf = d.keyframe_times(1).unwrap();
        assert_eq!(kf.len(), 6);
        // Disabled tracks are skipped.
        d.set_track_enabled(2, false);
        d.seek(MediaTime::ZERO).unwrap();
        assert!(std::iter::from_fn(|| d.read_packet().unwrap()).all(|p| p.track == 1));
    }

    #[test]
    fn fragmented_mp4() {
        // moov with an empty sample table + mvex/trex, then two moof/mdat pairs.
        let entry = video_entry(
            b"avc1",
            640,
            360,
            &bx(b"avcC", &crate::codec::annexb::tests::sample_avcc()),
        );
        let stbl = [
            full(b"stsd", 0, 0, &[&1u32.to_be_bytes()[..], &entry].concat()),
            full(b"stts", 0, 0, &[0; 4]),
            full(b"stsc", 0, 0, &[0; 4]),
            full(b"stsz", 0, 0, &[0; 8]),
            full(b"stco", 0, 0, &[0; 4]),
        ]
        .concat();
        let mdhd = full(
            b"mdhd",
            0,
            0,
            &[
                &[0u8; 8][..],
                &90_000u32.to_be_bytes(),
                &[0; 4],
                &[0x55, 0xc4, 0, 0],
            ]
            .concat(),
        );
        let hdlr = full(
            b"hdlr",
            0,
            0,
            &[&[0u8; 4][..], b"vide", &[0u8; 12], b"\0"].concat(),
        );
        let trak = bx(
            b"trak",
            &[
                full(
                    b"tkhd",
                    0,
                    3,
                    &[&[0u8; 8][..], &1u32.to_be_bytes(), &[0u8; 68]].concat(),
                ),
                bx(
                    b"mdia",
                    &[mdhd, hdlr, bx(b"minf", &bx(b"stbl", &stbl))].concat(),
                ),
            ]
            .concat(),
        );
        let trex = full(
            b"trex",
            0,
            0,
            &[1u32, 1, 3000, 0, 0x0001_0000]
                .iter()
                .flat_map(|v| v.to_be_bytes())
                .collect::<Vec<_>>(),
        );
        let moov = bx(
            b"moov",
            &[full(b"mvhd", 0, 0, &[0u8; 96]), trak, bx(b"mvex", &trex)].concat(),
        );
        let mut file = [bx(b"ftyp", b"iso6\0\0\0\0"), moov].concat();
        for frag in 0..2u32 {
            let sizes = [10u32, 20, 30];
            // trun: data_offset + first_sample_flags + per-sample size.
            let build_moof = |data_off: i32| {
                let tfhd = full(b"tfhd", 0, 0x020000, &1u32.to_be_bytes());
                let tfdt = full(b"tfdt", 1, 0, &((frag as u64) * 9000).to_be_bytes());
                let mut trun = 3u32.to_be_bytes().to_vec();
                trun.extend_from_slice(&data_off.to_be_bytes());
                trun.extend_from_slice(&0x0200_0000u32.to_be_bytes()); // first sample is sync
                for s in sizes {
                    trun.extend_from_slice(&s.to_be_bytes());
                }
                let trun = full(b"trun", 0, 0x000205, &trun);
                bx(b"moof", &bx(b"traf", &[tfhd, tfdt, trun].concat()))
            };
            let moof_len = build_moof(0).len();
            let moof = build_moof((moof_len + 8) as i32);
            let payload: Vec<u8> = sizes
                .iter()
                .enumerate()
                .flat_map(|(i, &s)| vec![(frag * 3 + i as u32) as u8; s as usize])
                .collect();
            file.extend(moof);
            file.extend(bx(b"mdat", &payload));
        }
        let mut d = Mp4Demuxer::open(Box::new(Cursor::new(file))).unwrap();
        let pkts: Vec<Packet> = std::iter::from_fn(|| d.read_packet().unwrap()).collect();
        assert_eq!(pkts.len(), 6);
        assert_eq!(pkts[3].pts, MediaTime::from_millis(100));
        assert_eq!(
            pkts[4].pts,
            MediaTime::from_secs_f64(3000.0 * 4.0 / 90_000.0)
        );
        assert!(pkts[0].keyframe && !pkts[1].keyframe && pkts[3].keyframe);
        assert_eq!(pkts[4].data, vec![4u8; 20]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(Mp4Demuxer::open(Box::new(Cursor::new(vec![0u8; 3]))).is_err());
    }
}
