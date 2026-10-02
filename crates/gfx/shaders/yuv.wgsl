// Two-plane Y'CbCr (NV12 / P010) -> display-linear RGBA16F.
// CPU reference: src/color.rs (ColorPush::shade). Keep the two in sync.

struct ColorPush {
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
    // (code_scale, y_offset, y_mul, c_offset)
    range: vec4<f32>,
    // (c_mul, exposure_mul, contrast, saturation)
    adjust: vec4<f32>,
    // (source_peak_nits, target_peak_nits, sharpen, unused)
    tone: vec4<f32>,
    // (transfer, tone_map, gamut_2020_to_709, unused)
    flags: vec4<u32>,
}

var<immediate> pc: ColorPush;

@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var chroma: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var dst: texture_storage_2d<rgba16float, write>;

const PQ_M1: f32 = 0.1593017578125;
const PQ_M2: f32 = 78.84375;
const PQ_C1: f32 = 0.8359375;
const PQ_C2: f32 = 18.8515625;
const PQ_C3: f32 = 18.6875;

fn pq_eotf(e: f32) -> f32 {
    let p = pow(clamp(e, 0.0, 1.0), 1.0 / PQ_M2);
    let num = max(p - PQ_C1, 0.0);
    let den = PQ_C2 - PQ_C3 * p;
    return 10000.0 * pow(num / den, 1.0 / PQ_M1);
}

fn pq_inverse_eotf(nits: f32) -> f32 {
    let y = pow(clamp(nits / 10000.0, 0.0, 1.0), PQ_M1);
    return pow((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y), PQ_M2);
}

fn hlg_inverse_oetf(e: f32) -> f32 {
    let x = clamp(e, 0.0, 1.0);
    let lo = x * x / 3.0;
    let hi = (exp((x - 0.55991073) / 0.17883277) + 0.28466892) / 12.0;
    return select(hi, lo, x <= 0.5);
}

fn hlg_to_nits(rgb: vec3<f32>) -> vec3<f32> {
    // BT.2100 OOTF for a 1000-nit nominal display: system gamma 1.2.
    let s = vec3<f32>(hlg_inverse_oetf(rgb.r), hlg_inverse_oetf(rgb.g), hlg_inverse_oetf(rgb.b));
    let ys = dot(s, vec3<f32>(0.2627, 0.6780, 0.0593));
    return s * (1000.0 * pow(max(ys, 1e-6), 0.2));
}

fn bt2390_eetf(nits: f32, src_peak: f32, dst_peak: f32) -> f32 {
    if (src_peak <= dst_peak) {
        return min(nits, dst_peak);
    }
    let src_pq = pq_inverse_eotf(src_peak);
    let e1 = pq_inverse_eotf(nits) / src_pq;
    let max_lum = pq_inverse_eotf(dst_peak) / src_pq;
    let ks = 1.5 * max_lum - 0.5;
    var e2 = e1;
    if (e1 >= ks) {
        let t = min((e1 - ks) / (1.0 - ks), 1.0);
        let t2 = t * t;
        let t3 = t2 * t;
        e2 = (2.0 * t3 - 3.0 * t2 + 1.0) * ks + (t3 - 2.0 * t2 + t) * (1.0 - ks) + (-2.0 * t3 + 3.0 * t2) * max_lum;
    }
    return pq_eotf(e2 * src_pq);
}

fn hable_partial(x: f32) -> f32 {
    let a = 0.15; let b = 0.50; let c = 0.10; let d = 0.20; let e = 0.02; let f = 0.30;
    return ((x * (a * x + c * b) + d * e) / (x * (a * x + b) + d * f)) - e / f;
}

fn aces_partial(x: f32) -> f32 {
    return (x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14);
}

fn tone_map_value(op: u32, nits: f32, src_peak: f32, dst_peak: f32) -> f32 {
    let x = max(nits, 0.0);
    let white = max(src_peak / dst_peak, 1.0);
    var out = x / dst_peak;
    switch op {
        case 1u: { out = bt2390_eetf(x, src_peak, dst_peak) / dst_peak; }
        case 2u: { out = max(hable_partial(min(x / dst_peak, white)) / hable_partial(white), 0.0); }
        case 3u: { out = max(aces_partial(min(x / dst_peak, white)) / aces_partial(white), 0.0); }
        default: {}
    }
    return clamp(out, 0.0, 1.0);
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let size = textureDimensions(dst);
    if (gid.x >= size.x || gid.y >= size.y) {
        return;
    }
    let p = vec2<i32>(gid.xy);
    let maxp = vec2<i32>(size) - vec2<i32>(1);
    let code_scale = pc.range.x;
    let y_off = pc.range.y;
    let y_mul = pc.range.z;
    let c_off = pc.range.w;

    let y_raw = textureLoad(luma, p, 0).r;
    var yn = (y_raw * code_scale - y_off) * y_mul;
    let sharpen = pc.tone.z;
    if (sharpen > 0.0) {
        let l = textureLoad(luma, clamp(p + vec2<i32>(-1, 0), vec2<i32>(0), maxp), 0).r;
        let r = textureLoad(luma, clamp(p + vec2<i32>(1, 0), vec2<i32>(0), maxp), 0).r;
        let u = textureLoad(luma, clamp(p + vec2<i32>(0, -1), vec2<i32>(0), maxp), 0).r;
        let d = textureLoad(luma, clamp(p + vec2<i32>(0, 1), vec2<i32>(0), maxp), 0).r;
        let yavg = (0.25 * (l + r + u + d) * code_scale - y_off) * y_mul;
        yn = yn + sharpen * (yn - yavg);
    }
    // Chroma is half resolution; bilinear upsampling via the sampler.
    let uv = (vec2<f32>(gid.xy) + vec2<f32>(0.5)) / vec2<f32>(size);
    let c = (textureSampleLevel(chroma, samp, uv, 0.0).rg * code_scale - vec2<f32>(c_off)) * pc.adjust.x;
    let v = vec3<f32>(yn, c.x, c.y);
    let rgb_p = clamp(vec3<f32>(dot(pc.m0.xyz, v), dot(pc.m1.xyz, v), dot(pc.m2.xyz, v)), vec3<f32>(0.0), vec3<f32>(1.0));

    let transfer = pc.flags.x;
    var rgb: vec3<f32>;
    if (transfer == 1u) {
        rgb = vec3<f32>(pq_eotf(rgb_p.r), pq_eotf(rgb_p.g), pq_eotf(rgb_p.b));
    } else if (transfer == 2u) {
        rgb = hlg_to_nits(rgb_p);
    } else {
        rgb = srgb_eotf3(rgb_p);
    }
    if (pc.flags.z != 0u) {
        let g = mat3x3<f32>(
            vec3<f32>(1.660491, -0.124550, -0.018151),
            vec3<f32>(-0.587641, 1.132900, -0.100579),
            vec3<f32>(-0.072850, -0.008349, 1.118730),
        );
        rgb = max(g * rgb, vec3<f32>(0.0));
    }
    rgb = rgb * pc.adjust.y;
    if (transfer == 0u) {
        rgb = min(rgb, vec3<f32>(1.0));
    } else {
        let m = max(rgb.r, max(rgb.g, rgb.b));
        if (m <= 1e-6) {
            rgb = vec3<f32>(0.0);
        } else {
            rgb = rgb * (tone_map_value(pc.flags.y, m, pc.tone.x, pc.tone.y) / m);
        }
    }
    rgb = 0.18 * pow(max(rgb, vec3<f32>(0.0)) / 0.18, vec3<f32>(pc.adjust.z));
    let luma_l = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    rgb = clamp(vec3<f32>(luma_l) + (rgb - vec3<f32>(luma_l)) * pc.adjust.w, vec3<f32>(0.0), vec3<f32>(1.0));
    textureStore(dst, p, vec4<f32>(rgb, 1.0));
}
