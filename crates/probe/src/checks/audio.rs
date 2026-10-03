//! Audio output: ALSA cards, PipeWire/Pulse sockets, and the ALSA
//! `default` PCM that cpal opens (talked to through `dlopen("libasound")`
//! so the default build needs no ALSA headers). With the `cpal` feature
//! the cpal device list and default config are reported too. Answers P15.
//! Nothing is played.

use crate::report::{CheckOutput, Status};
use crate::runner::CheckContext;
use crate::util::{self, Out};
use serde_json::{json, Value};
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CStr, CString};
use std::path::PathBuf;

pub fn run(_ctx: &CheckContext) -> CheckOutput {
    let mut o = Out::new();
    let cards: Vec<String> = util::read_trim("/proc/asound/cards")
        .map(|s| {
            s.lines()
                .filter(|l| {
                    l.trim_start()
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_digit())
                })
                .map(|l| l.trim().chars().take(100).collect())
                .collect()
        })
        .unwrap_or_default();
    o.set("alsa_cards", &cards);
    let rt = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // SAFETY: getuid never fails.
            PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }))
        });
    let pw = rt.join("pipewire-0").exists();
    let pulse = rt.join("pulse/native").exists();
    o.set(
        "sound_servers",
        json!({ "pipewire_socket": pw, "pulse_socket": pulse }),
    );

    let alsa = alsa_probe();
    o.set("alsa_default_pcm", &alsa);
    if alsa["loaded"] == false {
        o.finding(
            "alsa_default",
            Status::Unknown,
            &["P15"],
            format!(
                "libasound.so.2 not loadable: {}",
                alsa["error"].as_str().unwrap_or("?")
            ),
        );
    } else if alsa["opened"] == true {
        o.finding(
            "alsa_default",
            Status::Pass,
            &["P15"],
            format!(
                "ALSA 'default' PCM opens for playback: {} Hz, buffer {} frames, period {} frames{}",
                alsa["rate"],
                alsa["buffer_frames"],
                alsa["period_frames"],
                if pw { " (PipeWire running)" } else { "" }
            ),
        );
    } else {
        o.finding(
            "alsa_default",
            Status::Fail,
            &["P15"],
            format!(
                "ALSA 'default' PCM failed: {} (pipewire socket: {pw})",
                alsa["error"].as_str().unwrap_or("?")
            ),
        );
    }

    #[cfg(feature = "cpal")]
    o.set("cpal", cpal_probe());
    #[cfg(not(feature = "cpal"))]
    o.set("cpal", "not compiled in (feature `cpal`)");

    let status = crate::checks::combine(&o);
    let summary = match o.status_of("alsa_default") {
        Some(Status::Pass) => format!(
            "default output opens at {} Hz; {} ALSA card(s); PipeWire {}",
            alsa["rate"],
            cards.len(),
            if pw { "yes" } else { "no" }
        ),
        _ => format!(
            "{} ALSA card(s); PipeWire {}; default PCM not usable",
            cards.len(),
            if pw { "yes" } else { "no" }
        ),
    };
    o.finish(status, summary)
}

type PcmOpen = unsafe extern "C" fn(*mut *mut c_void, *const c_char, c_int, c_int) -> c_int;
type PcmClose = unsafe extern "C" fn(*mut c_void) -> c_int;
type HwMalloc = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type HwFree = unsafe extern "C" fn(*mut c_void);
type HwAny = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;
type HwSetInt = unsafe extern "C" fn(*mut c_void, *mut c_void, c_uint) -> c_int;
type HwSetRateNear =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_uint, *mut c_int) -> c_int;
type HwGetUlong = unsafe extern "C" fn(*const c_void, *mut c_ulong) -> c_int;
type HwGetPeriod = unsafe extern "C" fn(*const c_void, *mut c_ulong, *mut c_int) -> c_int;
type StrError = unsafe extern "C" fn(c_int) -> *const c_char;
type NameHint = unsafe extern "C" fn(c_int, *const c_char, *mut *mut *mut c_void) -> c_int;
type NameGetHint = unsafe extern "C" fn(*const c_void, *const c_char) -> *mut c_char;
type NameFreeHint = unsafe extern "C" fn(*mut *mut c_void) -> c_int;

const SND_PCM_STREAM_PLAYBACK: c_int = 0;
const SND_PCM_NONBLOCK: c_int = 1;
const SND_PCM_ACCESS_RW_INTERLEAVED: c_uint = 3;
const SND_PCM_FORMAT_FLOAT_LE: c_uint = 14;

struct Lib(*mut c_void);

impl Lib {
    fn sym<T: Copy>(&self, name: &str) -> Option<T> {
        let c = CString::new(name).ok()?;
        // SAFETY: dlsym on a live handle; T is the matching fn pointer type.
        unsafe {
            let p = libc::dlsym(self.0, c.as_ptr());
            (!p.is_null()).then(|| std::mem::transmute_copy::<*mut c_void, T>(&p))
        }
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        // SAFETY: handle from dlopen.
        unsafe { libc::dlclose(self.0) };
    }
}

/// Open the ALSA `default` playback PCM, negotiate 48 kHz stereo f32 and
/// read back what was granted; then close it again. Also lists PCM names.
fn alsa_probe() -> Value {
    let name = CString::new("libasound.so.2").unwrap();
    // SAFETY: dlopen of a system library.
    let h = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if h.is_null() {
        let p = util::dlopen_probe("libasound.so.2");
        return json!({ "loaded": false, "error": p.error });
    }
    let lib = Lib(h);
    let (Some(open), Some(close), Some(hw_malloc), Some(hw_free), Some(hw_any)) = (
        lib.sym::<PcmOpen>("snd_pcm_open"),
        lib.sym::<PcmClose>("snd_pcm_close"),
        lib.sym::<HwMalloc>("snd_pcm_hw_params_malloc"),
        lib.sym::<HwFree>("snd_pcm_hw_params_free"),
        lib.sym::<HwAny>("snd_pcm_hw_params_any"),
    ) else {
        return json!({ "loaded": true, "opened": false, "error": "libasound lacks expected symbols" });
    };
    let set_access = lib.sym::<HwSetInt>("snd_pcm_hw_params_set_access");
    let set_format = lib.sym::<HwSetInt>("snd_pcm_hw_params_set_format");
    let set_channels = lib.sym::<HwSetInt>("snd_pcm_hw_params_set_channels");
    let set_rate = lib.sym::<HwSetRateNear>("snd_pcm_hw_params_set_rate_near");
    let apply = lib.sym::<HwAny>("snd_pcm_hw_params");
    let get_buffer = lib.sym::<HwGetUlong>("snd_pcm_hw_params_get_buffer_size");
    let get_period = lib.sym::<HwGetPeriod>("snd_pcm_hw_params_get_period_size");
    let strerror = lib.sym::<StrError>("snd_strerror");
    let err = |rc: c_int| -> String {
        match strerror {
            // SAFETY: snd_strerror returns a static string.
            Some(f) => unsafe { CStr::from_ptr(f(rc)).to_string_lossy().into_owned() },
            None => format!("error {rc}"),
        }
    };
    let names = pcm_names(&lib);

    let dev = CString::new("default").unwrap();
    let mut pcm: *mut c_void = std::ptr::null_mut();
    // SAFETY: valid out-pointer and C string.
    let rc = unsafe {
        open(
            &mut pcm,
            dev.as_ptr(),
            SND_PCM_STREAM_PLAYBACK,
            SND_PCM_NONBLOCK,
        )
    };
    if rc < 0 {
        return json!({ "loaded": true, "opened": false, "error": err(rc), "pcm_names": names });
    }
    let mut hw: *mut c_void = std::ptr::null_mut();
    let mut result = json!({ "loaded": true, "opened": true, "pcm_names": names });
    // SAFETY: pcm and hw are live ALSA objects for the duration of this block.
    unsafe {
        if hw_malloc(&mut hw) == 0 && hw_any(pcm, hw) >= 0 {
            let mut steps = Vec::new();
            if let Some(f) = set_access {
                steps.push(("access", f(pcm, hw, SND_PCM_ACCESS_RW_INTERLEAVED)));
            }
            if let Some(f) = set_format {
                steps.push(("format_f32", f(pcm, hw, SND_PCM_FORMAT_FLOAT_LE)));
            }
            if let Some(f) = set_channels {
                steps.push(("channels_2", f(pcm, hw, 2)));
            }
            let mut rate: c_uint = 48_000;
            let mut dir: c_int = 0;
            if let Some(f) = set_rate {
                steps.push(("rate", f(pcm, hw, &mut rate, &mut dir)));
            }
            if let Some(f) = apply {
                steps.push(("hw_params", f(pcm, hw)));
            }
            let mut buf: c_ulong = 0;
            let mut per: c_ulong = 0;
            if let Some(f) = get_buffer {
                f(hw, &mut buf);
            }
            if let Some(f) = get_period {
                f(hw, &mut per, &mut dir);
            }
            let failed: Vec<String> = steps
                .iter()
                .filter(|(_, rc)| *rc < 0)
                .map(|(n, rc)| format!("{n}: {}", err(*rc)))
                .collect();
            result["rate"] = json!(rate);
            result["buffer_frames"] = json!(buf);
            result["period_frames"] = json!(per);
            result["latency_ms"] = json!(if rate > 0 {
                buf as f64 * 1000.0 / rate as f64
            } else {
                0.0
            });
            if !failed.is_empty() {
                result["opened"] = json!(false);
                result["error"] = json!(failed.join("; "));
            }
            hw_free(hw);
        }
        close(pcm);
    }
    result
}

fn pcm_names(lib: &Lib) -> Vec<String> {
    let (Some(hint), Some(get), Some(free)) = (
        lib.sym::<NameHint>("snd_device_name_hint"),
        lib.sym::<NameGetHint>("snd_device_name_get_hint"),
        lib.sym::<NameFreeHint>("snd_device_name_free_hint"),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let iface = CString::new("pcm").unwrap();
    let key = CString::new("NAME").unwrap();
    let ioid = CString::new("IOID").unwrap();
    // SAFETY: ALSA hint API; strings returned by get_hint are malloc'ed.
    unsafe {
        let mut hints: *mut *mut c_void = std::ptr::null_mut();
        if hint(-1, iface.as_ptr(), &mut hints) < 0 || hints.is_null() {
            return out;
        }
        let mut p = hints;
        while !(*p).is_null() && out.len() < 24 {
            let n = get(*p, key.as_ptr());
            let io = get(*p, ioid.as_ptr());
            let input_only = !io.is_null() && CStr::from_ptr(io).to_bytes() == b"Input";
            if !n.is_null() {
                if !input_only {
                    out.push(
                        CStr::from_ptr(n)
                            .to_string_lossy()
                            .chars()
                            .take(60)
                            .collect(),
                    );
                }
                libc::free(n as *mut c_void);
            }
            if !io.is_null() {
                libc::free(io as *mut c_void);
            }
            p = p.add(1);
        }
        free(hints);
    }
    out
}

#[cfg(feature = "cpal")]
fn cpal_probe() -> Value {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let devices: Vec<String> = host
        .output_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).take(24).collect())
        .unwrap_or_default();
    let default = host.default_output_device();
    let cfg = default
        .as_ref()
        .and_then(|d| d.default_output_config().ok());
    json!({
        "host": format!("{:?}", host.id()),
        "devices": devices,
        "default_device": default.and_then(|d| d.name().ok()),
        "default_config": cfg.map(|c| json!({
            "sample_rate": c.sample_rate().0,
            "channels": c.channels(),
            "format": format!("{:?}", c.sample_format()),
            "buffer": format!("{:?}", c.buffer_size()),
        })),
    })
}
