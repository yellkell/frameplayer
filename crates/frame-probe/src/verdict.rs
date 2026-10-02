//! Turns the raw probe data into answers to the platform questions marked
//! [verify] in docs/OUTLINE.md section 1.

use crate::Report;
use serde::Serialize;

#[derive(Serialize, Debug, PartialEq)]
pub enum Status {
    Yes,
    No,
    Partial,
    Unknown,
}

#[derive(Serialize, Debug)]
pub struct Verdict {
    pub question: &'static str,
    pub status: Status,
    pub answer: String,
}

fn v(question: &'static str, status: Status, answer: impl Into<String>) -> Verdict {
    Verdict {
        question,
        status,
        answer: answer.into(),
    }
}

pub fn evaluate(r: &Report) -> Vec<Verdict> {
    let xr = &r.xr;
    let xr_ext = |name: &str| xr.extensions.iter().any(|(n, _)| n == name);
    let xr_ext_like = |needle: &str| -> Vec<String> {
        xr.extensions
            .iter()
            .filter(|(n, _)| n.to_lowercase().contains(needle))
            .map(|(n, _)| n.clone())
            .collect()
    };
    let gpu = r.vulkan.devices.first();
    let gpu_ext = |name: &str| gpu.is_some_and(|d| d.notable_extensions.iter().any(|e| e == name));
    let mut out = Vec::new();

    // OpenXR runtime reachable at all.
    out.push(match (&xr.manifest, &xr.runtime_name, &xr.system) {
        (_, Some(name), Some(sys)) => v(
            "Native OpenXR runtime reachable",
            Status::Yes,
            format!(
                "{name} {} on \"{}\"",
                xr.runtime_version.clone().unwrap_or_default(),
                sys.name
            ),
        ),
        (Some(m), _, _) => v(
            "Native OpenXR runtime reachable",
            Status::Partial,
            format!(
                "manifest {} found, but: {}",
                m.manifest_path,
                xr.errors.join("; ")
            ),
        ),
        _ => v(
            "Native OpenXR runtime reachable",
            Status::No,
            xr.errors.join("; "),
        ),
    });

    // Refresh rates offered to native apps.
    let session = xr.session.as_ref();
    out.push(match session.and_then(|s| s.refresh_rates.as_ref()) {
        Some(rates) if !rates.is_empty() => v(
            "Display refresh rates a native app can request",
            if rates.len() > 1 {
                Status::Yes
            } else {
                Status::Partial
            },
            format!(
                "{:?} Hz, currently {} Hz",
                rates,
                session
                    .and_then(|s| s.current_refresh_rate)
                    .map(|c| c.to_string())
                    .unwrap_or("?".into())
            ),
        ),
        _ if xr.runtime_name.is_some() && !xr_ext("XR_FB_display_refresh_rate") => v(
            "Display refresh rates a native app can request",
            Status::No,
            "runtime lacks XR_FB_display_refresh_rate: the app gets whatever SteamVR sets",
        ),
        _ => v(
            "Display refresh rates a native app can request",
            Status::Unknown,
            "no session data",
        ),
    });

    // Per-eye render size.
    out.push(
        match xr.system.as_ref().and_then(|s| s.stereo_views.first()) {
            Some(view) => v(
                "Recommended per-eye render size",
                Status::Yes,
                format!(
                    "{}x{} recommended, {}x{} max",
                    view.recommended.0, view.recommended.1, view.max.0, view.max.1
                ),
            ),
            None => v(
                "Recommended per-eye render size",
                Status::Unknown,
                "no system data",
            ),
        },
    );

    // Hardware decode through V4L2.
    let decoders: Vec<&crate::v4l2::V4l2Device> = r
        .v4l2
        .devices
        .iter()
        .filter(|d| d.kind == "decoder")
        .collect();
    let blocked: Vec<String> = r
        .v4l2
        .devices
        .iter()
        .filter_map(|d| {
            d.open_error.as_ref().map(|e| {
                format!(
                    "{} ({}, group {}): {e}",
                    d.node,
                    d.mode,
                    d.owner_group.clone().unwrap_or("?".into())
                )
            })
        })
        .collect();
    out.push(if !decoders.is_empty() {
        let desc: Vec<String> = decoders
            .iter()
            .map(|d| {
                let codecs: Vec<String> = d
                    .compressed_in
                    .iter()
                    .filter(|f| f.compressed)
                    .map(|f| match f.max_size {
                        Some((w, h)) => format!("{} up to {w}x{h}", f.fourcc),
                        None => f.fourcc.clone(),
                    })
                    .collect();
                let outs: Vec<&str> = d.raw_out.iter().map(|f| f.fourcc.as_str()).collect();
                format!(
                    "{} [{}]: {} -> {}",
                    d.node,
                    d.driver,
                    codecs.join(", "),
                    outs.join("/")
                )
            })
            .collect();
        v(
            "V4L2 hardware decoder usable without root",
            Status::Yes,
            desc.join("; "),
        )
    } else if !blocked.is_empty() {
        v(
            "V4L2 hardware decoder usable without root",
            Status::Partial,
            format!(
                "video nodes exist but cannot be opened: {}",
                blocked.join("; ")
            ),
        )
    } else {
        v(
            "V4L2 hardware decoder usable without root",
            Status::No,
            "no V4L2 decoder nodes found",
        )
    });

    // Vulkan Video.
    let vv: Vec<String> = gpu
        .map(|d| {
            d.video_decode
                .iter()
                .filter(|c| c.supported)
                .map(|c| match c.max_coded_extent {
                    Some((w, h)) => format!("{} up to {w}x{h}", c.profile),
                    None => c.profile.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    out.push(if !vv.is_empty() {
        v(
            "Vulkan Video decode on the GPU driver",
            Status::Yes,
            vv.join("; "),
        )
    } else if gpu_ext("VK_KHR_video_decode_queue") {
        v(
            "Vulkan Video decode on the GPU driver",
            Status::Partial,
            "decode queue present but no profile reported support",
        )
    } else if gpu.is_some() {
        v(
            "Vulkan Video decode on the GPU driver",
            Status::No,
            "driver exposes no VK_KHR_video_decode_queue",
        )
    } else {
        v(
            "Vulkan Video decode on the GPU driver",
            Status::Unknown,
            r.vulkan.errors.join("; "),
        )
    });

    // Zero-copy DMA-BUF import of decoder output.
    let nv12_sampled = gpu.is_some_and(|d| {
        d.format_modifiers
            .iter()
            .any(|f| f.format.contains("G8_B8R8_2PLANE_420") && f.modifiers.iter().any(|m| m.2))
    });
    let dmabuf =
        gpu_ext("VK_EXT_external_memory_dma_buf") && gpu_ext("VK_EXT_image_drm_format_modifier");
    out.push(match (gpu.is_some(), dmabuf, nv12_sampled) {
        (true, true, true) => v("Zero-copy DMA-BUF import of NV12 into Vulkan", Status::Yes, "dma_buf + drm_format_modifier, NV12 sampleable with modifiers"),
        (true, true, false) => v("Zero-copy DMA-BUF import of NV12 into Vulkan", Status::Partial, "DMA-BUF import available, but no sampleable NV12 modifier: convert on GPU from a different layout"),
        (true, false, _) => v("Zero-copy DMA-BUF import of NV12 into Vulkan", Status::No, "VK_EXT_external_memory_dma_buf or VK_EXT_image_drm_format_modifier missing"),
        _ => v("Zero-copy DMA-BUF import of NV12 into Vulkan", Status::Unknown, "no Vulkan device"),
    });

    // Passthrough.
    let pt = xr_ext_like("passthrough");
    let blend = xr
        .system
        .as_ref()
        .map(|s| s.blend_modes.clone())
        .unwrap_or_default();
    let blend_mr = blend
        .iter()
        .any(|b| b.contains("ALPHA_BLEND") || b.contains("ADDITIVE"));
    out.push(match (!pt.is_empty(), blend_mr, xr.system.is_some()) {
        (true, _, _) => v(
            "Passthrough controllable from OpenXR",
            Status::Yes,
            format!(
                "extensions: {}; blend modes: {}",
                pt.join(", "),
                blend.join(", ")
            ),
        ),
        (false, true, _) => v(
            "Passthrough controllable from OpenXR",
            Status::Partial,
            format!(
                "no passthrough extension, but blend modes {}",
                blend.join(", ")
            ),
        ),
        (false, false, true) => v(
            "Passthrough controllable from OpenXR",
            Status::No,
            format!(
                "blend modes: {}; use the system passthrough toggle",
                blend.join(", ")
            ),
        ),
        _ => v(
            "Passthrough controllable from OpenXR",
            Status::Unknown,
            "no system data",
        ),
    });

    // Eye tracking.
    out.push(match xr.system.as_ref().and_then(|s| s.eye_gaze_supported) {
        Some(true) => v("Eye gaze via XR_EXT_eye_gaze_interaction", Status::Yes, "supported by the system"),
        Some(false) => v("Eye gaze via XR_EXT_eye_gaze_interaction", Status::Partial, "extension present, system reports unsupported (check SteamVR eye-tracking setting)"),
        None if xr.runtime_name.is_some() && !xr_ext("XR_EXT_eye_gaze_interaction") => v("Eye gaze via XR_EXT_eye_gaze_interaction", Status::No, "extension not offered"),
        None => v("Eye gaze via XR_EXT_eye_gaze_interaction", Status::Unknown, "no system data"),
    });

    // Hand tracking.
    out.push(
        match xr.system.as_ref().and_then(|s| s.hand_tracking_supported) {
            Some(true) => v(
                "Hand tracking via XR_EXT_hand_tracking",
                Status::Yes,
                "supported",
            ),
            Some(false) => v(
                "Hand tracking via XR_EXT_hand_tracking",
                Status::Partial,
                "extension present, system reports unsupported",
            ),
            None if xr.runtime_name.is_some() => v(
                "Hand tracking via XR_EXT_hand_tracking",
                Status::No,
                "extension not offered",
            ),
            None => v(
                "Hand tracking via XR_EXT_hand_tracking",
                Status::Unknown,
                "no system data",
            ),
        },
    );

    // Frame controller profile.
    let frame = xr
        .interaction_profiles
        .iter()
        .find(|p| p.profile.contains("frame_controller_valve"));
    out.push(match frame {
        Some(p) if !p.accepted.is_empty() => v(
            "Frame controller interaction profile",
            Status::Yes,
            format!(
                "{} components bound, {} rejected",
                p.accepted.len(),
                p.rejected.len()
            ),
        ),
        Some(p) => v(
            "Frame controller interaction profile",
            Status::No,
            p.rejected
                .first()
                .map(|(path, e)| format!("{path}: {e}"))
                .unwrap_or_default(),
        ),
        None => v(
            "Frame controller interaction profile",
            Status::Unknown,
            "not probed",
        ),
    });

    // Foveation.
    let mut fov = xr_ext_like("foveat");
    fov.extend(
        [
            "VK_EXT_fragment_density_map",
            "VK_EXT_fragment_density_map2",
            "VK_QCOM_fragment_density_map_offset",
            "VK_KHR_fragment_shading_rate",
        ]
        .iter()
        .filter(|e| gpu_ext(e))
        .map(|e| e.to_string()),
    );
    out.push(
        if fov.is_empty() && xr.extensions.is_empty() && gpu.is_none() {
            v(
                "Foveated rendering hooks",
                Status::Unknown,
                "no runtime or GPU data",
            )
        } else if fov.is_empty() {
            v("Foveated rendering hooks", Status::No, "none found")
        } else {
            v("Foveated rendering hooks", Status::Yes, fov.join(", "))
        },
    );

    // Runtime-composited 360/180 and curved UI layers.
    let layers: Vec<&str> = [
        "XR_KHR_composition_layer_equirect2",
        "XR_KHR_composition_layer_equirect",
        "XR_KHR_composition_layer_cylinder",
    ]
    .into_iter()
    .filter(|e| xr_ext(e))
    .collect();
    out.push(if layers.is_empty() && xr.extensions.is_empty() {
        v(
            "Runtime equirect/cylinder layers",
            Status::Unknown,
            "no runtime data",
        )
    } else if layers.is_empty() {
        v(
            "Runtime equirect/cylinder layers",
            Status::No,
            "none: project 180/360 in our own shaders",
        )
    } else {
        v(
            "Runtime equirect/cylinder layers",
            Status::Yes,
            layers.join(", "),
        )
    });

    // Swapchain formats.
    out.push(match session {
        Some(s) if !s.swapchain_formats.is_empty() => {
            let srgb = s.swapchain_formats.iter().any(|f| f.contains("SRGB"));
            let deep = s
                .swapchain_formats
                .iter()
                .any(|f| f.contains("A2B10G10R10") || f.contains("SFLOAT"));
            v(
                "Swapchain formats (sRGB / 10-bit or float)",
                if srgb && deep {
                    Status::Yes
                } else {
                    Status::Partial
                },
                s.swapchain_formats.join(", "),
            )
        }
        _ => v(
            "Swapchain formats (sRGB / 10-bit or float)",
            Status::Unknown,
            "no session data",
        ),
    });

    // C runtime and loader baseline.
    let vk_lib = r.libs.iter().find(|l| l.soname == "libvulkan.so.1");
    out.push(v(
        "C runtime baseline and system Vulkan loader",
        if vk_lib.is_some_and(|l| l.found) {
            Status::Yes
        } else {
            Status::No
        },
        format!(
            "glibc {}; libvulkan.so.1 {}; {}",
            r.system.glibc.clone().unwrap_or("?".into()),
            vk_lib
                .and_then(|l| l.path.clone())
                .unwrap_or("not found".into()),
            if r.system.container_hints.is_empty() {
                "running on the host".to_string()
            } else {
                r.system.container_hints.join(", ")
            }
        ),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Report, system, v4l2, vk, xr};

    fn empty() -> Report {
        Report {
            probe_version: "test",
            generated_unix: 0,
            verdicts: Vec::new(),
            system: system::SystemReport::default(),
            libs: Vec::new(),
            drm: Vec::new(),
            v4l2: v4l2::V4l2Report {
                devices: Vec::new(),
                media_nodes: Vec::new(),
            },
            vulkan: vk::VkReport::default(),
            xr: xr::XrReport::default(),
        }
    }

    fn find<'a>(vs: &'a [Verdict], q: &str) -> &'a Verdict {
        vs.iter()
            .find(|v| v.question.starts_with(q))
            .expect("verdict present")
    }

    #[test]
    fn nothing_known_is_unknown_not_no() {
        let vs = evaluate(&empty());
        for q in [
            "Runtime equirect",
            "Foveated",
            "Display refresh",
            "Swapchain",
        ] {
            assert_eq!(find(&vs, q).status, Status::Unknown, "{q}");
        }
    }

    #[test]
    fn decoder_found_is_yes_and_blocked_node_is_partial() {
        let mut r = empty();
        r.v4l2.devices.push(v4l2::V4l2Device {
            node: "/dev/video0".into(),
            kind: "decoder".into(),
            driver: "iris".into(),
            compressed_in: vec![v4l2::Format {
                fourcc: "HEVC".into(),
                description: String::new(),
                compressed: true,
                max_size: Some((8192, 4352)),
            }],
            raw_out: vec![v4l2::Format {
                fourcc: "NV12".into(),
                description: String::new(),
                compressed: false,
                max_size: None,
            }],
            ..Default::default()
        });
        let v = evaluate(&r);
        let d = find(&v, "V4L2");
        assert_eq!(d.status, Status::Yes);
        assert!(d.answer.contains("HEVC up to 8192x4352"), "{}", d.answer);

        let mut r = empty();
        r.v4l2.devices.push(v4l2::V4l2Device {
            node: "/dev/video0".into(),
            mode: "660".into(),
            owner_group: Some("video".into()),
            open_error: Some("Permission denied".into()),
            ..Default::default()
        });
        assert_eq!(find(&evaluate(&r), "V4L2").status, Status::Partial);
    }

    #[test]
    fn frame_profile_and_refresh_rates() {
        let mut r = empty();
        r.xr.runtime_name = Some("SteamVR/OpenXR".into());
        r.xr.extensions = vec![("XR_FB_display_refresh_rate".into(), 1)];
        r.xr.interaction_profiles.push(xr::ProfileProbe {
            profile: "/interaction_profiles/valve/frame_controller_valve".into(),
            accepted: vec!["/user/hand/left/input/trigger/value".into()],
            rejected: vec![],
        });
        r.xr.session = Some(xr::SessionInfo {
            refresh_rates: Some(vec![72.0, 90.0, 120.0, 144.0]),
            current_refresh_rate: Some(90.0),
            ..Default::default()
        });
        let v = evaluate(&r);
        assert_eq!(find(&v, "Frame controller").status, Status::Yes);
        let rr = find(&v, "Display refresh");
        assert_eq!(rr.status, Status::Yes);
        assert!(rr.answer.contains("currently 90"), "{}", rr.answer);
    }
}
