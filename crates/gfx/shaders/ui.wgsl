// UI draw-list pass: fp_core::draw::DrawList -> UI layer swapchain image.
// Output is premultiplied alpha.

struct UiPush {
    // 2 / panel size in pixels
    scale: vec2<f32>,
    // 0 = solid, 1 = R8 coverage (font atlas), 2 = straight-alpha RGBA image, 3 = opaque RGB (video)
    mode: u32,
    // bit 0: encode sRGB in the shader (UNORM target)
    flags: u32,
}

var<immediate> pc: UiPush;

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
}

@vertex
fn vs_main(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>, @location(2) color: vec4<f32>) -> VsOut {
    var out: VsOut;
    out.pos = vec4<f32>(pos * pc.scale - vec2<f32>(1.0), 0.0, 1.0);
    out.uv = uv;
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let t = textureSample(tex, samp, in.uv);
    var c = in.color;
    switch pc.mode {
        case 1u: { c = in.color * t.r; }
        case 2u: { c = in.color * vec4<f32>(t.rgb * t.a, t.a); }
        case 3u: { c = in.color * vec4<f32>(t.rgb, 1.0); }
        default: {}
    }
    if ((pc.flags & 1u) != 0u && c.a > 0.0) {
        c = vec4<f32>(srgb_oetf3(c.rgb / c.a) * c.a, c.a);
    }
    return c;
}
