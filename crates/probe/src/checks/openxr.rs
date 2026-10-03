//! OpenXR runtime: loader, runtime name/version, API layers, instance
//! extensions, HMD system properties, per-eye view sizes, blend modes,
//! Vulkan requirements, and which interaction-profile component paths the
//! runtime accepts (each binding suggested on its own, no session needed).
//! When a session is allowed it is created through fp-xr's real path
//! (`XrContext` → `VulkanContext` → `XrSession`) to read swapchain
//! formats, refresh rates and reference spaces.
//!
//! Answers P1, P6, P8, P10, P11, P12 and I11.

use crate::report::{CheckOutput, Status};
use crate::runner::CheckContext;
use crate::util::Out;
use fp_xr::bindings::{self, ActionKind, Profile};
use openxr as xr;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const STEREO: xr::ViewConfigurationType = xr::ViewConfigurationType::PRIMARY_STEREO;

/// The rest of Valve's published Frame controller profile (ValveSoftware/
/// Unity, `SteamFrameControllerProfile.cs`), beyond the paths fp-xr binds
/// (per hand suffixes; a path not valid for one hand is reported rejected).
pub const FRAME_EXTRA_CANDIDATES: &[&str] = &[
    "/input/system/click",
    "/input/system/touch",
    "/input/menu/touch",
    "/input/view/touch",
    "/input/squeeze/click",
    "/input/squeeze/touch",
    "/input/shoulder/touch",
    "/input/dpad_up/touch",
    "/input/dpad_down/touch",
    "/input/dpad_left/touch",
    "/input/dpad_right/touch",
];

/// Load the OpenXR loader the way fp-xr does (`libopenxr_loader.so`),
/// falling back to the versioned SONAME. Returns the entry and the name
/// that worked.
pub fn load_entry() -> Result<(xr::Entry, &'static str), String> {
    let mut errs = Vec::new();
    for name in ["libopenxr_loader.so", "libopenxr_loader.so.1"] {
        // SAFETY: loading the system OpenXR loader.
        match unsafe { xr::Entry::load_from(std::path::Path::new(name)) } {
            Ok(e) => return Ok((e, name)),
            Err(e) => errs.push(format!("{name}: {e}")),
        }
    }
    Err(errs.join("; "))
}

fn raw_extensions(entry: &xr::Entry) -> Vec<String> {
    let f = entry.fp().enumerate_instance_extension_properties;
    // SAFETY: two-call idiom with correctly typed output structs.
    unsafe {
        let mut n = 0u32;
        if f(std::ptr::null(), 0, &mut n, std::ptr::null_mut()).into_raw() < 0 {
            return Vec::new();
        }
        let blank = xr::sys::ExtensionProperties {
            ty: xr::sys::ExtensionProperties::TYPE,
            next: std::ptr::null_mut(),
            extension_name: [0; xr::sys::MAX_EXTENSION_NAME_SIZE],
            extension_version: 0,
        };
        let mut v = vec![blank; n as usize];
        if f(std::ptr::null(), n, &mut n, v.as_mut_ptr()).into_raw() < 0 {
            return Vec::new();
        }
        v.truncate(n as usize);
        v.iter()
            .map(|e| {
                let name = std::ffi::CStr::from_ptr(e.extension_name.as_ptr()).to_string_lossy();
                format!("{name} v{}", e.extension_version)
            })
            .collect()
    }
}

fn eye_gaze_supported(instance: &xr::Instance, system: xr::SystemId) -> Option<bool> {
    // SAFETY: properly chained output structs.
    unsafe {
        let mut eg = xr::sys::SystemEyeGazeInteractionPropertiesEXT {
            ty: xr::sys::SystemEyeGazeInteractionPropertiesEXT::TYPE,
            next: std::ptr::null_mut(),
            supports_eye_gaze_interaction: xr::sys::FALSE,
        };
        let mut p: xr::sys::SystemProperties = std::mem::zeroed();
        p.ty = xr::sys::SystemProperties::TYPE;
        p.next = &mut eg as *mut _ as *mut _;
        let r = (instance.fp().get_system_properties)(instance.as_raw(), system, &mut p);
        (r.into_raw() >= 0).then(|| eg.supports_eye_gaze_interaction.into())
    }
}

/// Paths to test for `profile`: fp-xr's full binding list plus, for the
/// Frame profile, extra candidates on both hands. De-duplicated.
pub fn candidate_paths(profile: Profile) -> Vec<String> {
    let mut v: Vec<String> = bindings::bindings(profile)
        .into_iter()
        .map(|(_, p)| p.to_string())
        .collect();
    if profile == Profile::Frame {
        for hand in [bindings::LEFT, bindings::RIGHT] {
            for s in FRAME_EXTRA_CANDIDATES {
                v.push(format!("{hand}{s}"));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    v.retain(|p| seen.insert(p.clone()));
    v
}

/// Compress `/user/hand/left/input/a/click` to `L a/click`.
pub fn short_path(p: &str) -> String {
    let p = p
        .replace("/user/hand/left/input/", "L ")
        .replace("/user/hand/right/input/", "R ")
        .replace("/user/hand/left/output/", "L out/")
        .replace("/user/hand/right/output/", "R out/");
    p.replace("/user/eyes_ext/input/", "eyes ")
}

struct ProbeActions {
    b: xr::Action<bool>,
    f: xr::Action<f32>,
    v: xr::Action<xr::Vector2f>,
    p: xr::Action<xr::Posef>,
    h: xr::Action<xr::Haptic>,
    _set: xr::ActionSet,
}

fn suggest_one(
    instance: &xr::Instance,
    a: &ProbeActions,
    profile: &str,
    path: &str,
) -> Result<(), String> {
    let prof = instance
        .string_to_path(profile)
        .map_err(|e| format!("{e:?}"))?;
    let p = instance
        .string_to_path(path)
        .map_err(|e| format!("{e:?}"))?;
    let kind = bindings::kind_for_path(path).unwrap_or(ActionKind::Bool);
    let b = match kind {
        ActionKind::Bool => xr::Binding::new(&a.b, p),
        ActionKind::Float => xr::Binding::new(&a.f, p),
        ActionKind::Vec2 => xr::Binding::new(&a.v, p),
        ActionKind::Pose => xr::Binding::new(&a.p, p),
        ActionKind::Haptic => xr::Binding::new(&a.h, p),
    };
    instance
        .suggest_interaction_profile_bindings(prof, &[b])
        .map_err(|e| format!("{e:?}"))
}

fn binding_tests(instance: &xr::Instance, eye_gaze: bool) -> Result<Value, String> {
    let set = instance
        .create_action_set("fp_probe", "FramePlayer probe", 0)
        .map_err(|e| format!("create_action_set: {e:?}"))?;
    let mk = |e: xr::sys::Result| format!("create_action: {e:?}");
    let a = ProbeActions {
        b: set.create_action("probe_bool", "bool", &[]).map_err(mk)?,
        f: set.create_action("probe_float", "float", &[]).map_err(mk)?,
        v: set.create_action("probe_vec2", "vec2", &[]).map_err(mk)?,
        p: set.create_action("probe_pose", "pose", &[]).map_err(mk)?,
        h: set
            .create_action("probe_haptic", "haptic", &[])
            .map_err(mk)?,
        _set: set,
    };
    let mut out = serde_json::Map::new();
    let mut profiles: Vec<(String, Vec<String>)> = Profile::ALL
        .iter()
        .map(|p| (p.path().to_string(), candidate_paths(*p)))
        .collect();
    if eye_gaze {
        profiles.push((
            bindings::EYE_GAZE_PROFILE.to_string(),
            vec![bindings::EYE_GAZE_POSE.to_string()],
        ));
    }
    for (profile, paths) in profiles {
        let mut ok = Vec::new();
        let mut rejected = Vec::new();
        for path in &paths {
            match suggest_one(instance, &a, &profile, path) {
                Ok(()) => ok.push(short_path(path)),
                Err(e) => rejected.push(format!(
                    "{} ({})",
                    short_path(path),
                    e.replace("ERROR_", "")
                )),
            }
        }
        // Rejections are usually all the same error; collapse them.
        let common = common_error(&rejected);
        let rejected_v = match &common {
            Some(err) => {
                json!({ "error": err, "paths": rejected.iter().map(|r| r.split(" (").next().unwrap_or(r).to_string()).collect::<Vec<_>>() })
            }
            None => json!(rejected),
        };
        let fp_bound: Vec<String> = Profile::from_path(&profile)
            .map(|p| {
                bindings::bindings(p)
                    .into_iter()
                    .map(|(_, s)| short_path(s))
                    .collect()
            })
            .unwrap_or_default();
        out.insert(
            profile.clone(),
            json!({
                "accepted": ok,
                "rejected": rejected_v,
                "fp_xr_bindings_all_accepted": fp_bound.iter().all(|c| ok.contains(c)),
            }),
        );
    }
    Ok(Value::Object(out))
}

fn common_error(rejected: &[String]) -> Option<String> {
    let errs: Vec<&str> = rejected
        .iter()
        .filter_map(|r| r.rsplit_once(" (").map(|x| x.1.trim_end_matches(')')))
        .collect();
    let first = *errs.first()?;
    errs.iter().all(|e| *e == first).then(|| first.to_string())
}

pub fn run(ctx: &CheckContext) -> CheckOutput {
    let mut o = Out::new();
    let (entry, loader_name) = match load_entry() {
        Ok(x) => x,
        Err(e) => {
            o.finding(
                "openxr_loader",
                Status::Unknown,
                &["I11"],
                format!("no OpenXR loader: {e}"),
            );
            return o.finish(Status::Unknown, "OpenXR loader not available");
        }
    };
    o.set("loader", loader_name);
    if loader_name != "libopenxr_loader.so" {
        o.finding(
            "fp_xr_loader_name",
            Status::Fail,
            &["I11"],
            "only libopenxr_loader.so.1 is loadable; fp-xr's Entry::load() (libopenxr_loader.so) would fail",
        );
    }
    let exts = raw_extensions(&entry);
    // One space-separated string: compact in the pretty-printed report.
    o.set("instance_extensions", exts.join(", "));
    let layers: Vec<String> = entry
        .enumerate_layers()
        .unwrap_or_default()
        .into_iter()
        .map(|l| l.layer_name)
        .collect();
    o.set("api_layers", &layers);
    let avail = match entry.enumerate_extensions() {
        Ok(a) => a,
        Err(e) => {
            o.finding(
                "runtime",
                Status::Fail,
                &["I11"],
                format!("xrEnumerateInstanceExtensionProperties: {e:?} (no active runtime?)"),
            );
            return o.finish(Status::Fail, format!("no OpenXR runtime reachable: {e:?}"));
        }
    };
    let has = |n: &str| exts.iter().any(|e| e.split(' ').next() == Some(n));
    for (id, name, refs) in [
        ("ext_vulkan_enable2", "XR_KHR_vulkan_enable2", &["P12"][..]),
        ("ext_eye_gaze", "XR_EXT_eye_gaze_interaction", &["P11"][..]),
        (
            "ext_refresh_rate",
            "XR_FB_display_refresh_rate",
            &["P1"][..],
        ),
        (
            "ext_cylinder",
            "XR_KHR_composition_layer_cylinder",
            &["P13"][..],
        ),
    ] {
        o.finding(
            id,
            if has(name) {
                Status::Pass
            } else {
                Status::Fail
            },
            refs,
            format!(
                "{name} {}",
                if has(name) { "offered" } else { "NOT offered" }
            ),
        );
    }
    let foveation: Vec<&String> = exts
        .iter()
        .filter(|e| e.contains("foveat") || e.contains("FOVEAT"))
        .collect();
    o.finding(
        "ext_foveation",
        if foveation.is_empty() {
            Status::Fail
        } else {
            Status::Pass
        },
        &["P8"],
        if foveation.is_empty() {
            "no foveation extension offered".to_string()
        } else {
            format!(
                "foveation: {}",
                foveation
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    );
    let passthrough: Vec<&String> = exts
        .iter()
        .filter(|e| e.to_lowercase().contains("passthrough"))
        .collect();
    o.set("passthrough_extensions", &passthrough);

    let mut enable = xr::ExtensionSet::default();
    enable.khr_vulkan_enable2 = avail.khr_vulkan_enable2;
    enable.ext_eye_gaze_interaction = avail.ext_eye_gaze_interaction;
    enable.ext_hand_tracking = avail.ext_hand_tracking;
    enable.fb_display_refresh_rate = avail.fb_display_refresh_rate;
    enable.khr_composition_layer_cylinder = avail.khr_composition_layer_cylinder;
    enable.khr_convert_timespec_time = avail.khr_convert_timespec_time;
    // The Frame controller profile only exists with its extension enabled.
    let frame_ext = fp_xr::extensions::ExtensionReport::from_available(&avail).frame_controller;
    if frame_ext {
        enable
            .other
            .push(bindings::FRAME_CONTROLLER_EXTENSION.to_string());
    }
    o.set("frame_controller_extension", frame_ext);
    let app = xr::ApplicationInfo {
        application_name: "frameplayer-probe",
        application_version: 1,
        engine_name: "fp-probe",
        engine_version: 1,
        api_version: xr::Version::new(1, 0, 0),
    };
    let instance = match entry.create_instance(&app, &enable, &[]) {
        Ok(i) => i,
        Err(e) => {
            o.finding(
                "runtime",
                Status::Fail,
                &["I11"],
                format!("xrCreateInstance failed: {e:?}"),
            );
            return o.finish(Status::Fail, format!("xrCreateInstance failed: {e:?}"));
        }
    };
    let rt = instance
        .properties()
        .map(|p| {
            format!(
                "{} {}.{}.{}",
                p.runtime_name,
                p.runtime_version.major(),
                p.runtime_version.minor(),
                p.runtime_version.patch()
            )
        })
        .unwrap_or_else(|e| format!("? ({e:?})"));
    o.set("runtime", &rt);
    o.finding(
        "runtime",
        Status::Pass,
        &["I11"],
        format!(
            "OpenXR runtime {rt} reachable{}",
            if std::env::var_os("SSH_CONNECTION").is_some() {
                " from an SSH session"
            } else {
                ""
            }
        ),
    );
    ctx.partial(&o.snapshot(Status::Unknown, "OpenXR instance created"));

    let system = match instance.system(xr::FormFactor::HEAD_MOUNTED_DISPLAY) {
        Ok(s) => Some(s),
        Err(e) => {
            o.finding(
                "hmd_system",
                Status::Fail,
                &["P1"],
                format!("xrGetSystem(HMD): {e:?} (headset asleep or not ready?)"),
            );
            None
        }
    };
    let mut summary_bits = vec![rt.clone()];
    if let Some(system) = system {
        if let Ok(p) = instance.system_properties(system) {
            o.set(
                "system",
                json!({
                    "name": p.system_name,
                    "vendor_id": p.vendor_id,
                    "max_swapchain": format!("{}x{}", p.graphics_properties.max_swapchain_image_width, p.graphics_properties.max_swapchain_image_height),
                    "max_layers": p.graphics_properties.max_layer_count,
                    "orientation_tracking": p.tracking_properties.orientation_tracking,
                    "position_tracking": p.tracking_properties.position_tracking,
                }),
            );
            summary_bits.push(format!("system '{}'", p.system_name));
        }
        let hand = if enable.ext_hand_tracking {
            instance.supports_hand_tracking(system).ok()
        } else {
            Some(false)
        };
        let gaze = if enable.ext_eye_gaze_interaction {
            eye_gaze_supported(&instance, system)
        } else {
            Some(false)
        };
        let eye_gaze = gaze == Some(true);
        o.set("hand_tracking_supported", hand);
        o.set("eye_gaze_supported", gaze);
        o.finding(
            "eye_gaze_system",
            if eye_gaze { Status::Pass } else { Status::Fail },
            &["P11"],
            format!(
                "system reports eye-gaze interaction support: {}",
                gaze.map_or("?".into(), |g| g.to_string())
            ),
        );
        o.finding(
            "hand_tracking_system",
            if hand == Some(true) {
                Status::Pass
            } else {
                Status::Fail
            },
            &[],
            format!(
                "system reports hand tracking support: {}",
                hand.map_or("?".into(), |g| g.to_string())
            ),
        );
        let configs = instance
            .enumerate_view_configurations(system)
            .unwrap_or_default();
        o.set("view_configurations", format!("{configs:?}"));
        match instance.enumerate_view_configuration_views(system, STEREO) {
            Ok(v) if !v.is_empty() => {
                let rec = format!(
                    "{}x{}",
                    v[0].recommended_image_rect_width, v[0].recommended_image_rect_height
                );
                o.set(
                    "views",
                    v.iter()
                        .map(|x| {
                            format!(
                                "recommended {}x{} max {}x{} samples {}/{}",
                                x.recommended_image_rect_width,
                                x.recommended_image_rect_height,
                                x.max_image_rect_width,
                                x.max_image_rect_height,
                                x.recommended_swapchain_sample_count,
                                x.max_swapchain_sample_count
                            )
                        })
                        .collect::<Vec<_>>(),
                );
                o.finding(
                    "eye_resolution",
                    Status::Pass,
                    &["P1"],
                    format!(
                        "recommended per-eye {rec}, max {}x{}",
                        v[0].max_image_rect_width, v[0].max_image_rect_height
                    ),
                );
                summary_bits.push(format!("eye {rec}"));
            }
            other => o.set("views_error", format!("{:?}", other.err())),
        }
        match instance.enumerate_environment_blend_modes(system, STEREO) {
            Ok(m) => {
                let alpha = m.contains(&xr::EnvironmentBlendMode::ALPHA_BLEND);
                o.set("blend_modes", format!("{m:?}"));
                o.finding(
                    "blend_modes",
                    if alpha { Status::Pass } else { Status::Fail },
                    &["P6"],
                    format!(
                        "environment blend modes {m:?}: ALPHA_BLEND {}",
                        if alpha { "available" } else { "not offered" }
                    ),
                );
            }
            Err(e) => o.set("blend_modes_error", format!("{e:?}")),
        }
        if enable.khr_vulkan_enable2 {
            match instance.graphics_requirements::<xr::Vulkan>(system) {
                Ok(r) => o.set(
                    "vulkan_requirements",
                    format!(
                        "min {}.{} max {}.{}",
                        r.min_api_version_supported.major(),
                        r.min_api_version_supported.minor(),
                        r.max_api_version_supported.major(),
                        r.max_api_version_supported.minor()
                    ),
                ),
                Err(e) => o.set("vulkan_requirements", format!("{e:?}")),
            }
        }
    }

    // Binding paths (no session needed).
    match binding_tests(&instance, enable.ext_eye_gaze_interaction) {
        Ok(v) => {
            let frame = &v[Profile::Frame.path()];
            let accepted = frame["accepted"].as_array().map_or(0, |a| a.len());
            let all_ok = frame["fp_xr_bindings_all_accepted"] == true;
            o.finding(
                "frame_profile",
                if accepted > 0 { Status::Pass } else { Status::Fail },
                &["P10"],
                if accepted == 0 {
                    format!(
                        "runtime rejects every path of {} ({})",
                        Profile::Frame.path(),
                        frame["rejected"]["error"].as_str().unwrap_or("various errors")
                    )
                } else {
                    format!(
                        "{} accepts {accepted} component paths (all fp-xr bindings accepted: {all_ok}); see data.bindings",
                        Profile::Frame.path()
                    )
                },
            );
            o.set("bindings", v);
        }
        Err(e) => o.set("bindings_error", e),
    }
    ctx.partial(&o.snapshot(Status::Unknown, "instance-level OpenXR facts gathered"));
    drop(instance);

    // Session-level facts through fp-xr.
    if ctx.session_allowed() {
        session_facts(&mut o);
    } else {
        o.set(
            "session",
            "skipped in headless mode (creating a session can take over the headset display); run without --headless or add --with-session",
        );
    }
    let status = if system.is_some() {
        Status::Pass
    } else {
        Status::Fail
    };
    o.finish(status, summary_bits.join(", "))
}

fn session_facts(o: &mut Out) {
    let ctx = match fp_xr::XrContext::new(fp_xr::XrConfig {
        app_name: "frameplayer-probe".into(),
        ..Default::default()
    }) {
        Ok(c) => c,
        Err(e) => {
            o.finding(
                "session",
                Status::Fail,
                &["P12"],
                format!("fp-xr XrContext::new failed: {e}"),
            );
            return;
        }
    };
    let vk = match fp_xr::VulkanContext::new(&ctx) {
        Ok(v) => v,
        Err(e) => {
            o.finding(
                "session",
                Status::Fail,
                &["P12"],
                format!("fp-xr VulkanContext::new failed: {e}"),
            );
            return;
        }
    };
    o.set(
        "xr_vulkan_device",
        json!({ "name": vk.device_name, "zero_copy_exts": format!("{:?}", vk.device_extensions), "ycbcr": vk.sampler_ycbcr_conversion }),
    );
    let mut session = match fp_xr::XrSession::new(&ctx, &vk) {
        Ok(s) => s,
        Err(e) => {
            o.finding(
                "session",
                Status::Fail,
                &["P12"],
                format!("fp-xr XrSession::new failed: {e}"),
            );
            return;
        }
    };
    let raw = session.raw();
    let formats: Vec<String> = raw
        .enumerate_swapchain_formats()
        .unwrap_or_default()
        .iter()
        .map(|&f| format!("{:?}", ash::vk::Format::from_raw(f as i32)))
        .collect();
    let spaces = raw
        .enumerate_reference_spaces()
        .map(|s| format!("{s:?}"))
        .unwrap_or_default();
    let rates = session.refresh_rates().to_vec();
    let current = session.current_refresh_rate();
    o.set(
        "session",
        json!({
            "swapchain_formats": formats,
            "reference_spaces": spaces,
            "refresh_rates": rates,
            "current_refresh_rate": current,
            "blend_mode_chosen": format!("{:?}", session.blend_mode()),
        }),
    );
    o.finding(
        "swapchain_formats",
        if formats.is_empty() {
            Status::Fail
        } else {
            Status::Pass
        },
        &["P12"],
        format!("swapchain formats: {}", formats.join(", ")),
    );
    o.finding(
        "refresh_rates",
        if rates.is_empty() {
            Status::Fail
        } else {
            Status::Pass
        },
        &["P1"],
        format!("refresh rates offered: {rates:?} Hz (current {current:?})"),
    );
    match session.create_stereo_swapchain() {
        Ok(sc) => o.set(
            "stereo_swapchain",
            format!(
                "{}x{} {:?} x{} images",
                sc.width,
                sc.height,
                sc.format,
                sc.images.len()
            ),
        ),
        Err(e) => o.set("stereo_swapchain", format!("failed: {e}")),
    }
    // Watch the lifecycle briefly (fp-xr begins the session on READY).
    let mut states = Vec::new();
    let mut events = Vec::new();
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        events.clear();
        if session.poll_events(&mut events).is_err() {
            break;
        }
        for e in &events {
            states.push(format!("{e:?}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = session.request_exit();
    o.set("session_events", &states);
    o.finding(
        "session",
        Status::Pass,
        &["P12"],
        "fp-xr created an XR session on its Vulkan device",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_cover_fp_xr_bindings() {
        let c = candidate_paths(Profile::Frame);
        for (_, p) in bindings::bindings(Profile::Frame) {
            assert!(c.iter().any(|x| x == p), "{p}");
        }
        let unique: std::collections::HashSet<_> = c.iter().collect();
        assert_eq!(unique.len(), c.len());
        for p in &c {
            assert!(p.starts_with("/user/hand/"), "{p}");
        }
        assert!(!candidate_paths(Profile::Simple)
            .iter()
            .any(|p| p.contains("thumbrest")));
    }

    #[test]
    fn path_shortening_and_errors() {
        assert_eq!(short_path("/user/hand/left/input/a/click"), "L a/click");
        assert_eq!(short_path("/user/hand/right/output/haptic"), "R out/haptic");
        assert_eq!(
            short_path("/user/eyes_ext/input/gaze_ext/pose"),
            "eyes gaze_ext/pose"
        );
        let r = vec![
            "L a/click (PATH_UNSUPPORTED)".to_string(),
            "R b/click (PATH_UNSUPPORTED)".to_string(),
        ];
        assert_eq!(common_error(&r).as_deref(), Some("PATH_UNSUPPORTED"));
        let r2 = vec!["L a (X)".to_string(), "L b (Y)".to_string()];
        assert_eq!(common_error(&r2), None);
        assert_eq!(common_error(&[]), None);
    }
}
