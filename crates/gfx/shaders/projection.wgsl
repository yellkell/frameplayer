// Per-eye projection pass: projection mesh -> XR swapchain image.
// CPU reference: src/correction.rs (EyePush::sample_uv). Keep in sync.

struct EyePush {
    mvp: mat4x4<f32>,
    uv_rect: vec4<f32>,
    // (u_min, v_min, u_max, v_max) of the visible eye image
    crop: vec4<f32>,
    // (zoom, k1, k2, flags-as-bits)
    lens: vec4<f32>,
    tint: vec4<f32>,
}

var<immediate> pc: EyePush;

@group(0) @binding(0) var video: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@location(0) pos: vec3<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var out: VsOut;
    out.pos = pc.mvp * vec4<f32>(pos, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let zoom = pc.lens.x;
    let p = (in.uv - vec2<f32>(0.5)) / zoom;
    let r2 = 4.0 * dot(p, p);
    let d = vec2<f32>(0.5) + p * (1.0 + pc.lens.y * r2 + pc.lens.z * r2 * r2);
    let inside = all(d >= pc.crop.xy) && all(d <= pc.crop.zw);
    let fuv = mix(pc.uv_rect.xy, pc.uv_rect.zw, clamp(d, vec2<f32>(0.0), vec2<f32>(1.0)));
    // Sample unconditionally (derivatives need uniform control flow), mask after.
    let texel = textureSample(video, samp, fuv);
    var c = vec4<f32>(texel.rgb, 1.0) * pc.tint;
    c = select(vec4<f32>(0.0), c, inside);
    if ((bitcast<u32>(pc.lens.w) & 1u) != 0u) {
        c = vec4<f32>(srgb_oetf3(c.rgb), c.a);
    }
    return c;
}
