// Shared helpers, prepended to every shader by build.rs.

fn srgb_oetf3(l: vec3<f32>) -> vec3<f32> {
    let c = max(l, vec3<f32>(0.0));
    let lo = c * 12.92;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

fn srgb_eotf3(e: vec3<f32>) -> vec3<f32> {
    let lo = e / 12.92;
    let hi = pow((e + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, e <= vec3<f32>(0.04045));
}
