//! Compiles `shaders/*.wgsl` to SPIR-V with naga (pure Rust, no
//! shaderc/glslang). `common.wgsl` is prepended to every other shader.

use std::path::Path;

fn compile(name: &str, common: &str, out_dir: &Path) {
    let path = format!("shaders/{name}.wgsl");
    println!("cargo:rerun-if-changed={path}");
    let body = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let src = format!("{common}\n{body}");
    let module = naga::front::wgsl::parse_str(&src)
        .unwrap_or_else(|e| panic!("{path}:\n{}", e.emit_to_string(&src)));
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::IMMEDIATES,
    )
    .validate(&module)
    .unwrap_or_else(|e| panic!("{path}: validation failed:\n{}", e.emit_to_string(&src)));
    let options = naga::back::spv::Options {
        // Vulkan 1.1+ consumes SPIR-V 1.3. Emit Vulkan clip space as-is (no y flip).
        lang_version: (1, 3),
        flags: naga::back::spv::WriterFlags::empty(),
        ..Default::default()
    };
    let words = naga::back::spv::write_vec(&module, &info, &options, None)
        .unwrap_or_else(|e| panic!("{path}: SPIR-V generation failed: {e}"));
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    std::fs::write(out_dir.join(format!("{name}.spv")), bytes).expect("write spv");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=shaders/common.wgsl");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let common = std::fs::read_to_string("shaders/common.wgsl").expect("shaders/common.wgsl");
    for name in ["yuv", "projection", "ui"] {
        compile(name, &common, Path::new(&out_dir));
    }
}
