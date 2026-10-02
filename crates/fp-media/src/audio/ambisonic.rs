//! First-order ambisonics (AmbiX: ACN order W, Y, Z, X with SN3D) rendered
//! to stereo for the listener's head orientation, so 360° audio stays fixed
//! to the scene as the viewer turns.

/// Head orientation as an OpenXR quaternion (x, y, z, w): head → world.
pub type Quat = [f32; 4];

pub const IDENTITY: Quat = [0.0, 0.0, 0.0, 1.0];

/// Rotates vector `v` by the inverse of unit quaternion `q`.
fn rotate_inverse(q: Quat, v: [f32; 3]) -> [f32; 3] {
    let (qx, qy, qz, qw) = (-q[0], -q[1], -q[2], q[3]);
    // v' = v + 2w(q×v) + 2 q×(q×v)
    let t = [
        2.0 * (qy * v[2] - qz * v[1]),
        2.0 * (qz * v[0] - qx * v[2]),
        2.0 * (qx * v[1] - qy * v[0]),
    ];
    [
        v[0] + qw * t[0] + (qy * t[2] - qz * t[1]),
        v[1] + qw * t[1] + (qz * t[0] - qx * t[2]),
        v[2] + qw * t[2] + (qx * t[1] - qy * t[0]),
    ]
}

/// Ambisonic axes (X front, Y left, Z up) from OpenXR axes (X right, Y up,
/// Z back), and back.
fn xr_to_amb(v: [f32; 3]) -> [f32; 3] {
    [-v[2], -v[0], v[1]]
}
fn amb_to_xr(a: [f32; 3]) -> [f32; 3] {
    [-a[1], a[2], -a[0]]
}

/// Renders interleaved 4-channel AmbiX frames to interleaved stereo.
pub fn render_stereo(foa: &[f32], head: Quat) -> Vec<f32> {
    // Directional components rotate like a vector into head space.
    let rot = |a: [f32; 3]| xr_to_amb(rotate_inverse(head, amb_to_xr(a)));
    // Two virtual cardioid microphones at ±60° from the front.
    let (c, s) = (60f32.to_radians().cos(), 60f32.to_radians().sin());
    let mut out = Vec::with_capacity(foa.len() / 2);
    for f in foa.chunks_exact(4) {
        let (w, y, z, x) = (f[0], f[1], f[2], f[3]);
        let [x, y, _z] = rot([x, y, z]);
        out.push(0.5 * (w + c * x + s * y));
        out.push(0.5 * (w + c * x - s * y));
    }
    out
}

/// Quaternion for a yaw (turn left positive) in radians about OpenXR +Y.
pub fn yaw(radians: f32) -> Quat {
    [0.0, (radians / 2.0).sin(), 0.0, (radians / 2.0).cos()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source straight ahead, encoded in SN3D first order.
    fn front_source() -> Vec<f32> {
        vec![1.0, 0.0, 0.0, 1.0] // W, Y, Z, X
    }
    fn left_source() -> Vec<f32> {
        vec![1.0, 1.0, 0.0, 0.0]
    }

    #[test]
    fn front_source_is_centred() {
        let out = render_stereo(&front_source(), IDENTITY);
        assert!((out[0] - out[1]).abs() < 1e-5);
    }

    #[test]
    fn left_source_is_louder_left() {
        let out = render_stereo(&left_source(), IDENTITY);
        assert!(out[0] > out[1] + 0.5, "{out:?}");
    }

    #[test]
    fn turning_left_moves_left_source_to_front() {
        // Head turned 90° left: the left source is now straight ahead.
        let out = render_stereo(&left_source(), yaw(std::f32::consts::FRAC_PI_2));
        assert!((out[0] - out[1]).abs() < 1e-4, "{out:?}");
        // And a front source is now on the right.
        let out = render_stereo(&front_source(), yaw(std::f32::consts::FRAC_PI_2));
        assert!(out[1] > out[0] + 0.5, "{out:?}");
    }
}
