//! Audio devices: ALSA (loaded at run time; on SteamOS it routes to
//! PipeWire through pipewire-alsa) and a null sink that keeps real time.

use crate::{Error, Result};
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_void};
use std::time::{Duration, Instant};

/// Something that plays interleaved f32 stereo at [`super::RATE`].
pub trait Sink: Send {
    /// Blocks until all frames are queued.
    fn write(&mut self, interleaved: &[f32]) -> Result<()>;
    /// Seconds of audio queued but not yet heard.
    fn delay(&mut self) -> f64;
    /// Stops output immediately, discarding queued audio.
    fn flush(&mut self);
    fn name(&self) -> String;
}

/// Which device to open.
#[derive(Clone, Debug, PartialEq)]
pub enum Backend {
    /// ALSA PCM name, e.g. "default" or "pipewire".
    Alsa { device: String, latency_ms: u32 },
    /// No sound; timing only.
    Null,
}

impl Default for Backend {
    fn default() -> Self {
        Backend::Alsa {
            device: "default".into(),
            latency_ms: 80,
        }
    }
}

/// Opens `backend`, falling back to the null sink if ALSA is unavailable.
pub fn open(backend: &Backend, channels: u32) -> Box<dyn Sink> {
    match backend {
        Backend::Alsa { device, latency_ms } => match Alsa::open(device, channels, *latency_ms) {
            Ok(a) => Box::new(a),
            Err(e) => {
                log::warn!("audio: {e}; continuing without sound");
                Box::new(NullSink::new(channels))
            }
        },
        Backend::Null => Box::new(NullSink::new(channels)),
    }
}

/// Plays nothing but blocks like a real device, so timing logic is exercised.
pub struct NullSink {
    channels: u32,
    /// When the queued audio will have finished playing.
    busy_until: Instant,
}

impl NullSink {
    pub fn new(channels: u32) -> NullSink {
        NullSink {
            channels: channels.max(1),
            busy_until: Instant::now(),
        }
    }
}

impl Sink for NullSink {
    fn write(&mut self, interleaved: &[f32]) -> Result<()> {
        let now = Instant::now();
        if self.busy_until < now {
            self.busy_until = now;
        }
        let secs = interleaved.len() as f64 / self.channels as f64 / super::RATE as f64;
        self.busy_until += Duration::from_secs_f64(secs);
        // Keep at most ~50 ms queued, like a device buffer.
        let ahead = self.busy_until.saturating_duration_since(Instant::now());
        if ahead > Duration::from_millis(50) {
            std::thread::sleep(ahead - Duration::from_millis(50));
        }
        Ok(())
    }
    fn delay(&mut self) -> f64 {
        self.busy_until
            .saturating_duration_since(Instant::now())
            .as_secs_f64()
    }
    fn flush(&mut self) {
        self.busy_until = Instant::now();
    }
    fn name(&self) -> String {
        "null".into()
    }
}

type Pcm = *mut c_void;
const SND_PCM_STREAM_PLAYBACK: c_int = 0;
const SND_PCM_FORMAT_FLOAT_LE: c_int = 14;
const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;

struct AlsaFns {
    _lib: libloading::Library,
    open: unsafe extern "C" fn(*mut Pcm, *const c_char, c_int, c_int) -> c_int,
    set_params: unsafe extern "C" fn(Pcm, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int,
    writei: unsafe extern "C" fn(Pcm, *const c_void, std::ffi::c_ulong) -> c_long,
    recover: unsafe extern "C" fn(Pcm, c_int, c_int) -> c_int,
    delay: unsafe extern "C" fn(Pcm, *mut c_long) -> c_int,
    drop_: unsafe extern "C" fn(Pcm) -> c_int,
    prepare: unsafe extern "C" fn(Pcm) -> c_int,
    close: unsafe extern "C" fn(Pcm) -> c_int,
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
}

impl AlsaFns {
    fn load() -> Result<AlsaFns> {
        // SAFETY: loading libasound and resolving documented symbols with
        // their C signatures.
        unsafe {
            let lib = libloading::Library::new("libasound.so.2")
                .map_err(|e| Error::Audio(format!("libasound: {e}")))?;
            macro_rules! sym {
                ($n:literal) => {
                    *lib.get($n)
                        .map_err(|e| Error::Audio(format!("libasound symbol: {e}")))?
                };
            }
            Ok(AlsaFns {
                open: sym!(b"snd_pcm_open\0"),
                set_params: sym!(b"snd_pcm_set_params\0"),
                writei: sym!(b"snd_pcm_writei\0"),
                recover: sym!(b"snd_pcm_recover\0"),
                delay: sym!(b"snd_pcm_delay\0"),
                drop_: sym!(b"snd_pcm_drop\0"),
                prepare: sym!(b"snd_pcm_prepare\0"),
                close: sym!(b"snd_pcm_close\0"),
                strerror: sym!(b"snd_strerror\0"),
                _lib: lib,
            })
        }
    }

    fn err(&self, code: c_int) -> String {
        // SAFETY: snd_strerror returns a static string.
        unsafe {
            CStr::from_ptr((self.strerror)(code))
                .to_string_lossy()
                .into_owned()
        }
    }
}

pub struct Alsa {
    fns: AlsaFns,
    pcm: Pcm,
    channels: u32,
    device: String,
}

// SAFETY: the PCM handle is only used from the audio thread that owns it.
unsafe impl Send for Alsa {}

impl Alsa {
    pub fn open(device: &str, channels: u32, latency_ms: u32) -> Result<Alsa> {
        let fns = AlsaFns::load()?;
        let name = CString::new(device).map_err(|_| Error::Audio("bad device name".into()))?;
        let mut pcm: Pcm = std::ptr::null_mut();
        // SAFETY: valid out-pointer and NUL-terminated name.
        let r = unsafe { (fns.open)(&mut pcm, name.as_ptr(), SND_PCM_STREAM_PLAYBACK, 0) };
        if r < 0 {
            return Err(Error::Audio(format!("open {device}: {}", fns.err(r))));
        }
        // SAFETY: pcm is open.
        let r = unsafe {
            (fns.set_params)(
                pcm,
                SND_PCM_FORMAT_FLOAT_LE,
                SND_PCM_ACCESS_RW_INTERLEAVED,
                channels,
                super::RATE,
                1,
                latency_ms * 1000,
            )
        };
        if r < 0 {
            unsafe { (fns.close)(pcm) };
            return Err(Error::Audio(format!("configure {device}: {}", fns.err(r))));
        }
        Ok(Alsa {
            fns,
            pcm,
            channels,
            device: device.to_string(),
        })
    }
}

impl Sink for Alsa {
    fn write(&mut self, interleaved: &[f32]) -> Result<()> {
        let ch = self.channels as usize;
        let mut off = 0;
        while off < interleaved.len() {
            let frames = (interleaved.len() - off) / ch;
            // SAFETY: buffer holds `frames * ch` floats from `off`.
            let n = unsafe {
                (self.fns.writei)(
                    self.pcm,
                    interleaved[off..].as_ptr() as *const c_void,
                    frames as _,
                )
            };
            if n < 0 {
                // SAFETY: recover handles underrun/suspend.
                let r = unsafe { (self.fns.recover)(self.pcm, n as c_int, 1) };
                if r < 0 {
                    return Err(Error::Audio(self.fns.err(r)));
                }
                continue;
            }
            off += n as usize * ch;
        }
        Ok(())
    }
    fn delay(&mut self) -> f64 {
        let mut d: c_long = 0;
        // SAFETY: pcm open, valid out-pointer.
        if unsafe { (self.fns.delay)(self.pcm, &mut d) } < 0 {
            return 0.0;
        }
        d.max(0) as f64 / super::RATE as f64
    }
    fn flush(&mut self) {
        // SAFETY: pcm open.
        unsafe {
            (self.fns.drop_)(self.pcm);
            (self.fns.prepare)(self.pcm);
        }
    }
    fn name(&self) -> String {
        format!("alsa:{}", self.device)
    }
}

impl Drop for Alsa {
    fn drop(&mut self) {
        // SAFETY: closes our handle once.
        unsafe { (self.fns.close)(self.pcm) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_sink_paces_real_time() {
        let mut s = NullSink::new(2);
        let t = Instant::now();
        let chunk = vec![0.0f32; 2 * 4800]; // 100 ms
        for _ in 0..3 {
            s.write(&chunk).unwrap();
        }
        let el = t.elapsed().as_secs_f64();
        assert!(el > 0.2 && el < 0.6, "{el}");
        assert!(s.delay() <= 0.06);
        s.flush();
        assert_eq!(s.delay(), 0.0);
    }

    #[test]
    fn missing_alsa_device_falls_back() {
        let s = open(
            &Backend::Alsa {
                device: "no-such-device-xyz".into(),
                latency_ms: 50,
            },
            2,
        );
        assert_eq!(s.name(), "null");
    }
}
