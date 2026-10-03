//! libavformat demuxer over a custom AVIO context (feature `ffmpeg`).
//!
//! Any [`MediaInput`] (local file, SMB, WebDAV, HTTP range reader) is
//! exposed to libavformat through `avio_alloc_context` read/seek callbacks,
//! so containers without a pure-Rust demuxer here (MPEG-TS, AVI, FLV, …)
//! work from every source. Spherical / stereo side data is mapped onto the
//! same [`VideoParams`] fields as the native demuxers.

use crate::demux::Demuxer;
use crate::error::{Result, VideoError};
use crate::input::MediaInput;
use crate::packet::{
    media_info_from_tracks, AudioParams, CodecId, Packet, TrackDesc, TrackKind, VideoParams,
};
use ffmpeg_next::ffi;
use fp_core::media::{Chapter, ColorTransfer};
use fp_core::{MediaInfo, MediaTime, Projection, StereoMode};
use std::collections::VecDeque;
use std::ffi::{c_int, c_void, CStr};
use std::io::{Read, Seek, SeekFrom};

const IO_BUF: usize = 256 * 1024;

struct IoCtx {
    input: Box<dyn MediaInput>,
}

unsafe extern "C" fn read_cb(opaque: *mut c_void, buf: *mut u8, size: c_int) -> c_int {
    let io = &mut *(opaque as *mut IoCtx);
    let out = std::slice::from_raw_parts_mut(buf, size.max(0) as usize);
    match io.input.read(out) {
        Ok(0) => ffi::AVERROR_EOF,
        Ok(n) => n as c_int,
        Err(e) => {
            tracing::warn!("AVIO read: {e}");
            ffi::AVERROR(libc::EIO)
        }
    }
}

unsafe extern "C" fn seek_cb(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    let io = &mut *(opaque as *mut IoCtx);
    if whence & (ffi::AVSEEK_SIZE as c_int) != 0 {
        return io.input.size().map_or(-1, |s| s as i64);
    }
    let pos = match whence & !(ffi::AVSEEK_FORCE as c_int) {
        libc::SEEK_SET => SeekFrom::Start(offset.max(0) as u64),
        libc::SEEK_CUR => SeekFrom::Current(offset),
        libc::SEEK_END => SeekFrom::End(offset),
        _ => return -1,
    };
    io.input.seek(pos).map_or(-1, |p| p as i64)
}

pub struct FfmpegDemuxer {
    fmt: *mut ffi::AVFormatContext,
    avio: *mut ffi::AVIOContext,
    io: *mut IoCtx,
    pkt: *mut ffi::AVPacket,
    name: String,
    tracks: Vec<TrackDesc>,
    info: MediaInfo,
    /// Per stream index: (time base num, den, enabled, track id).
    streams: Vec<(i32, i32, bool)>,
    pending: VecDeque<Packet>,
}

// SAFETY: the libav contexts are only touched through `&mut self`.
unsafe impl Send for FfmpegDemuxer {}

fn dict_get(d: *mut ffi::AVDictionary, key: &CStr) -> Option<String> {
    // SAFETY: av_dict_get tolerates a null dictionary.
    unsafe {
        let e = ffi::av_dict_get(d, key.as_ptr(), std::ptr::null(), 0);
        if e.is_null() || (*e).value.is_null() {
            None
        } else {
            Some(CStr::from_ptr((*e).value).to_string_lossy().into_owned())
        }
    }
}

fn codec_from_id(id: ffi::AVCodecID) -> CodecId {
    use ffi::AVCodecID::*;
    match id {
        AV_CODEC_ID_H264 => CodecId::H264,
        AV_CODEC_ID_HEVC => CodecId::Hevc,
        AV_CODEC_ID_VP8 => CodecId::Vp8,
        AV_CODEC_ID_VP9 => CodecId::Vp9,
        AV_CODEC_ID_AV1 => CodecId::Av1,
        AV_CODEC_ID_AAC => CodecId::Aac,
        AV_CODEC_ID_OPUS => CodecId::Opus,
        AV_CODEC_ID_VORBIS => CodecId::Vorbis,
        AV_CODEC_ID_FLAC => CodecId::Flac,
        AV_CODEC_ID_MP3 => CodecId::Mp3,
        AV_CODEC_ID_AC3 => CodecId::Ac3,
        AV_CODEC_ID_EAC3 => CodecId::Eac3,
        AV_CODEC_ID_PCM_S16LE => CodecId::Pcm {
            bits: 16,
            float: false,
            big_endian: false,
        },
        AV_CODEC_ID_PCM_S16BE => CodecId::Pcm {
            bits: 16,
            float: false,
            big_endian: true,
        },
        AV_CODEC_ID_PCM_S24LE => CodecId::Pcm {
            bits: 24,
            float: false,
            big_endian: false,
        },
        AV_CODEC_ID_PCM_F32LE => CodecId::Pcm {
            bits: 32,
            float: true,
            big_endian: false,
        },
        AV_CODEC_ID_SUBRIP | AV_CODEC_ID_TEXT => CodecId::SubRip,
        AV_CODEC_ID_ASS | AV_CODEC_ID_SSA => CodecId::Ass,
        AV_CODEC_ID_WEBVTT => CodecId::WebVtt,
        AV_CODEC_ID_MOV_TEXT => CodecId::MovText,
        AV_CODEC_ID_HDMV_PGS_SUBTITLE => CodecId::Pgs,
        other => CodecId::Unknown(format!("{other:?}")),
    }
}

/// Leading fields of `AVSphericalMapping` (`libavutil/spherical.h`, not in
/// the generated bindings).
#[repr(C)]
struct SphericalMapping {
    projection: c_int,
    yaw: i32,
    pitch: i32,
    roll: i32,
    bound_left: u32,
    bound_top: u32,
    bound_right: u32,
    bound_bottom: u32,
}

/// Leading fields of `AVStereo3D` (`libavutil/stereo3d.h`).
#[repr(C)]
struct Stereo3D {
    type_: c_int,
    flags: c_int,
}

const AV_SPHERICAL_EQUIRECTANGULAR: c_int = 0;
const AV_SPHERICAL_CUBEMAP: c_int = 1;
const AV_SPHERICAL_EQUIRECTANGULAR_TILE: c_int = 2;
/// FFmpeg ≥ 7.1.
const AV_SPHERICAL_HALF_EQUIRECTANGULAR: c_int = 3;
const AV_STEREO3D_2D: c_int = 0;
const AV_STEREO3D_SIDEBYSIDE: c_int = 1;
const AV_STEREO3D_TOPBOTTOM: c_int = 2;

/// Spherical / stereo side data of a stream's codec parameters.
unsafe fn side_data_projection(
    par: *const ffi::AVCodecParameters,
) -> (Option<Projection>, Option<StereoMode>) {
    let mut proj = None;
    let mut stereo = None;
    let sd = ffi::av_packet_side_data_get(
        (*par).coded_side_data,
        (*par).nb_coded_side_data,
        ffi::AVPacketSideDataType::AV_PKT_DATA_SPHERICAL,
    );
    if !sd.is_null() && !(*sd).data.is_null() {
        let m = &*((*sd).data as *const SphericalMapping);
        proj = match m.projection {
            AV_SPHERICAL_EQUIRECTANGULAR => Some(Projection::EQUIRECT_360),
            AV_SPHERICAL_HALF_EQUIRECTANGULAR => Some(Projection::EQUIRECT_180),
            AV_SPHERICAL_EQUIRECTANGULAR_TILE => {
                let l = m.bound_left as f64 / 4_294_967_296.0;
                let r = m.bound_right as f64 / 4_294_967_296.0;
                let fov: f64 = (360.0 * (1.0 - l - r)).clamp(1.0, 360.0);
                Some(Projection::Equirect {
                    h_fov_deg: if (fov - 180.0).abs() < 1.0 {
                        180.0
                    } else {
                        fov as f32
                    },
                })
            }
            AV_SPHERICAL_CUBEMAP => Some(Projection::Eac),
            _ => None,
        };
        let _ = (m.yaw, m.pitch, m.roll, m.bound_top, m.bound_bottom);
    }
    let sd = ffi::av_packet_side_data_get(
        (*par).coded_side_data,
        (*par).nb_coded_side_data,
        ffi::AVPacketSideDataType::AV_PKT_DATA_STEREO3D,
    );
    if !sd.is_null() && !(*sd).data.is_null() {
        let s = &*((*sd).data as *const Stereo3D);
        let _ = s.flags;
        stereo = match s.type_ {
            AV_STEREO3D_2D => Some(StereoMode::Mono),
            AV_STEREO3D_SIDEBYSIDE => Some(StereoMode::Sbs),
            AV_STEREO3D_TOPBOTTOM => Some(StereoMode::Ou),
            _ => None,
        };
    }
    (proj, stereo)
}

impl FfmpegDemuxer {
    pub fn open(input: Box<dyn MediaInput>) -> Result<Self> {
        // SAFETY: standard libavformat custom-IO setup; every allocation is
        // checked and released on the error paths and in Drop.
        unsafe {
            let io = Box::into_raw(Box::new(IoCtx { input }));
            let buf = ffi::av_malloc(IO_BUF) as *mut u8;
            if buf.is_null() {
                drop(Box::from_raw(io));
                return Err(VideoError::Device("av_malloc failed".into()));
            }
            let mut avio = ffi::avio_alloc_context(
                buf,
                IO_BUF as c_int,
                0,
                io as *mut c_void,
                Some(read_cb),
                None,
                Some(seek_cb),
            );
            if avio.is_null() {
                ffi::av_free(buf as *mut c_void);
                drop(Box::from_raw(io));
                return Err(VideoError::Device("avio_alloc_context failed".into()));
            }
            let free_io = |avio: &mut *mut ffi::AVIOContext| {
                ffi::av_freep(&mut (**avio).buffer as *mut *mut u8 as *mut c_void);
                ffi::avio_context_free(avio);
                drop(Box::from_raw(io));
            };
            let mut fmt = ffi::avformat_alloc_context();
            if fmt.is_null() {
                free_io(&mut avio);
                return Err(VideoError::Device("avformat_alloc_context failed".into()));
            }
            (*fmt).pb = avio;
            (*fmt).flags |= ffi::AVFMT_FLAG_CUSTOM_IO as c_int;
            let r = ffi::avformat_open_input(
                &mut fmt,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
            );
            if r < 0 {
                // avformat_open_input frees `fmt` on failure.
                free_io(&mut avio);
                return Err(VideoError::Unsupported(format!(
                    "libavformat could not open input ({r})"
                )));
            }
            if ffi::avformat_find_stream_info(fmt, std::ptr::null_mut()) < 0 {
                tracing::warn!("avformat_find_stream_info failed; continuing with partial info");
            }
            let name = CStr::from_ptr((*(*fmt).iformat).name)
                .to_string_lossy()
                .into_owned();
            let mut tracks = Vec::new();
            let mut streams = Vec::new();
            for i in 0..(*fmt).nb_streams as usize {
                let st = *(*fmt).streams.add(i);
                let par = (*st).codecpar;
                let tb = (*st).time_base;
                streams.push((tb.num, tb.den, true));
                let kind = match (*par).codec_type {
                    ffi::AVMediaType::AVMEDIA_TYPE_VIDEO => TrackKind::Video,
                    ffi::AVMediaType::AVMEDIA_TYPE_AUDIO => TrackKind::Audio,
                    ffi::AVMediaType::AVMEDIA_TYPE_SUBTITLE => TrackKind::Subtitle,
                    _ => TrackKind::Other,
                };
                if (*st).disposition & ffi::AV_DISPOSITION_ATTACHED_PIC as c_int != 0 {
                    continue; // cover art
                }
                let mut d = TrackDesc::new(i as u32, kind, codec_from_id((*par).codec_id));
                if !(*par).extradata.is_null() && (*par).extradata_size > 0 {
                    d.codec_private = std::slice::from_raw_parts(
                        (*par).extradata,
                        (*par).extradata_size as usize,
                    )
                    .to_vec();
                }
                d.language = dict_get((*st).metadata, c"language").filter(|l| l != "und");
                d.name = dict_get((*st).metadata, c"title");
                d.default = (*st).disposition & ffi::AV_DISPOSITION_DEFAULT as c_int != 0;
                if (*st).duration > 0 && tb.den > 0 {
                    d.duration = Some(MediaTime::from_timebase((*st).duration, tb.num, tb.den));
                }
                match kind {
                    TrackKind::Video => {
                        let fr = (*st).avg_frame_rate;
                        let (projection, stereo) = side_data_projection(par);
                        let transfer = match (*par).color_trc {
                            ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084 => {
                                ColorTransfer::Pq
                            }
                            ffi::AVColorTransferCharacteristic::AVCOL_TRC_ARIB_STD_B67 => {
                                ColorTransfer::Hlg
                            }
                            _ => ColorTransfer::Sdr,
                        };
                        let bits = (*par).bits_per_raw_sample;
                        d.video = Some(VideoParams {
                            width: (*par).width as u32,
                            height: (*par).height as u32,
                            bit_depth: if bits > 0 {
                                bits as u8
                            } else {
                                crate::codec::bit_depth_from_config(&d.codec, &d.codec_private)
                                    .unwrap_or(8)
                            },
                            fps: if fr.den > 0 {
                                fr.num as f64 / fr.den as f64
                            } else {
                                0.0
                            },
                            transfer,
                            projection,
                            stereo,
                        });
                    }
                    TrackKind::Audio => {
                        d.audio = Some(AudioParams {
                            sample_rate: (*par).sample_rate as u32,
                            channels: (*par).ch_layout.nb_channels as u16,
                            bits_per_sample: (*par).bits_per_raw_sample as u16,
                            ambisonic: None,
                        });
                    }
                    _ => {}
                }
                tracks.push(d);
            }
            let mut chapters = Vec::new();
            for i in 0..(*fmt).nb_chapters as usize {
                let c = *(*fmt).chapters.add(i);
                chapters.push(Chapter {
                    start: MediaTime::from_timebase(
                        (*c).start,
                        (*c).time_base.num,
                        (*c).time_base.den,
                    ),
                    title: dict_get((*c).metadata, c"title").unwrap_or_default(),
                });
            }
            let duration = ((*fmt).duration > 0).then(|| MediaTime((*fmt).duration)); // AV_TIME_BASE = µs
            let info =
                media_info_from_tracks(&format!("ffmpeg:{name}"), duration, &tracks, chapters);
            let pkt = ffi::av_packet_alloc();
            Ok(FfmpegDemuxer {
                fmt,
                avio,
                io,
                pkt,
                name,
                tracks,
                info,
                streams,
                pending: VecDeque::new(),
            })
        }
    }

    fn read_raw(&mut self) -> Result<Option<Packet>> {
        // SAFETY: fmt/pkt are valid for the demuxer's lifetime.
        unsafe {
            loop {
                let r = ffi::av_read_frame(self.fmt, self.pkt);
                if r == ffi::AVERROR_EOF {
                    return Ok(None);
                }
                if r < 0 {
                    return Err(VideoError::Invalid(format!("av_read_frame: {r}")));
                }
                let p = &*self.pkt;
                let idx = p.stream_index as usize;
                let Some(&(num, den, enabled)) = self.streams.get(idx) else {
                    ffi::av_packet_unref(self.pkt);
                    continue;
                };
                if !enabled || !self.tracks.iter().any(|t| t.id == idx as u32) {
                    ffi::av_packet_unref(self.pkt);
                    continue;
                }
                let conv = |v: i64| {
                    (v != ffi::AV_NOPTS_VALUE).then(|| MediaTime::from_timebase(v, num, den))
                };
                let dts = conv(p.dts);
                let pts = conv(p.pts).or(dts).unwrap_or(MediaTime::ZERO);
                let data = if p.data.is_null() {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(p.data, p.size as usize).to_vec()
                };
                let out = Packet {
                    track: idx as u32,
                    pts,
                    dts: dts.unwrap_or(pts),
                    duration: MediaTime::from_timebase(p.duration, num, den),
                    keyframe: p.flags & ffi::AV_PKT_FLAG_KEY as c_int != 0,
                    data,
                };
                ffi::av_packet_unref(self.pkt);
                return Ok(Some(out));
            }
        }
    }
}

impl Demuxer for FfmpegDemuxer {
    fn format_name(&self) -> &str {
        &self.name
    }
    fn tracks(&self) -> &[TrackDesc] {
        &self.tracks
    }
    fn media_info(&self) -> &MediaInfo {
        &self.info
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        if let Some(p) = self.pending.pop_front() {
            return Ok(Some(p));
        }
        self.read_raw()
    }

    fn seek(&mut self, target: MediaTime) -> Result<MediaTime> {
        self.pending.clear();
        // SAFETY: valid context; stream -1 seeks in AV_TIME_BASE (µs).
        let r = unsafe {
            ffi::av_seek_frame(
                self.fmt,
                -1,
                target.0.max(0),
                ffi::AVSEEK_FLAG_BACKWARD as c_int,
            )
        };
        if r < 0 {
            return Err(VideoError::Invalid(format!("av_seek_frame: {r}")));
        }
        // Find the keyframe we landed on; buffer from it onward.
        let video = self
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Video && self.streams[t.id as usize].2)
            .map(|t| t.id);
        let Some(vid) = video else { return Ok(target) };
        let mut others = Vec::new();
        for _ in 0..4000 {
            match self.read_raw()? {
                Some(p) if p.track == vid && p.keyframe => {
                    let k = p.pts;
                    // Keep other tracks' packets that belong at/after the keyframe.
                    let from = k - MediaTime::from_millis(100);
                    self.pending
                        .extend(others.into_iter().filter(|o: &Packet| o.pts >= from));
                    self.pending.push_back(p);
                    return Ok(k);
                }
                Some(p) if p.track != vid => others.push(p),
                Some(_) => {}
                None => break,
            }
        }
        self.pending.extend(others);
        Ok(target)
    }

    fn set_track_enabled(&mut self, track: u32, enabled: bool) {
        if let Some(s) = self.streams.get_mut(track as usize) {
            s.2 = enabled;
            // Let libavformat skip disabled streams' data where it can.
            // SAFETY: index checked against nb_streams via `streams`.
            unsafe {
                let st = *(*self.fmt).streams.add(track as usize);
                (*st).discard = if enabled {
                    ffi::AVDiscard::AVDISCARD_DEFAULT
                } else {
                    ffi::AVDiscard::AVDISCARD_ALL
                };
            }
        }
    }
}

impl Drop for FfmpegDemuxer {
    fn drop(&mut self) {
        // SAFETY: tear down in reverse order of creation; custom IO is ours to free.
        unsafe {
            ffi::av_packet_free(&mut self.pkt);
            ffi::avformat_close_input(&mut self.fmt);
            if !self.avio.is_null() {
                ffi::av_freep(&mut (*self.avio).buffer as *mut *mut u8 as *mut c_void);
                ffi::avio_context_free(&mut self.avio);
            }
            drop(Box::from_raw(self.io));
        }
    }
}
