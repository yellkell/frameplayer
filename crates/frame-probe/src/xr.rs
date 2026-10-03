//! OpenXR: runtime identity, extensions, system, views, blend modes, input
//! profiles, and (with a short-lived Vulkan session) swapchain formats,
//! reference spaces and display refresh rates.

use crate::util::fixed_cstr;
use crate::xr_loader::{self, RuntimeManifest};
use ash::vk::{self, Handle};
use openxr as xr;
use serde::Serialize;
use std::ffi::CString;

#[derive(Serialize, Default)]
pub struct XrReport {
    pub manifest: Option<RuntimeManifest>,
    pub runtime_interface_version: Option<u32>,
    pub negotiated_api_version: Option<String>,
    pub extensions: Vec<(String, u32)>,
    pub api_layers: Vec<String>,
    pub instance_api_version: Option<String>,
    pub runtime_name: Option<String>,
    pub runtime_version: Option<String>,
    pub enabled_extensions: Vec<String>,
    pub system: Option<SystemInfo>,
    pub interaction_profiles: Vec<ProfileProbe>,
    pub session: Option<SessionInfo>,
    pub errors: Vec<String>,
}

#[derive(Serialize, Default)]
pub struct SystemInfo {
    pub name: String,
    pub vendor_id: u32,
    pub max_swapchain_width: u32,
    pub max_swapchain_height: u32,
    pub max_layer_count: u32,
    pub orientation_tracking: bool,
    pub position_tracking: bool,
    pub view_configurations: Vec<String>,
    pub stereo_views: Vec<ViewInfo>,
    pub blend_modes: Vec<String>,
    pub hand_tracking_supported: Option<bool>,
    pub eye_gaze_supported: Option<bool>,
    pub vulkan_api_range: Option<(String, String)>,
}

#[derive(Serialize)]
pub struct ViewInfo {
    pub recommended: (u32, u32),
    pub max: (u32, u32),
    pub recommended_samples: u32,
    pub max_samples: u32,
}

#[derive(Serialize)]
pub struct ProfileProbe {
    pub profile: String,
    pub accepted: Vec<String>,
    pub rejected: Vec<(String, String)>,
}

#[derive(Serialize, Default)]
pub struct SessionInfo {
    pub vulkan_device: Option<String>,
    pub vulkan_driver: Option<String>,
    pub swapchain_formats: Vec<String>,
    pub reference_spaces: Vec<String>,
    pub refresh_rates: Option<Vec<f32>>,
    pub current_refresh_rate: Option<f32>,
    pub color_spaces: Option<Vec<String>>,
    pub errors: Vec<String>,
}

/// Extensions FramePlayer would use, enabled when present so their system
/// properties and session functions can be queried.
fn wanted(avail: &xr::ExtensionSet) -> xr::ExtensionSet {
    let mut e = xr::ExtensionSet::default();
    e.khr_vulkan_enable2 = avail.khr_vulkan_enable2;
    e.khr_composition_layer_cylinder = avail.khr_composition_layer_cylinder;
    e.khr_composition_layer_equirect2 = avail.khr_composition_layer_equirect2;
    e.khr_convert_timespec_time = avail.khr_convert_timespec_time;
    e.khr_visibility_mask = avail.khr_visibility_mask;
    e.fb_display_refresh_rate = avail.fb_display_refresh_rate;
    e.fb_color_space = avail.fb_color_space;
    e.ext_eye_gaze_interaction = avail.ext_eye_gaze_interaction;
    e.ext_hand_tracking = avail.ext_hand_tracking;
    e.ext_hand_interaction = avail.ext_hand_interaction;
    if avail
        .other
        .iter()
        .any(|n| n.as_slice() == FRAME_CONTROLLER_EXTENSION)
    {
        e.other.push(FRAME_CONTROLLER_EXTENSION.to_vec());
    }
    // XR_EXT_dpad_binding is only valid together with XR_KHR_binding_modification.
    e.ext_dpad_binding = avail.ext_dpad_binding && avail.khr_binding_modification;
    e.khr_binding_modification = e.ext_dpad_binding;
    e
}

/// OpenXR result as its spec name (e.g. `XR_ERROR_PATH_UNSUPPORTED`).
fn xe(r: xr::sys::Result) -> String {
    format!("XR_{r:?}")
}

pub fn probe(want_session: bool) -> XrReport {
    let mut r = XrReport::default();
    let manifest = match xr_loader::find_active_runtime(&xr_loader::SearchEnv::from_process()) {
        Ok(m) => m,
        Err(e) => {
            r.errors.push(e);
            return r;
        }
    };
    r.manifest = Some(manifest.clone());
    let loaded = match xr_loader::load_runtime(&manifest) {
        Ok(l) => l,
        Err(e) => {
            r.errors.push(e);
            return r;
        }
    };
    r.runtime_interface_version = Some(loaded.runtime_interface_version);
    r.negotiated_api_version = Some(loaded.runtime_api_version.clone());
    let entry = loaded.entry;

    r.extensions = raw_extensions(&entry).unwrap_or_else(|e| {
        r.errors
            .push(format!("xrEnumerateInstanceExtensionProperties: {}", xe(e)));
        Vec::new()
    });
    r.api_layers = entry
        .enumerate_layers()
        .map(|l| l.into_iter().map(|l| l.layer_name).collect())
        .unwrap_or_default();

    let avail = match entry.enumerate_extensions() {
        Ok(a) => a,
        Err(e) => {
            r.errors.push(format!("enumerate_extensions: {}", xe(e)));
            return r;
        }
    };
    let enabled = wanted(&avail);
    r.enabled_extensions = enabled
        .names()
        .iter()
        .map(|n| {
            String::from_utf8_lossy(n)
                .trim_end_matches('\0')
                .to_string()
        })
        .collect();

    // Ask for OpenXR 1.1 first, then fall back to 1.0 for older runtimes.
    let mut instance = None;
    for api in [xr::Version::new(1, 1, 0), xr::Version::new(1, 0, 0)] {
        let app = xr::ApplicationInfo {
            application_name: "frameplayer-probe",
            application_version: 1,
            engine_name: "frameplayer",
            engine_version: 1,
            api_version: api,
        };
        match entry.create_instance(&app, &enabled, &[], &()) {
            Ok(i) => {
                r.instance_api_version = Some(api.to_string());
                instance = Some(i);
                break;
            }
            Err(e) => r
                .errors
                .push(format!("xrCreateInstance (API {api}): {}", xe(e))),
        }
    }
    let Some(instance) = instance else { return r };
    if let Ok(p) = instance.properties() {
        r.runtime_name = Some(p.runtime_name.clone());
        r.runtime_version = Some(p.runtime_version.to_string());
    }

    let system = match instance.system(xr::FormFactor::HEAD_MOUNTED_DISPLAY) {
        Ok(s) => s,
        Err(e) => {
            r.errors.push(format!(
                "xrGetSystem(HMD): {} (is SteamVR running and the headset awake?)",
                xe(e)
            ));
            return r;
        }
    };
    r.system = Some(system_info(&instance, system, &enabled, &mut r.errors));
    r.interaction_profiles = probe_profiles(&instance, &enabled);

    if want_session {
        if enabled.khr_vulkan_enable2 {
            r.session = Some(probe_session(&instance, system, &enabled));
        } else {
            r.errors
                .push("session skipped: runtime lacks XR_KHR_vulkan_enable2".into());
        }
    }
    r
}

fn raw_extensions(entry: &xr::Entry) -> Result<Vec<(String, u32)>, xr::sys::Result> {
    let f = entry.fp().enumerate_instance_extension_properties;
    let mut count = 0u32;
    let r = unsafe { f(std::ptr::null(), 0, &mut count, std::ptr::null_mut()) };
    if r.into_raw() < 0 {
        return Err(r);
    }
    let mut props: Vec<xr::sys::ExtensionProperties> = (0..count)
        .map(|_| unsafe { xr::sys::ExtensionProperties::out(std::ptr::null_mut()).assume_init() })
        .collect();
    let r = unsafe { f(std::ptr::null(), count, &mut count, props.as_mut_ptr()) };
    if r.into_raw() < 0 {
        return Err(r);
    }
    props.truncate(count as usize);
    let mut out: Vec<(String, u32)> = props
        .iter()
        .map(|p| (fixed_cstr(&p.extension_name), p.extension_version))
        .collect();
    out.sort();
    Ok(out)
}

fn system_info(
    instance: &xr::Instance,
    system: xr::SystemId,
    enabled: &xr::ExtensionSet,
    errors: &mut Vec<String>,
) -> SystemInfo {
    let mut s = SystemInfo::default();
    match instance.system_properties(system) {
        Ok(p) => {
            s.name = p.system_name;
            s.vendor_id = p.vendor_id;
            s.max_swapchain_width = p.graphics_properties.max_swapchain_image_width;
            s.max_swapchain_height = p.graphics_properties.max_swapchain_image_height;
            s.max_layer_count = p.graphics_properties.max_layer_count;
            s.orientation_tracking = p.tracking_properties.orientation_tracking;
            s.position_tracking = p.tracking_properties.position_tracking;
        }
        Err(e) => errors.push(format!("xrGetSystemProperties: {}", xe(e))),
    }
    if let Ok(cfgs) = instance.enumerate_view_configurations(system) {
        s.view_configurations = cfgs.iter().map(|c| format!("{c:?}")).collect();
    }
    let stereo = xr::ViewConfigurationType::PRIMARY_STEREO;
    match instance.enumerate_view_configuration_views(system, stereo) {
        Ok(views) => {
            s.stereo_views = views
                .iter()
                .map(|v| ViewInfo {
                    recommended: (
                        v.recommended_image_rect_width,
                        v.recommended_image_rect_height,
                    ),
                    max: (v.max_image_rect_width, v.max_image_rect_height),
                    recommended_samples: v.recommended_swapchain_sample_count,
                    max_samples: v.max_swapchain_sample_count,
                })
                .collect()
        }
        Err(e) => errors.push(format!("xrEnumerateViewConfigurationViews: {}", xe(e))),
    }
    match instance.enumerate_environment_blend_modes(system, stereo) {
        Ok(m) => s.blend_modes = m.iter().map(|b| format!("{b:?}")).collect(),
        Err(e) => errors.push(format!("xrEnumerateEnvironmentBlendModes: {}", xe(e))),
    }
    if enabled.ext_hand_tracking {
        s.hand_tracking_supported = instance.supports_hand_tracking(system).ok();
    }
    if enabled.ext_eye_gaze_interaction {
        s.eye_gaze_supported = eye_gaze_supported(instance, system).ok();
    }
    if enabled.khr_vulkan_enable2
        && let Ok(req) = instance.graphics_requirements::<xr::Vulkan>(system)
    {
        s.vulkan_api_range = Some((
            req.min_api_version_supported.to_string(),
            req.max_api_version_supported.to_string(),
        ));
    }
    s
}

fn eye_gaze_supported(
    instance: &xr::Instance,
    system: xr::SystemId,
) -> Result<bool, xr::sys::Result> {
    unsafe {
        let mut eye = xr::sys::SystemEyeGazeInteractionPropertiesEXT {
            ty: xr::sys::SystemEyeGazeInteractionPropertiesEXT::TYPE,
            next: std::ptr::null_mut(),
            supports_eye_gaze_interaction: xr::sys::FALSE,
        };
        let mut p = xr::sys::SystemProperties {
            ty: xr::sys::SystemProperties::TYPE,
            next: &mut eye as *mut _ as *mut _,
            ..std::mem::zeroed()
        };
        let r = (instance.fp().get_system_properties)(instance.as_raw(), system, &mut p);
        if r.into_raw() < 0 {
            return Err(r);
        }
        Ok(eye.supports_eye_gaze_interaction != xr::sys::FALSE)
    }
}

/// The input kind an interaction path carries, which picks the action type.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum InputKind {
    Pose,
    Float,
    Bool,
    Vec2,
    Haptic,
}

pub fn input_kind(path: &str) -> InputKind {
    if path.ends_with("/pose") {
        InputKind::Pose
    } else if path.ends_with("/haptic") {
        InputKind::Haptic
    } else if path.ends_with("/value") || path.ends_with("/force") {
        InputKind::Float
    } else if path.ends_with("/thumbstick") || path.ends_with("/trackpad") {
        InputKind::Vec2
    } else {
        InputKind::Bool
    }
}

/// Defines the Frame controller profile. Not in the Khronos registry yet;
/// SteamVR on the Steam Frame provides it.
pub const FRAME_CONTROLLER_EXTENSION: &[u8] = b"XR_VALVE_frame_controller_interaction\0";

/// Every component of Valve's published Frame controller profile
/// (ValveSoftware/Unity, SteamFrameControllerProfile.cs): A/B/X/Y and menu on
/// the right, D-pad and view on the left, a shoulder button (the bumper),
/// grip, trigger and stick on both, touch on every button. Tried on both
/// hands, so a one-hand component also shows up rejected for the other.
const FRAME_COMPONENTS: &[&str] = &[
    "input/trigger/value",
    "input/trigger/click",
    "input/trigger/touch",
    "input/squeeze/value",
    "input/squeeze/click",
    "input/squeeze/touch",
    "input/shoulder/click",
    "input/shoulder/touch",
    "input/thumbstick",
    "input/thumbstick/click",
    "input/thumbstick/touch",
    "input/a/click",
    "input/a/touch",
    "input/b/click",
    "input/b/touch",
    "input/x/click",
    "input/x/touch",
    "input/y/click",
    "input/y/touch",
    "input/menu/click",
    "input/menu/touch",
    "input/view/click",
    "input/view/touch",
    "input/system/click",
    "input/system/touch",
    "input/dpad_up/click",
    "input/dpad_up/touch",
    "input/dpad_down/click",
    "input/dpad_down/touch",
    "input/dpad_left/click",
    "input/dpad_left/touch",
    "input/dpad_right/click",
    "input/dpad_right/touch",
    "input/grip/pose",
    "input/aim/pose",
    "output/haptic",
];

fn probe_profiles(instance: &xr::Instance, enabled: &xr::ExtensionSet) -> Vec<ProfileProbe> {
    let mut profiles: Vec<(&str, Vec<String>)> = Vec::new();
    let both = |comps: &[&str]| -> Vec<String> {
        ["left", "right"]
            .iter()
            .flat_map(|h| comps.iter().map(move |c| format!("/user/hand/{h}/{c}")))
            .collect()
    };
    profiles.push((
        "/interaction_profiles/valve/frame_controller_valve",
        both(FRAME_COMPONENTS),
    ));
    // What SteamVR presents the Frame controllers as without the extension.
    profiles.push((
        "/interaction_profiles/oculus/touch_controller",
        both(&[
            "input/trigger/value",
            "input/squeeze/value",
            "input/thumbstick",
            "input/grip/pose",
        ]),
    ));
    profiles.push((
        "/interaction_profiles/khr/simple_controller",
        both(&[
            "input/select/click",
            "input/menu/click",
            "input/aim/pose",
            "output/haptic",
        ]),
    ));
    if enabled.ext_eye_gaze_interaction {
        profiles.push((
            "/interaction_profiles/ext/eye_gaze_interaction",
            vec!["/user/eyes_ext/input/gaze_ext/pose".into()],
        ));
    }
    if enabled.ext_hand_interaction {
        profiles.push((
            "/interaction_profiles/ext/hand_interaction_ext",
            both(&[
                "input/pinch_ext/value",
                "input/aim_activate_ext/value",
                "input/grasp_ext/value",
                "input/pinch_ext/pose",
            ]),
        ));
    }

    let mut out = Vec::new();
    let Ok(set) = instance.create_action_set("probe", "Probe", 0) else {
        return out;
    };
    let mk = |n: &str| (n.to_string(), n.to_string());
    let (pn, pl) = mk("pose");
    let pose = set.create_action::<xr::Posef>(&pn, &pl, &[]).ok();
    let float = set.create_action::<f32>("float", "Float", &[]).ok();
    let boolean = set.create_action::<bool>("boolean", "Boolean", &[]).ok();
    let vec2 = set
        .create_action::<xr::Vector2f>("vec2", "Vector2", &[])
        .ok();
    let haptic = set
        .create_action::<xr::Haptic>("haptic", "Haptic", &[])
        .ok();

    for (profile, paths) in profiles {
        let mut probe = ProfileProbe {
            profile: profile.to_string(),
            accepted: vec![],
            rejected: vec![],
        };
        let profile_path = match instance.string_to_path(profile) {
            Ok(p) => p,
            Err(e) => {
                probe.rejected.push((profile.to_string(), xe(e)));
                out.push(probe);
                continue;
            }
        };
        for path in paths {
            let Ok(p) = instance.string_to_path(&path) else {
                probe.rejected.push((path, "invalid path".into()));
                continue;
            };
            let binding = match input_kind(&path) {
                InputKind::Pose => pose.as_ref().map(|a| xr::Binding::new(a, p)),
                InputKind::Float => float.as_ref().map(|a| xr::Binding::new(a, p)),
                InputKind::Bool => boolean.as_ref().map(|a| xr::Binding::new(a, p)),
                InputKind::Vec2 => vec2.as_ref().map(|a| xr::Binding::new(a, p)),
                InputKind::Haptic => haptic.as_ref().map(|a| xr::Binding::new(a, p)),
            };
            let Some(binding) = binding else { continue };
            match instance.suggest_interaction_profile_bindings(profile_path, &[binding]) {
                Ok(()) => probe.accepted.push(path),
                Err(e) => probe.rejected.push((path, xe(e))),
            }
        }
        out.push(probe);
    }
    out
}

fn probe_session(
    instance: &xr::Instance,
    system: xr::SystemId,
    enabled: &xr::ExtensionSet,
) -> SessionInfo {
    let mut s = SessionInfo::default();
    if let Err(e) = unsafe { session_inner(instance, system, enabled, &mut s) } {
        s.errors.push(e);
    }
    s
}

unsafe fn session_inner(
    instance: &xr::Instance,
    system: xr::SystemId,
    enabled: &xr::ExtensionSet,
    s: &mut SessionInfo,
) -> Result<(), String> {
    let req = instance
        .graphics_requirements::<xr::Vulkan>(system)
        .map_err(|e| format!("xrGetVulkanGraphicsRequirements2KHR: {}", xe(e)))?;
    let vk_entry =
        unsafe { ash::Entry::load() }.map_err(|e| format!("loading libvulkan.so.1: {e}"))?;
    let max = req.max_api_version_supported;
    let api = if max.major() > 1 || max.minor() >= 3 {
        vk::API_VERSION_1_3
    } else {
        vk::API_VERSION_1_1
    };

    let app_name = CString::new("frameplayer-probe").unwrap();
    let app = vk::ApplicationInfo::default()
        .application_name(&app_name)
        .api_version(api);
    let ici = vk::InstanceCreateInfo::default().application_info(&app);
    let gipa: xr::sys::platform::VkGetInstanceProcAddr =
        unsafe { std::mem::transmute(vk_entry.static_fn().get_instance_proc_addr) };
    let raw_instance =
        unsafe { instance.create_vulkan_instance(system, gipa, &ici as *const _ as *const _) }
            .map_err(|e| format!("xrCreateVulkanInstanceKHR: {}", xe(e)))?
            .map_err(|r| format!("vkCreateInstance via OpenXR: VkResult {r}"))?;
    let vk_instance = unsafe {
        ash::Instance::load(
            vk_entry.static_fn(),
            vk::Instance::from_raw(raw_instance as _),
        )
    };

    let result = (|| -> Result<(), String> {
        let raw_pd = unsafe { instance.vulkan_graphics_device(system, raw_instance) }
            .map_err(|e| format!("xrGetVulkanGraphicsDevice2KHR: {}", xe(e)))?;
        let pd = vk::PhysicalDevice::from_raw(raw_pd as _);
        let mut driver = vk::PhysicalDeviceDriverProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
        unsafe { vk_instance.get_physical_device_properties2(pd, &mut props2) };
        s.vulkan_device = props2
            .properties
            .device_name_as_c_str()
            .ok()
            .map(|c| c.to_string_lossy().into_owned());
        s.vulkan_driver = Some(format!(
            "{} ({})",
            driver
                .driver_name_as_c_str()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default(),
            driver
                .driver_info_as_c_str()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));

        let families = unsafe { vk_instance.get_physical_device_queue_family_properties(pd) };
        let qfi = families
            .iter()
            .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .ok_or("no graphics queue family")? as u32;
        let prio = [1.0f32];
        let qci = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qfi)
            .queue_priorities(&prio)];
        let dci = vk::DeviceCreateInfo::default().queue_create_infos(&qci);
        let raw_device = unsafe {
            instance.create_vulkan_device(system, gipa, raw_pd, &dci as *const _ as *const _)
        }
        .map_err(|e| format!("xrCreateVulkanDeviceKHR: {}", xe(e)))?
        .map_err(|r| format!("vkCreateDevice via OpenXR: VkResult {r}"))?;
        let device = unsafe {
            ash::Device::load(vk_instance.fp_v1_0(), vk::Device::from_raw(raw_device as _))
        };

        let session_result = (|| -> Result<(), String> {
            let info = xr::vulkan::SessionCreateInfo {
                instance: raw_instance,
                physical_device: raw_pd,
                device: raw_device,
                queue_family_index: qfi,
                queue_index: 0,
            };
            let (session, _waiter, _stream) =
                unsafe { instance.create_session::<xr::Vulkan>(system, &info) }
                    .map_err(|e| format!("xrCreateSession: {}", xe(e)))?;
            match session.enumerate_swapchain_formats() {
                Ok(f) => {
                    s.swapchain_formats = f
                        .iter()
                        .map(|&raw| format!("{:?}", vk::Format::from_raw(raw as i32)))
                        .collect()
                }
                Err(e) => s
                    .errors
                    .push(format!("xrEnumerateSwapchainFormats: {}", xe(e))),
            }
            match session.enumerate_reference_spaces() {
                Ok(sp) => s.reference_spaces = sp.iter().map(|x| format!("{x:?}")).collect(),
                Err(e) => s
                    .errors
                    .push(format!("xrEnumerateReferenceSpaces: {}", xe(e))),
            }
            if enabled.fb_display_refresh_rate {
                s.refresh_rates = session.enumerate_display_refresh_rates().ok();
                s.current_refresh_rate = session.get_display_refresh_rate().ok();
            }
            if enabled.fb_color_space {
                s.color_spaces = session
                    .enumerate_color_spaces()
                    .ok()
                    .map(|v| v.iter().map(|c| format!("{c:?}")).collect());
            }
            drop(session);
            Ok(())
        })();
        unsafe { device.destroy_device(None) };
        session_result
    })();
    unsafe { vk_instance.destroy_instance(None) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enables_the_frame_controller_extension_when_offered() {
        let mut avail = xr::ExtensionSet::default();
        assert!(wanted(&avail).other.is_empty());
        avail.other.push(FRAME_CONTROLLER_EXTENSION.to_vec());
        assert_eq!(
            wanted(&avail).other,
            vec![FRAME_CONTROLLER_EXTENSION.to_vec()]
        );
        assert!(FRAME_COMPONENTS.contains(&"input/shoulder/click"));
        assert!(!FRAME_COMPONENTS.iter().any(|c| c.contains("bumper")));
    }

    #[test]
    fn classifies_input_paths() {
        assert_eq!(
            input_kind("/user/hand/left/input/grip/pose"),
            InputKind::Pose
        );
        assert_eq!(
            input_kind("/user/hand/left/input/trigger/value"),
            InputKind::Float
        );
        assert_eq!(
            input_kind("/user/hand/left/input/trigger/click"),
            InputKind::Bool
        );
        assert_eq!(input_kind("/user/hand/left/input/a/touch"), InputKind::Bool);
        assert_eq!(
            input_kind("/user/hand/left/input/thumbstick"),
            InputKind::Vec2
        );
        assert_eq!(
            input_kind("/user/hand/right/output/haptic"),
            InputKind::Haptic
        );
        assert_eq!(
            input_kind("/user/eyes_ext/input/gaze_ext/pose"),
            InputKind::Pose
        );
    }
}
