//! Converts decoded audio frames to interleaved f32 at 48 kHz.

use crate::decode::Frame;
use crate::{Error, Result, check};
use fp_ffmpeg_sys as ff;

pub struct Resampler {
    swr: *mut ff::SwrContext,
    key: (i32, i32, i32, u64),
    out_layout: ff::AVChannelLayout,
    pub out_channels: u32,
}

// SAFETY: used by the audio thread only.
unsafe impl Send for Resampler {}

impl Resampler {
    /// `keep_layout`: keep the source channel layout (ambisonics) instead of
    /// down-mixing to stereo.
    pub fn new(keep_layout_channels: Option<u32>) -> Resampler {
        // SAFETY: zeroed layout is the documented "unset" state.
        let mut out_layout: ff::AVChannelLayout = unsafe { std::mem::zeroed() };
        let ch = keep_layout_channels.unwrap_or(2);
        unsafe { ff::av_channel_layout_default(&mut out_layout, ch as i32) };
        Resampler {
            swr: std::ptr::null_mut(),
            key: (0, 0, 0, 0),
            out_layout,
            out_channels: ch,
        }
    }

    /// Converts one frame, appending samples to `out`.
    pub fn convert(&mut self, frame: &Frame, out: &mut Vec<f32>) -> Result<()> {
        // SAFETY: frame is a decoded audio frame; swr is (re)created to match it.
        unsafe {
            let f = &*frame.as_ptr();
            let key = (
                f.format,
                f.sample_rate,
                f.ch_layout.nb_channels,
                f.ch_layout.u.mask,
            );
            if self.swr.is_null() || key != self.key {
                ff::swr_free(&mut self.swr);
                let mut swr = std::ptr::null_mut();
                check(
                    ff::swr_alloc_set_opts2(
                        &mut swr,
                        &self.out_layout,
                        ff::AV_SAMPLE_FMT_FLT,
                        super::RATE as i32,
                        &f.ch_layout,
                        f.format,
                        f.sample_rate,
                        0,
                        std::ptr::null_mut(),
                    ),
                    "audio resampler",
                )?;
                check(ff::swr_init(swr), "audio resampler init")?;
                self.swr = swr;
                self.key = key;
            }
            let max_out = ff::swr_get_out_samples(self.swr, f.nb_samples).max(0) as usize + 32;
            let ch = self.out_channels as usize;
            let start = out.len();
            out.resize(start + max_out * ch, 0.0);
            let dst = out[start..].as_mut_ptr() as *mut u8;
            let n = ff::swr_convert(
                self.swr,
                &dst,
                max_out as i32,
                f.extended_data as *mut *const u8,
                f.nb_samples,
            );
            if n < 0 {
                out.truncate(start);
                return Err(Error::Ffmpeg {
                    context: "resample",
                    code: n,
                    message: ff::err_to_string(n),
                });
            }
            out.truncate(start + n as usize * ch);
        }
        Ok(())
    }

    pub fn reset(&mut self) {
        // SAFETY: frees our context; recreated lazily.
        unsafe { ff::swr_free(&mut self.swr) };
    }
}

impl Drop for Resampler {
    fn drop(&mut self) {
        // SAFETY: frees our own state.
        unsafe {
            ff::swr_free(&mut self.swr);
            ff::av_channel_layout_uninit(&mut self.out_layout);
        }
    }
}
