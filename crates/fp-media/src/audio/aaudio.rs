//! Android's AAudio, the sound device on a Meta Quest.

use super::output::Sink;
use crate::{Error, Result};
use std::ffi::{CStr, c_char, c_void};

type Builder = c_void;
type Stream = c_void;

const FORMAT_PCM_FLOAT: i32 = 2;
const PERFORMANCE_MODE_NONE: i32 = 10;
const USAGE_MEDIA: i32 = 1;
const CONTENT_TYPE_MOVIE: i32 = 3;
const STATE_PAUSING: i32 = 5;
const STATE_FLUSHING: i32 = 7;
const ERROR_DISCONNECTED: i32 = -899;
const CLOCK_MONOTONIC: i32 = 1;

#[link(name = "aaudio")]
unsafe extern "C" {
    fn AAudio_createStreamBuilder(builder: *mut *mut Builder) -> i32;
    fn AAudio_convertResultToText(result: i32) -> *const c_char;
    fn AAudioStreamBuilder_setFormat(builder: *mut Builder, format: i32);
    fn AAudioStreamBuilder_setChannelCount(builder: *mut Builder, channels: i32);
    fn AAudioStreamBuilder_setSampleRate(builder: *mut Builder, rate: i32);
    fn AAudioStreamBuilder_setPerformanceMode(builder: *mut Builder, mode: i32);
    fn AAudioStreamBuilder_setUsage(builder: *mut Builder, usage: i32);
    fn AAudioStreamBuilder_setContentType(builder: *mut Builder, content: i32);
    fn AAudioStreamBuilder_openStream(builder: *mut Builder, stream: *mut *mut Stream) -> i32;
    fn AAudioStreamBuilder_delete(builder: *mut Builder) -> i32;
    fn AAudioStream_requestStart(stream: *mut Stream) -> i32;
    fn AAudioStream_requestPause(stream: *mut Stream) -> i32;
    fn AAudioStream_requestFlush(stream: *mut Stream) -> i32;
    fn AAudioStream_waitForStateChange(
        stream: *mut Stream,
        input: i32,
        next: *mut i32,
        timeout_ns: i64,
    ) -> i32;
    fn AAudioStream_write(
        stream: *mut Stream,
        buffer: *const c_void,
        frames: i32,
        timeout_ns: i64,
    ) -> i32;
    fn AAudioStream_getFramesWritten(stream: *mut Stream) -> i64;
    fn AAudioStream_getFramesRead(stream: *mut Stream) -> i64;
    fn AAudioStream_getTimestamp(
        stream: *mut Stream,
        clock: i32,
        position: *mut i64,
        time_ns: *mut i64,
    ) -> i32;
    fn AAudioStream_getSampleRate(stream: *mut Stream) -> i32;
    fn AAudioStream_close(stream: *mut Stream) -> i32;
}

fn text(r: i32) -> String {
    // SAFETY: returns a static string for any value.
    unsafe { CStr::from_ptr(AAudio_convertResultToText(r)) }
        .to_string_lossy()
        .into_owned()
}

pub struct AAudio {
    stream: *mut Stream,
    channels: u32,
}

// SAFETY: the stream is only used from the audio thread that owns it.
unsafe impl Send for AAudio {}

impl AAudio {
    pub fn open(channels: u32) -> Result<AAudio> {
        let stream = open_stream(channels)?;
        Ok(AAudio { stream, channels })
    }

    /// Replaces a stream the system took away (headphones plugged in or
    /// out, Link connected): AAudio never recovers one by itself.
    fn reopen(&mut self) -> Result<()> {
        // SAFETY: closes our stream once; replaced right after.
        unsafe { AAudioStream_close(self.stream) };
        self.stream = open_stream(self.channels)?;
        Ok(())
    }
}

/// An f32 output stream at [`super::RATE`], already started.
fn open_stream(channels: u32) -> Result<*mut Stream> {
    let mut b: *mut Builder = std::ptr::null_mut();
    // SAFETY: valid out-pointers; the builder is deleted on every path.
    unsafe {
        let r = AAudio_createStreamBuilder(&mut b);
        if r < 0 {
            return Err(Error::Audio(format!("AAudio builder: {}", text(r))));
        }
        AAudioStreamBuilder_setFormat(b, FORMAT_PCM_FLOAT);
        AAudioStreamBuilder_setChannelCount(b, channels as i32);
        AAudioStreamBuilder_setSampleRate(b, super::RATE as i32);
        AAudioStreamBuilder_setPerformanceMode(b, PERFORMANCE_MODE_NONE);
        AAudioStreamBuilder_setUsage(b, USAGE_MEDIA);
        AAudioStreamBuilder_setContentType(b, CONTENT_TYPE_MOVIE);
        let mut s: *mut Stream = std::ptr::null_mut();
        let r = AAudioStreamBuilder_openStream(b, &mut s);
        AAudioStreamBuilder_delete(b);
        if r < 0 {
            return Err(Error::Audio(format!("AAudio open: {}", text(r))));
        }
        let rate = AAudioStream_getSampleRate(s);
        if rate != super::RATE as i32 {
            AAudioStream_close(s);
            return Err(Error::Audio(format!("AAudio opened at {rate} Hz")));
        }
        let r = AAudioStream_requestStart(s);
        if r < 0 {
            AAudioStream_close(s);
            return Err(Error::Audio(format!("AAudio start: {}", text(r))));
        }
        Ok(s)
    }
}

impl Sink for AAudio {
    fn write(&mut self, interleaved: &[f32]) -> Result<()> {
        let ch = self.channels as usize;
        let mut off = 0;
        while off < interleaved.len() {
            let frames = ((interleaved.len() - off) / ch) as i32;
            // SAFETY: the buffer holds `frames * ch` floats from `off`.
            let n = unsafe {
                AAudioStream_write(
                    self.stream,
                    interleaved[off..].as_ptr() as *const c_void,
                    frames,
                    1_000_000_000,
                )
            };
            if n == ERROR_DISCONNECTED {
                self.reopen()?;
                continue;
            }
            if n < 0 {
                return Err(Error::Audio(format!("AAudio write: {}", text(n))));
            }
            off += n as usize * ch;
        }
        Ok(())
    }

    fn delay(&mut self) -> f64 {
        let rate = super::RATE as f64;
        // SAFETY: stream open, valid out-pointers.
        unsafe {
            let written = AAudioStream_getFramesWritten(self.stream);
            let (mut pos, mut at) = (0i64, 0i64);
            // Frames heard by now: the last presented frame, advanced by
            // the time since it was presented.
            let heard = if AAudioStream_getTimestamp(
                self.stream,
                CLOCK_MONOTONIC,
                &mut pos,
                &mut at,
            ) >= 0
            {
                let mut now = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
                let now_ns = now.tv_sec * 1_000_000_000 + now.tv_nsec;
                pos as f64 + (now_ns - at).max(0) as f64 * rate / 1e9
            } else {
                AAudioStream_getFramesRead(self.stream) as f64
            };
            ((written as f64 - heard) / rate).max(0.0)
        }
    }

    fn flush(&mut self) {
        // SAFETY: stream open; flushing is only allowed once paused.
        unsafe {
            let mut next = 0;
            AAudioStream_requestPause(self.stream);
            AAudioStream_waitForStateChange(self.stream, STATE_PAUSING, &mut next, 200_000_000);
            AAudioStream_requestFlush(self.stream);
            AAudioStream_waitForStateChange(self.stream, STATE_FLUSHING, &mut next, 200_000_000);
            AAudioStream_requestStart(self.stream);
        }
    }

    fn name(&self) -> String {
        "aaudio".into()
    }
}

impl Drop for AAudio {
    fn drop(&mut self) {
        // SAFETY: closes our stream once.
        unsafe { AAudioStream_close(self.stream) };
    }
}
