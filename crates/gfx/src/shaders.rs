//! SPIR-V produced by `build.rs` from `shaders/*.wgsl`.

/// Entry point names (shared by all modules that have them).
pub const ENTRY_COMPUTE: &std::ffi::CStr = c"main";
pub const ENTRY_VERTEX: &std::ffi::CStr = c"vs_main";
pub const ENTRY_FRAGMENT: &std::ffi::CStr = c"fs_main";

/// SPIR-V bytes for the YUV → RGB compute shader.
pub const YUV_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/yuv.spv"));
/// SPIR-V bytes for the projection pass (vertex + fragment).
pub const PROJECTION_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/projection.spv"));
/// SPIR-V bytes for the UI pass (vertex + fragment).
pub const UI_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ui.spv"));

/// Reassemble little-endian SPIR-V bytes into words (include_bytes gives no
/// alignment guarantee, so copy instead of casting).
pub fn words(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorPush;
    use crate::correction::EyePush;
    use std::mem::size_of;

    const SPIRV_MAGIC: u32 = 0x0723_0203;

    #[test]
    fn spirv_modules_are_valid_headers() {
        for spv in [YUV_SPV, PROJECTION_SPV, UI_SPV] {
            assert_eq!(spv.len() % 4, 0);
            let w = words(spv);
            assert_eq!(w[0], SPIRV_MAGIC);
            assert!(w.len() > 50);
        }
    }

    /// Byte size naga computes for a named struct in a WGSL file.
    fn wgsl_struct_span(file: &str, name: &str) -> u32 {
        let common =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/shaders/common.wgsl"))
                .unwrap();
        let body =
            std::fs::read_to_string(format!("{}/shaders/{file}", env!("CARGO_MANIFEST_DIR")))
                .unwrap();
        let module = naga::front::wgsl::parse_str(&format!("{common}\n{body}")).unwrap();
        let span = module
            .types
            .iter()
            .find_map(|(_, ty)| match (&ty.name, &ty.inner) {
                (Some(n), naga::TypeInner::Struct { span, .. }) if n == name => Some(*span),
                _ => None,
            })
            .unwrap_or_else(|| panic!("struct {name} not in {file}"));
        span
    }

    #[test]
    fn push_constant_layouts_match_rust() {
        assert_eq!(
            wgsl_struct_span("yuv.wgsl", "ColorPush") as usize,
            size_of::<ColorPush>()
        );
        assert_eq!(
            wgsl_struct_span("projection.wgsl", "EyePush") as usize,
            size_of::<EyePush>()
        );
        assert_eq!(
            wgsl_struct_span("ui.wgsl", "UiPush") as usize,
            size_of::<crate::vk::UiPush>()
        );
    }
}
