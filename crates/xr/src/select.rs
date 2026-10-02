//! Pure selection policies: swapchain formats, environment blend mode,
//! refresh rate, and Vulkan device extensions.

use ash::vk;
use openxr as xr;
use std::ffi::CStr;

/// Colour precision wanted for the stereo swapchain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorFormatPreference {
    /// 8-bit sRGB: half the bandwidth, hardware gamma encode. Default on a
    /// mobile GPU at 72+ Hz.
    #[default]
    Srgb8,
    /// RGBA16F: no banding in dark gradients, 2× bandwidth.
    Float16,
}

/// Pick a colour format from the runtime's list (`xrEnumerateSwapchainFormats`,
/// already in runtime preference order).
// [verify] Which formats SteamVR on the Frame offers; the raylib quickstart
// only reports that sRGB RGBA8 works.
pub fn choose_color_format(offered: &[i64], pref: ColorFormatPreference) -> Option<vk::Format> {
    const SRGB: [vk::Format; 2] = [vk::Format::R8G8B8A8_SRGB, vk::Format::B8G8R8A8_SRGB];
    const F16: [vk::Format; 1] = [vk::Format::R16G16B16A16_SFLOAT];
    const UNORM: [vk::Format; 3] = [
        vk::Format::R8G8B8A8_UNORM,
        vk::Format::B8G8R8A8_UNORM,
        vk::Format::A2B10G10R10_UNORM_PACK32,
    ];
    let order: [&[vk::Format]; 3] = match pref {
        ColorFormatPreference::Srgb8 => [&SRGB, &F16, &UNORM],
        ColorFormatPreference::Float16 => [&F16, &SRGB, &UNORM],
    };
    order
        .iter()
        .flat_map(|group| group.iter())
        .find(|f| offered.contains(&(f.as_raw() as i64)))
        .copied()
        .or_else(|| offered.first().map(|&f| vk::Format::from_raw(f as i32)))
}

/// Format for UI quad/cylinder layers: always prefer 8-bit sRGB (text is
/// authored for it and the layer is small).
pub fn choose_ui_format(offered: &[i64]) -> Option<vk::Format> {
    choose_color_format(offered, ColorFormatPreference::Srgb8)
}

/// Prefer `ALPHA_BLEND` when passthrough is wanted and offered (lets the
/// system passthrough show through transparent pixels), else `OPAQUE`.
// [verify] Whether SteamVR on the Frame offers ALPHA_BLEND to native apps at
// all; the outline expects only OPAQUE (passthrough is system-driven).
pub fn choose_blend_mode(
    offered: &[xr::EnvironmentBlendMode],
    want_passthrough: bool,
) -> xr::EnvironmentBlendMode {
    if want_passthrough && offered.contains(&xr::EnvironmentBlendMode::ALPHA_BLEND) {
        return xr::EnvironmentBlendMode::ALPHA_BLEND;
    }
    if offered.contains(&xr::EnvironmentBlendMode::OPAQUE) {
        return xr::EnvironmentBlendMode::OPAQUE;
    }
    offered
        .first()
        .copied()
        .unwrap_or(xr::EnvironmentBlendMode::OPAQUE)
}

/// Pick the refresh rate to request: the smallest offered rate that is an
/// integer multiple of the video frame rate (judder-free) and at least
/// `min_hz`; otherwise the highest offered rate not above `max_hz`.
pub fn choose_refresh_rate(
    offered: &[f32],
    video_fps: Option<f32>,
    min_hz: f32,
    max_hz: f32,
) -> Option<f32> {
    let usable = || {
        offered
            .iter()
            .copied()
            .filter(move |&r| r >= min_hz - 0.5 && r <= max_hz + 0.5)
    };
    if let Some(fps) = video_fps.filter(|f| *f > 1.0) {
        let judder_free = usable()
            .filter(|&r| {
                let k = (r / fps).round();
                k >= 1.0 && (r - k * fps).abs() < 0.6
            })
            .min_by(|a, b| a.total_cmp(b));
        if judder_free.is_some() {
            return judder_free;
        }
    }
    usable()
        .max_by(|a, b| a.total_cmp(b))
        .or_else(|| offered.iter().copied().max_by(|a, b| a.total_cmp(b)))
}

/// Optional device extensions fp-gfx can use for zero-copy video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceExtensions {
    pub external_memory_fd: bool,
    pub external_memory_dma_buf: bool,
    pub image_drm_format_modifier: bool,
    pub queue_family_foreign: bool,
}

impl DeviceExtensions {
    /// All pieces needed for DMA-BUF import with explicit modifiers.
    pub fn zero_copy(&self) -> bool {
        self.external_memory_fd && self.external_memory_dma_buf && self.image_drm_format_modifier
    }
}

/// Choose which wish-list device extensions to enable given what the
/// physical device offers. Dependent extensions are only enabled together
/// with what they require.
pub fn select_device_extensions(available: &[&CStr]) -> (Vec<&'static CStr>, DeviceExtensions) {
    let has = |n: &CStr| available.contains(&n);
    let fd = has(ash::khr::external_memory_fd::NAME);
    let dma = fd && has(ash::ext::external_memory_dma_buf::NAME);
    let modifier = dma && has(ash::ext::image_drm_format_modifier::NAME);
    let foreign = dma && has(ash::ext::queue_family_foreign::NAME);
    let mut names = Vec::new();
    if fd {
        names.push(ash::khr::external_memory_fd::NAME);
    }
    if dma {
        names.push(ash::ext::external_memory_dma_buf::NAME);
    }
    if modifier {
        names.push(ash::ext::image_drm_format_modifier::NAME);
    }
    if foreign {
        names.push(ash::ext::queue_family_foreign::NAME);
    }
    let exts = DeviceExtensions {
        external_memory_fd: fd,
        external_memory_dma_buf: dma,
        image_drm_format_modifier: modifier,
        queue_family_foreign: foreign,
    };
    (names, exts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(fs: &[vk::Format]) -> Vec<i64> {
        fs.iter().map(|f| f.as_raw() as i64).collect()
    }

    #[test]
    fn color_formats() {
        let offered = raw(&[
            vk::Format::B8G8R8A8_UNORM,
            vk::Format::R16G16B16A16_SFLOAT,
            vk::Format::R8G8B8A8_SRGB,
        ]);
        assert_eq!(
            choose_color_format(&offered, ColorFormatPreference::Srgb8),
            Some(vk::Format::R8G8B8A8_SRGB)
        );
        assert_eq!(
            choose_color_format(&offered, ColorFormatPreference::Float16),
            Some(vk::Format::R16G16B16A16_SFLOAT)
        );
        let only_unorm = raw(&[vk::Format::B8G8R8A8_UNORM]);
        assert_eq!(
            choose_color_format(&only_unorm, ColorFormatPreference::Float16),
            Some(vk::Format::B8G8R8A8_UNORM)
        );
        let odd = raw(&[vk::Format::R5G6B5_UNORM_PACK16]);
        assert_eq!(
            choose_ui_format(&odd),
            Some(vk::Format::R5G6B5_UNORM_PACK16)
        );
        assert_eq!(choose_ui_format(&[]), None);
    }

    #[test]
    fn blend_modes() {
        use xr::EnvironmentBlendMode as B;
        assert_eq!(
            choose_blend_mode(&[B::OPAQUE, B::ALPHA_BLEND], true),
            B::ALPHA_BLEND
        );
        assert_eq!(
            choose_blend_mode(&[B::OPAQUE, B::ALPHA_BLEND], false),
            B::OPAQUE
        );
        assert_eq!(choose_blend_mode(&[B::OPAQUE], true), B::OPAQUE);
        assert_eq!(choose_blend_mode(&[B::ADDITIVE], false), B::ADDITIVE);
        assert_eq!(choose_blend_mode(&[], true), B::OPAQUE);
    }

    #[test]
    fn refresh_rates() {
        let offered = [72.0, 90.0, 120.0, 144.0];
        assert_eq!(
            choose_refresh_rate(&offered, Some(24.0), 72.0, 144.0),
            Some(72.0)
        );
        assert_eq!(
            choose_refresh_rate(&offered, Some(30.0), 72.0, 144.0),
            Some(90.0)
        );
        assert_eq!(
            choose_refresh_rate(&offered, Some(60.0), 72.0, 144.0),
            Some(120.0)
        );
        assert_eq!(
            choose_refresh_rate(&offered, Some(59.94), 72.0, 144.0),
            Some(120.0)
        );
        assert_eq!(
            choose_refresh_rate(&offered, Some(50.0), 72.0, 120.0),
            Some(120.0),
            "no multiple → highest allowed"
        );
        assert_eq!(choose_refresh_rate(&offered, None, 72.0, 90.0), Some(90.0));
        assert_eq!(
            choose_refresh_rate(&[72.0], Some(60.0), 90.0, 144.0),
            Some(72.0)
        );
        assert_eq!(choose_refresh_rate(&[], Some(60.0), 72.0, 144.0), None);
    }

    #[test]
    fn device_extension_dependencies() {
        let all = [
            ash::khr::external_memory_fd::NAME,
            ash::ext::external_memory_dma_buf::NAME,
            ash::ext::image_drm_format_modifier::NAME,
            ash::ext::queue_family_foreign::NAME,
            c"VK_KHR_swapchain",
        ];
        let (names, e) = select_device_extensions(&all);
        assert_eq!(names.len(), 4);
        assert!(e.zero_copy() && e.queue_family_foreign);
        // dma_buf without external_memory_fd is useless → nothing enabled.
        let (names, e) = select_device_extensions(&all[1..]);
        assert!(names.is_empty() && !e.zero_copy());
        let (names, e) = select_device_extensions(&all[..2]);
        assert_eq!(names.len(), 2);
        assert!(!e.zero_copy() && e.external_memory_dma_buf);
    }
}
