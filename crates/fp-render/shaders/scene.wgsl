// Per-pixel VR video projection.
//
// Every fragment builds its view ray from the eye's inverse view-projection,
// maps the ray to a video coordinate for the active projection, picks the
// eye's half of a stereo frame, converts YUV to RGB, applies HDR tone mapping
// and picture adjustments, and writes linear colour to an sRGB target.

struct Params {
    inv_view_proj: array<mat4x4<f32>, 2>,
    // Rotation applied to world rays before projection (inverse of the
    // video's yaw/pitch/roll correction).
    correction: mat4x4<f32>,
    // World -> screen-local transform for flat screens.
    screen_inv: mat4x4<f32>,
    // x: projection kind (0 flat, 1 equirect, 2 fisheye, 3 eac)
    // y: stereo (0 mono, 1 sbs, 2 tb), z: swap eyes, w: plane layout (0 I420, 1 NV12/P010)
    mode: vec4<u32>,
    // x: h_fov, y: v_fov (radians), z: zoom, w: ipd offset (radians)
    proj: vec4<f32>,
    // x: lens k1, y: lens k2, z: vertical align, w: rotation align (radians)
    lens: vec4<f32>,
    // x: screen width, y: screen height, z: curvature arc (radians), w: has video
    screen: vec4<f32>,
    // YUV -> RGB: rgb = m * (yuv * scale - offset)
    yuv_r: vec4<f32>,
    yuv_g: vec4<f32>,
    yuv_b: vec4<f32>,
    // xyz: offsets, w: sample scale (10-bit-in-16 = 65535/1023)
    yuv_offset: vec4<f32>,
    // x brightness, y contrast, z saturation, w gamma
    picture: vec4<f32>,
    // x sharpen, y transfer (0 sdr, 1 pq, 2 hlg), z wide gamut, w background alpha
    extra: vec4<f32>,
    // x width, y height, z 1/width, w 1/height (luma texels)
    tex_size: vec4<f32>,
    bg_color: vec4<f32>,
};

struct PushConstants {
    eye: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var plane0: texture_2d<f32>;
@group(0) @binding(3) var plane1: texture_2d<f32>;
@group(0) @binding(4) var plane2: texture_2d<f32>;
var<immediate> pc: PushConstants;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) ndc: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    // One triangle covering the screen.
    let xy = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u)) * 2.0 - 1.0;
    var o: VsOut;
    o.pos = vec4<f32>(xy, 0.0, 1.0);
    o.ndc = xy;
    return o;
}

const PI: f32 = 3.14159265358979;
const INVALID: vec2<f32> = vec2<f32>(-1.0, -1.0);

fn rot_y(a: f32) -> mat3x3<f32> {
    let c = cos(a); let s = sin(a);
    return mat3x3<f32>(vec3<f32>(c, 0.0, -s), vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(s, 0.0, c));
}
fn rot_x(a: f32) -> mat3x3<f32> {
    let c = cos(a); let s = sin(a);
    return mat3x3<f32>(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, c, s), vec3<f32>(0.0, -s, c));
}
fn rot_z(a: f32) -> mat3x3<f32> {
    let c = cos(a); let s = sin(a);
    return mat3x3<f32>(vec3<f32>(c, s, 0.0), vec3<f32>(-s, c, 0.0), vec3<f32>(0.0, 0.0, 1.0));
}

// Angle-preserving zoom: scales the angle from the forward axis.
fn zoom_dir(d: vec3<f32>, zoom: f32) -> vec3<f32> {
    if (abs(zoom - 1.0) < 1e-4) { return d; }
    let theta = acos(clamp(-d.z, -1.0, 1.0));
    let r = length(d.xy);
    if (r < 1e-6) { return d; }
    let t2 = min(theta / zoom, PI);
    let phi = d.xy / r;
    return vec3<f32>(phi * sin(t2), -cos(t2));
}

// Ray (forward = -Z, right = +X, up = +Y) to normalised video coordinates
// (0..1, origin top-left) for spherical projections.
fn equirect_uv(d: vec3<f32>, h_fov: f32, v_fov: f32) -> vec2<f32> {
    let lon = atan2(d.x, -d.z);
    let lat = asin(clamp(d.y, -1.0, 1.0));
    var u = 0.5 + lon / h_fov;
    let v = 0.5 - lat / v_fov;
    if (h_fov >= 2.0 * PI - 1e-3) {
        u = fract(u);
    } else if (u < 0.0 || u > 1.0) {
        return INVALID;
    }
    if (v < 0.0 || v > 1.0) { return INVALID; }
    return vec2<f32>(u, v);
}

fn fisheye_uv(d: vec3<f32>, fov: f32, k1: f32, k2: f32) -> vec2<f32> {
    let theta = acos(clamp(-d.z, -1.0, 1.0));
    var r = theta / fov; // 0.5 at the edge of the image circle
    if (r > 0.5) { return INVALID; }
    let r2 = r * r * 4.0;
    r = r * (1.0 + k1 * r2 + k2 * r2 * r2);
    let len = length(d.xy);
    var dir = vec2<f32>(0.0, 0.0);
    if (len > 1e-6) { dir = d.xy / len; }
    let uv = vec2<f32>(0.5 + r * dir.x, 0.5 - r * dir.y);
    if (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))) { return INVALID; }
    return uv;
}

// YouTube equi-angular cubemap, 3x2 layout as FFmpeg's v360 "eac" output:
// top row left|front|right, bottom row down|back|up, bottom faces rotated.
fn eac_face_uv(a: f32, b: f32) -> vec2<f32> {
    // Equi-angular warp of cube-face coordinates in [-1, 1].
    return vec2<f32>(atan(a) * 4.0 / PI, atan(b) * 4.0 / PI) * 0.5 + 0.5;
}

fn eac_uv(d: vec3<f32>, h_fov: f32) -> vec2<f32> {
    let ax = abs(d.x); let ay = abs(d.y); let az = abs(d.z);
    var face: i32;
    var a: f32; var b: f32; // face-local: a right, b down
    if (az >= ax && az >= ay) {
        if (d.z < 0.0) { face = 1; a = d.x / az; b = -d.y / az; }      // front
        else { face = 4; a = -d.x / az; b = -d.y / az; }               // back
    } else if (ax >= ay) {
        if (d.x > 0.0) { face = 2; a = d.z / ax; b = -d.y / ax; }      // right
        else { face = 0; a = -d.z / ax; b = -d.y / ax; }               // left
    } else {
        if (d.y > 0.0) { face = 5; a = d.x / ay; b = d.z / ay; }       // up
        else { face = 3; a = d.x / ay; b = -d.z / ay; }                // down
    }
    var f = eac_face_uv(a, b);
    var col: f32; var row: f32;
    switch face {
        case 0: { col = 0.0; row = 0.0; }
        case 1: { col = 1.0; row = 0.0; }
        case 2: { col = 2.0; row = 0.0; }
        // Orientations verified against FFmpeg's v360 EAC output
        // (tests/projections.rs): down and up are anti-transposed, back is
        // rotated 90 degrees.
        case 3: { col = 0.0; row = 1.0; f = vec2<f32>(1.0 - f.y, 1.0 - f.x); }
        case 4: { col = 1.0; row = 1.0; f = vec2<f32>(1.0 - f.y, f.x); }
        default: { col = 2.0; row = 1.0; f = vec2<f32>(1.0 - f.y, 1.0 - f.x); }
    }
    let uv = vec2<f32>((col + f.x) / 3.0, (row + f.y) / 2.0);
    if (h_fov < 2.0 * PI - 1e-3) {
        // EAC 180: only the front half of the layout is stored (columns
        // mapped onto the full width).
        if (d.z > 0.0) { return INVALID; }
    }
    return uv;
}

fn flat_uv(origin: vec3<f32>, d: vec3<f32>) -> vec2<f32> {
    let o = (params.screen_inv * vec4<f32>(origin, 1.0)).xyz;
    let dir = normalize((params.screen_inv * vec4<f32>(d, 0.0)).xyz);
    let w = params.screen.x; let h = params.screen.y; let arc = params.screen.z;
    var p: vec3<f32>;
    var u: f32;
    if (arc < 1e-3) {
        if (dir.z >= -1e-6) { return INVALID; }
        let t = -o.z / dir.z;
        if (t <= 0.0) { return INVALID; }
        p = o + t * dir;
        u = p.x / w + 0.5;
    } else {
        // Concave cylinder of radius R centred at (0, 0, R), screen centre at origin.
        let r = w / arc;
        let oc = vec2<f32>(o.x, o.z - r);
        let dd = vec2<f32>(dir.x, dir.z);
        let qa = dot(dd, dd);
        let qb = 2.0 * dot(oc, dd);
        let qc = dot(oc, oc) - r * r;
        let disc = qb * qb - 4.0 * qa * qc;
        if (disc < 0.0) { return INVALID; }
        let t = (-qb + sqrt(disc)) / (2.0 * qa);
        if (t <= 0.0) { return INVALID; }
        p = o + t * dir;
        let phi = atan2(p.x, r - p.z);
        u = 0.5 + phi * r / w;
    }
    let v = 0.5 - p.y / h;
    if (u < 0.0 || u > 1.0 || v < 0.0 || v > 1.0) { return INVALID; }
    return vec2<f32>(u, v);
}

fn to_eye_rect(uv: vec2<f32>, eye: u32) -> vec2<f32> {
    let slot = f32(eye ^ params.mode.z);
    switch params.mode.y {
        case 1u: { return vec2<f32>(uv.x * 0.5 + slot * 0.5, uv.y); }
        case 2u: { return vec2<f32>(uv.x, uv.y * 0.5 + slot * 0.5); }
        default: { return uv; }
    }
}

fn sample_yuv(uv: vec2<f32>) -> vec3<f32> {
    let y = textureSampleLevel(plane0, samp, uv, 0.0).r;
    var c: vec2<f32>;
    if (params.mode.w == 0u) {
        c = vec2<f32>(textureSampleLevel(plane1, samp, uv, 0.0).r, textureSampleLevel(plane2, samp, uv, 0.0).r);
    } else {
        c = textureSampleLevel(plane1, samp, uv, 0.0).rg;
    }
    let yuv = vec3<f32>(y, c) * params.yuv_offset.w - params.yuv_offset.xyz;
    return vec3<f32>(dot(params.yuv_r.xyz, yuv), dot(params.yuv_g.xyz, yuv), dot(params.yuv_b.xyz, yuv));
}

fn luma_at(uv: vec2<f32>) -> f32 {
    return textureSampleLevel(plane0, samp, uv, 0.0).r * params.yuv_offset.w;
}

fn pq_eotf(e: vec3<f32>) -> vec3<f32> {
    let m1 = 0.1593017578125; let m2 = 78.84375;
    let c1 = 0.8359375; let c2 = 18.8515625; let c3 = 18.6875;
    let p = pow(clamp(e, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / m2));
    return pow(max(p - c1, vec3<f32>(0.0)) / (c2 - c3 * p), vec3<f32>(1.0 / m1)) * 10000.0;
}

fn hlg_inverse_oetf(e: vec3<f32>) -> vec3<f32> {
    let a = 0.17883277; let b = 0.28466892; let c = 0.55991073;
    var o: vec3<f32>;
    for (var i = 0; i < 3; i++) {
        let x = e[i];
        if (x <= 0.5) { o[i] = x * x / 3.0; } else { o[i] = (exp((x - c) / a) + b) / 12.0; }
    }
    return o;
}

fn bt2020_to_709(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        dot(vec3<f32>(1.6605, -0.5876, -0.0728), c),
        dot(vec3<f32>(-0.1246, 1.1329, -0.0083), c),
        dot(vec3<f32>(-0.0182, -0.1006, 1.1187), c));
}

// Linear light (1.0 = SDR reference white) -> displayable linear 0..1.
fn tonemap(c: vec3<f32>, peak: f32) -> vec3<f32> {
    let m = max(max(c.r, c.g), c.b);
    if (m <= 1e-6) { return c; }
    let mapped = m * (1.0 + m / (peak * peak)) / (1.0 + m);
    return c * (mapped / m);
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    return pow(max(c, vec3<f32>(0.0)), vec3<f32>(2.2));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let eye = pc.eye;
    let near = params.inv_view_proj[eye] * vec4<f32>(in.ndc, 0.0, 1.0);
    let far = params.inv_view_proj[eye] * vec4<f32>(in.ndc, 1.0, 1.0);
    let origin = near.xyz / near.w;
    let world_dir = normalize(far.xyz / far.w - origin);

    if (params.screen.w < 0.5) {
        return params.bg_color;
    }

    var uv: vec2<f32>;
    let kind = params.mode.x;
    if (kind == 0u) {
        uv = flat_uv(origin, world_dir);
    } else {
        // Per-eye stereo corrections, then the global correction rotation.
        let s = select(-0.5, 0.5, eye == 1u);
        var d = rot_y(params.proj.w * s) * world_dir;
        d = rot_x(params.lens.z * s) * d;
        d = rot_z(params.lens.w * s) * d;
        d = normalize((params.correction * vec4<f32>(d, 0.0)).xyz);
        d = zoom_dir(d, params.proj.z);
        if (kind == 1u) {
            uv = equirect_uv(d, params.proj.x, params.proj.y);
        } else if (kind == 2u) {
            uv = fisheye_uv(d, params.proj.x, params.lens.x, params.lens.y);
        } else {
            uv = eac_uv(d, params.proj.x);
        }
    }
    if (uv.x < 0.0) {
        return params.bg_color;
    }
    let tuv = to_eye_rect(uv, eye);
    var rgb = sample_yuv(tuv);

    // Sharpen: unsharp mask on luma.
    if (params.extra.x > 0.0) {
        let dx = vec2<f32>(params.tex_size.z, 0.0);
        let dy = vec2<f32>(0.0, params.tex_size.w);
        let yc = luma_at(tuv);
        let blur = (luma_at(tuv + dx) + luma_at(tuv - dx) + luma_at(tuv + dy) + luma_at(tuv - dy)) * 0.25;
        rgb += vec3<f32>((yc - blur) * params.extra.x * 2.0);
    }

    var lin: vec3<f32>;
    let transfer = u32(params.extra.y + 0.5);
    if (transfer == 1u) {
        var nits = pq_eotf(rgb);
        if (params.extra.z > 0.5) { nits = bt2020_to_709(nits); }
        lin = tonemap(max(nits, vec3<f32>(0.0)) / 203.0, 1000.0 / 203.0);
    } else if (transfer == 2u) {
        var scene = hlg_inverse_oetf(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)));
        if (params.extra.z > 0.5) { scene = bt2020_to_709(scene); }
        // HLG OOTF for a 1000-nit display, normalised to 203-nit white.
        let ys = dot(vec3<f32>(0.2627, 0.6780, 0.0593), max(scene, vec3<f32>(0.0)));
        lin = tonemap(max(scene, vec3<f32>(0.0)) * pow(max(ys, 1e-6), 0.2) * (1000.0 / 203.0), 1000.0 / 203.0);
    } else {
        lin = srgb_to_linear(rgb);
    }

    // Picture adjustments, applied in a perceptual (gamma) space.
    var g = pow(max(lin, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.2));
    g = (g - 0.5) * params.picture.y + 0.5 + params.picture.x;
    let luma = dot(g, vec3<f32>(0.2126, 0.7152, 0.0722));
    g = mix(vec3<f32>(luma), g, params.picture.z);
    g = pow(max(g, vec3<f32>(0.0)), vec3<f32>(1.0 / max(params.picture.w, 0.05)));
    return vec4<f32>(srgb_to_linear(clamp(g, vec3<f32>(0.0), vec3<f32>(1.0))), 1.0);
}
