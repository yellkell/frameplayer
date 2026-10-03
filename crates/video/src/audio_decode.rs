//! Audio decoding to interleaved `f32` PCM for the fp-audio pipeline.
//!
//! * PCM (MP4 `sowt`/`twos`/`lpcm`/`in24`/`fl32`…, Matroska `A_PCM/*`): built in.
//! * AAC-LC mono/stereo: pure-Rust symphonia (feature `aac`, on by default).
//! * Everything else (Opus, Vorbis, FLAC, MP3, AC-3, E-AC-3, multichannel
//!   or HE-AAC): libavcodec (feature `ffmpeg`).

use crate::error::{Result, VideoError};
use crate::packet::{CodecId, Packet, TrackDesc};
use fp_core::MediaTime;

/// A block of decoded audio.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFrame {
    pub pts: MediaTime,
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved samples in `[-1, 1]`.
    pub samples: Vec<f32>,
}

impl AudioFrame {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }
    pub fn duration(&self) -> MediaTime {
        MediaTime::from_secs_f64(self.frames() as f64 / self.sample_rate.max(1) as f64)
    }
}

pub trait AudioDecoder: Send {
    /// Decode one packet (may yield nothing, e.g. while priming).
    fn decode(&mut self, pkt: &Packet) -> Result<Option<AudioFrame>>;
    /// Reset state after a seek.
    fn flush(&mut self);
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
}

/// Open a decoder for an audio track.
pub fn open_audio_decoder(track: &TrackDesc) -> Result<Box<dyn AudioDecoder>> {
    let a = track.audio.clone().unwrap_or_default();
    match &track.codec {
        CodecId::Pcm {
            bits,
            float,
            big_endian,
        } => {
            return Ok(Box::new(PcmDecoder::new(
                *bits,
                *float,
                *big_endian,
                a.sample_rate,
                a.channels,
            )?));
        }
        #[cfg(feature = "aac")]
        CodecId::Aac if a.channels <= 2 => match aac::AacLcDecoder::new(track) {
            Ok(d) => return Ok(Box::new(d)),
            Err(e) => tracing::debug!("symphonia AAC unavailable for this stream: {e}"),
        },
        _ => {}
    }
    #[cfg(feature = "ffmpeg")]
    {
        return Ok(Box::new(ffmpeg_audio::FfmpegAudioDecoder::new(track)?));
    }
    #[allow(unreachable_code)]
    Err(VideoError::NoDecoder(format!(
        "no audio decoder for {:?} (enable the `ffmpeg` feature)",
        track.codec
    )))
}

/// Uncompressed PCM → f32.
pub struct PcmDecoder {
    bits: u8,
    float: bool,
    big_endian: bool,
    rate: u32,
    channels: u16,
}

impl PcmDecoder {
    pub fn new(bits: u8, float: bool, big_endian: bool, rate: u32, channels: u16) -> Result<Self> {
        let ok = matches!((bits, float), (8 | 16 | 24 | 32, false) | (32 | 64, true));
        if !ok || channels == 0 {
            return Err(VideoError::Unsupported(format!(
                "PCM {bits}-bit float={float} ×{channels}"
            )));
        }
        Ok(PcmDecoder {
            bits,
            float,
            big_endian,
            rate,
            channels,
        })
    }

    pub fn convert(&self, data: &[u8]) -> Vec<f32> {
        let bytes = self.bits as usize / 8;
        data.chunks_exact(bytes)
            .map(|c| {
                let mut b = [0u8; 8];
                if self.big_endian {
                    for (i, &v) in c.iter().rev().enumerate() {
                        b[i] = v;
                    }
                } else {
                    b[..bytes].copy_from_slice(c);
                }
                match (self.bits, self.float) {
                    // 8-bit PCM in these containers is signed (QuickTime `twos`).
                    (8, false) => b[0] as i8 as f32 / 128.0,
                    (16, false) => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
                    (24, false) => {
                        (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0
                    }
                    (32, false) => {
                        i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0
                    }
                    (32, true) => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                    _ => f64::from_le_bytes(b) as f32,
                }
            })
            .collect()
    }
}

impl AudioDecoder for PcmDecoder {
    fn decode(&mut self, pkt: &Packet) -> Result<Option<AudioFrame>> {
        Ok(Some(AudioFrame {
            pts: pkt.pts,
            sample_rate: self.rate,
            channels: self.channels,
            samples: self.convert(&pkt.data),
        }))
    }
    fn flush(&mut self) {}
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }
}

#[cfg(feature = "aac")]
mod aac {
    use super::*;
    use symphonia_codec_aac::AacDecoder;
    use symphonia_core::audio::SampleBuffer;
    use symphonia_core::codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_AAC};
    use symphonia_core::formats::Packet as SymPacket;

    pub struct AacLcDecoder {
        dec: AacDecoder,
        rate: u32,
        channels: u16,
        buf: Option<SampleBuffer<f32>>,
    }

    impl AacLcDecoder {
        pub fn new(track: &TrackDesc) -> Result<Self> {
            let a = track.audio.clone().unwrap_or_default();
            if track.codec_private.len() < 2 {
                return Err(VideoError::Unsupported(
                    "AAC without AudioSpecificConfig".into(),
                ));
            }
            let mut params = CodecParameters::new();
            params
                .for_codec(CODEC_TYPE_AAC)
                .with_sample_rate(a.sample_rate)
                .with_extra_data(track.codec_private.clone().into_boxed_slice());
            let dec = AacDecoder::try_new(&params, &DecoderOptions::default())
                .map_err(|e| VideoError::Unsupported(format!("AAC: {e}")))?;
            Ok(AacLcDecoder {
                dec,
                rate: a.sample_rate,
                channels: a.channels.max(1),
                buf: None,
            })
        }
    }

    impl AudioDecoder for AacLcDecoder {
        fn decode(&mut self, pkt: &Packet) -> Result<Option<AudioFrame>> {
            let sp = SymPacket::new_from_slice(0, 0, 1024, &pkt.data);
            let decoded = match self.dec.decode(&sp) {
                Ok(d) => d,
                Err(e) => {
                    tracing::debug!("AAC decode error (packet dropped): {e}");
                    return Ok(None);
                }
            };
            let spec = *decoded.spec();
            let frames = decoded.frames();
            if self
                .buf
                .as_ref()
                .is_none_or(|b| b.capacity() < frames * spec.channels.count())
            {
                self.buf = Some(SampleBuffer::new(frames as u64, spec));
            }
            let buf = self.buf.as_mut().unwrap();
            buf.copy_interleaved_ref(decoded);
            self.rate = spec.rate;
            self.channels = spec.channels.count() as u16;
            Ok(Some(AudioFrame {
                pts: pkt.pts,
                sample_rate: self.rate,
                channels: self.channels,
                samples: buf.samples().to_vec(),
            }))
        }
        fn flush(&mut self) {
            self.dec.reset();
        }
        fn sample_rate(&self) -> u32 {
            self.rate
        }
        fn channels(&self) -> u16 {
            self.channels
        }
    }
}

#[cfg(feature = "ffmpeg")]
mod ffmpeg_audio {
    use super::*;
    use crate::decode::ffmpeg::{codec_id, fferr, set_extradata};
    use ffmpeg_next as ff;

    pub struct FfmpegAudioDecoder {
        dec: ff::decoder::Audio,
        frame: ff::frame::Audio,
        resampler: Option<ff::software::resampling::Context>,
        rate: u32,
        channels: u16,
    }

    // SAFETY: used from one thread at a time.
    unsafe impl Send for FfmpegAudioDecoder {}

    impl FfmpegAudioDecoder {
        pub fn new(track: &TrackDesc) -> Result<Self> {
            ff::init().map_err(fferr)?;
            let id = codec_id(&track.codec)
                .ok_or_else(|| VideoError::Unsupported(format!("{:?}", track.codec)))?;
            let codec = ff::decoder::find(id)
                .ok_or_else(|| VideoError::NoDecoder(format!("libavcodec lacks {id:?}")))?;
            let mut ctx = ff::codec::Context::new_with_codec(codec);
            let a = track.audio.clone().unwrap_or_default();
            // SAFETY: fresh context; plain field writes before opening.
            unsafe {
                set_extradata(&mut ctx, &track.codec_private);
                let raw = ctx.as_mut_ptr();
                (*raw).sample_rate = a.sample_rate as i32;
                ff::ffi::av_channel_layout_default(&mut (*raw).ch_layout, a.channels.max(1) as i32);
            }
            ctx.set_time_base(ff::Rational::new(1, 1_000_000));
            let dec = ctx.decoder().audio().map_err(fferr)?;
            Ok(FfmpegAudioDecoder {
                dec,
                frame: ff::frame::Audio::empty(),
                resampler: None,
                rate: a.sample_rate,
                channels: a.channels.max(1),
            })
        }
    }

    impl AudioDecoder for FfmpegAudioDecoder {
        fn decode(&mut self, pkt: &Packet) -> Result<Option<AudioFrame>> {
            let mut p = ff::Packet::copy(&pkt.data);
            p.set_pts(Some(pkt.pts.0));
            if let Err(e) = self.dec.send_packet(&p) {
                tracing::debug!("audio decode error (packet dropped): {e}");
                return Ok(None);
            }
            let mut out: Option<AudioFrame> = None;
            while self.dec.receive_frame(&mut self.frame).is_ok() {
                let rate = self.frame.rate();
                let ch = self.frame.channels();
                let target = ff::format::Sample::F32(ff::format::sample::Type::Packed);
                if self.resampler.as_ref().is_none_or(|r| {
                    r.input().format != self.frame.format() || r.input().rate != rate
                }) {
                    self.resampler = Some(
                        ff::software::resampling::Context::get(
                            self.frame.format(),
                            self.frame.channel_layout(),
                            rate,
                            target,
                            self.frame.channel_layout(),
                            rate,
                        )
                        .map_err(fferr)?,
                    );
                }
                let mut conv = ff::frame::Audio::empty();
                self.resampler
                    .as_mut()
                    .unwrap()
                    .run(&self.frame, &mut conv)
                    .map_err(fferr)?;
                let n = conv.samples() * ch as usize;
                let bytes = &conv.data(0)[..n * 4];
                let samples: Vec<f32> = bytes
                    .chunks_exact(4)
                    .map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                self.rate = rate;
                self.channels = ch;
                let pts = self.frame.pts().map(MediaTime).unwrap_or(pkt.pts);
                match &mut out {
                    Some(f) => f.samples.extend(samples),
                    None => {
                        out = Some(AudioFrame {
                            pts,
                            sample_rate: rate,
                            channels: ch,
                            samples,
                        })
                    }
                }
            }
            Ok(out)
        }
        fn flush(&mut self) {
            self.dec.flush();
        }
        fn sample_rate(&self) -> u32 {
            self.rate
        }
        fn channels(&self) -> u16 {
            self.channels
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{AudioParams, TrackKind};

    #[test]
    fn pcm_conversions() {
        let d = PcmDecoder::new(16, false, false, 48_000, 2).unwrap();
        assert_eq!(
            d.convert(&[0x00, 0x80, 0xff, 0x7f]),
            vec![-1.0, 32767.0 / 32768.0]
        );
        let d = PcmDecoder::new(16, false, true, 48_000, 1).unwrap();
        assert_eq!(d.convert(&[0x40, 0x00]), vec![0.5]);
        let d = PcmDecoder::new(24, false, true, 48_000, 1).unwrap();
        assert_eq!(d.convert(&[0xc0, 0x00, 0x00]), vec![-0.5]);
        let d = PcmDecoder::new(32, true, false, 48_000, 1).unwrap();
        assert_eq!(d.convert(&0.25f32.to_le_bytes()), vec![0.25]);
        assert!(PcmDecoder::new(12, false, false, 48_000, 1).is_err());
    }

    #[test]
    fn opens_pcm_track() {
        let mut t = TrackDesc::new(
            2,
            TrackKind::Audio,
            CodecId::Pcm {
                bits: 16,
                float: false,
                big_endian: false,
            },
        );
        t.audio = Some(AudioParams {
            sample_rate: 44_100,
            channels: 2,
            bits_per_sample: 16,
            ambisonic: None,
        });
        let mut d = open_audio_decoder(&t).unwrap();
        let pkt = Packet {
            track: 2,
            pts: MediaTime::from_millis(10),
            dts: MediaTime::ZERO,
            duration: MediaTime::ZERO,
            keyframe: true,
            data: vec![0; 400],
        };
        let f = d.decode(&pkt).unwrap().unwrap();
        assert_eq!((f.frames(), f.channels, f.sample_rate), (100, 2, 44_100));
        assert_eq!(f.pts, MediaTime::from_millis(10));
    }

    #[cfg(feature = "aac")]
    #[test]
    fn aac_decoder_opens_from_asc() {
        let mut t = TrackDesc::new(1, TrackKind::Audio, CodecId::Aac);
        t.codec_private = vec![0x12, 0x10];
        t.audio = Some(AudioParams {
            sample_rate: 44_100,
            channels: 2,
            bits_per_sample: 16,
            ambisonic: None,
        });
        let mut d = open_audio_decoder(&t).unwrap();
        // A garbage packet is dropped, not fatal.
        let pkt = Packet {
            track: 1,
            pts: MediaTime::ZERO,
            dts: MediaTime::ZERO,
            duration: MediaTime::ZERO,
            keyframe: true,
            data: vec![0xa0; 10],
        };
        assert!(d.decode(&pkt).is_ok());
    }
}
