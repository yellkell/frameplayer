// Textured, tinted quads in world space: UI panels, the pointer ray, cursors.

struct PushConstants {
    view_proj: mat4x4<f32>,
};

@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var tex: texture_2d<f32>;
var<immediate> pc: PushConstants;

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_main(v: VsIn) -> VsOut {
    var o: VsOut;
    o.pos = pc.view_proj * vec4<f32>(v.pos, 1.0);
    o.uv = v.uv;
    o.color = v.color;
    return o;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Texture is sampled through an sRGB view (linear result) and holds
    // premultiplied alpha; colour is linear and premultiplied too.
    return textureSample(tex, samp, in.uv) * in.color;
}
