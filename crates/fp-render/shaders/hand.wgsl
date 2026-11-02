// A tracked hand as a glove of hard light round the real one, drawn over
// everything else in two pipelines over the same mesh:
//   1. fs_mask: depth, and the REVEAL. It wipes what's already drawn under the
//      hand (colour and alpha) by `reveal`, so with passthrough the menus open
//      where your hand is and show the real hand through them. Blend Zero,
//      OneMinusSrcAlpha: it only ever takes away.
//   2. fs_glow: added over what's there (colour only, alpha untouched): a
//      fresnel rim tracing the hand's silhouette, a faint body, and the
//      pointing fingertip lit (vertex colour r). It's depth-tested against
//      pass 1, so only the front-most surface glows: you never see a finger's
//      outline through the palm.
// Vertex colour a is the glove's presence: it fades out up the wrist.

struct PushConstants {
    view_proj: mat4x4<f32>,
    eye: vec4<f32>,
    // rgb: the rim's colour (linear); a: its strength.
    tint: vec4<f32>,
    // rgb: the pointing fingertip's colour; a: its glow.
    tip_tint: vec4<f32>,
    // x: the body, y: the reveal.
    extra: vec4<f32>,
};

var<immediate> pc: PushConstants;

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec4<f32>,
};

struct VsOut {
    // Both passes must land on the very same depth.
    @builtin(position) @invariant pos: vec4<f32>,
    @location(0) n: vec3<f32>,
    @location(1) v: vec3<f32>,
    @location(2) color: vec4<f32>,
};

@vertex
fn vs_main(i: VsIn) -> VsOut {
    var o: VsOut;
    o.pos = pc.view_proj * vec4<f32>(i.pos, 1.0);
    o.n = i.normal;
    o.v = pc.eye.xyz - i.pos;
    o.color = i.color;
    return o;
}

@fragment
fn fs_mask(i: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(0.0, 0.0, 0.0, clamp(pc.extra.y * i.color.a, 0.0, 1.0));
}

@fragment
fn fs_glow(i: VsOut) -> @location(0) vec4<f32> {
    let facing = abs(dot(normalize(i.n), normalize(i.v)));
    let rim = pow(clamp(1.0 - facing, 0.0, 1.0), 2.4);
    let c = pc.tint.rgb * (pc.extra.x + rim * pc.tint.a)
        + pc.tip_tint.rgb * (i.color.r * pc.tip_tint.a * (0.3 + rim));
    return vec4<f32>(c * i.color.a, 0.0);
}
