//! Colour pipeline: CPU reference of the YUV → linear RGB compute shader
//! (`shaders/yuv.wgsl`) and the push-constant block that parametrises it.
//!
//! Per pixel:
//! 1. code values → normalized Y'CbCr (bit depth, limited/full range),
//! 2. optional unsharp mask on Y' (`y + s·(y − avg4)`),
//! 3. Y'CbCr → R'G'B' (BT.601 / BT.709 / BT.2020-NCL),
//! 4. EOTF to display-linear light: SDR uses the exact inverse of the sRGB
//!    OETF the swapchain applies, so SDR content round-trips untouched; PQ
//!    and HLG decode to absolute nits,
//! 5. BT.2020 → BT.709 gamut conversion (clipped at 0),
//! 6. exposure (`2^ev`),
//! 7. tone mapping on max(R,G,B) (hue preserving) from the source peak to the
//!    display peak: BT.2390 EETF, Hable, ACES-fit or clip, output 1.0 = display white,
//! 8. contrast (power curve pivoting at 18 % grey) and saturation (BT.709 luma mix).

use bytemuck::{Pod, Zeroable};
use fp_core::{ColorTransfer, Corrections};

/// Y'CbCr matrix coefficients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum YuvMatrix {
    Bt601,
    #[default]
    Bt709,
    /// BT.2020 non-constant luminance.
    Bt2020,
}

impl YuvMatrix {
    /// `(Kr, Kb)`.
    pub fn kr_kb(self) -> (f32, f32) {
        match self {
            YuvMatrix::Bt601 => (0.299, 0.114),
            YuvMatrix::Bt709 => (0.2126, 0.0722),
            YuvMatrix::Bt2020 => (0.2627, 0.0593),
        }
    }

    /// Row-major 3×3 matrix mapping `(Y', Cb, Cr)` (Cb/Cr centred on 0,
    /// range ±0.5) to `(R', G', B')`.
    pub fn to_rgb(self) -> [[f32; 3]; 3] {
        let (kr, kb) = self.kr_kb();
        let kg = 1.0 - kr - kb;
        [
            [1.0, 0.0, 2.0 * (1.0 - kr)],
            [
                1.0,
                -2.0 * kb * (1.0 - kb) / kg,
                -2.0 * kr * (1.0 - kr) / kg,
            ],
            [1.0, 2.0 * (1.0 - kb), 0.0],
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorRange {
    /// "TV" range: Y 16–235, C 16–240 (scaled for higher bit depths).
    #[default]
    Limited,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Primaries {
    #[default]
    Bt709,
    Bt2020,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToneMapOp {
    Clip,
    /// ITU-R BT.2390 EETF (hermite knee in PQ space).
    #[default]
    Bt2390,
    /// John Hable's filmic curve, normalized to the source peak.
    Hable,
    /// Narkowicz ACES fit, normalized to the source peak.
    Aces,
}

/// How the decoded samples should be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ColorInfo {
    pub matrix: YuvMatrix,
    pub range: ColorRange,
    pub transfer: ColorTransfer,
    pub primaries: Primaries,
    /// Significant bits per sample (8 or 10).
    pub bit_depth: u8,
    /// Mastering / MaxCLL peak in nits for HDR sources (0 = unknown → 1000).
    pub source_peak_nits: f32,
}

impl ColorInfo {
    /// Typical SDR HD video.
    pub const SDR_709: ColorInfo = ColorInfo {
        matrix: YuvMatrix::Bt709,
        range: ColorRange::Limited,
        transfer: ColorTransfer::Sdr,
        primaries: Primaries::Bt709,
        bit_depth: 8,
        source_peak_nits: 0.0,
    };
    /// Typical HDR10.
    pub const HDR10: ColorInfo = ColorInfo {
        matrix: YuvMatrix::Bt2020,
        range: ColorRange::Limited,
        transfer: ColorTransfer::Pq,
        primaries: Primaries::Bt2020,
        bit_depth: 10,
        source_peak_nits: 1000.0,
    };
}

/// Display-side settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayParams {
    /// Luminance of display white in nits. [verify] Frame LCD peak.
    pub target_peak_nits: f32,
    pub tone_map: ToneMapOp,
}

impl Default for DisplayParams {
    fn default() -> Self {
        DisplayParams {
            target_peak_nits: 200.0,
            tone_map: ToneMapOp::Bt2390,
        }
    }
}

/// How samples are stored in the plane textures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleStorage {
    /// 8-bit UNORM (NV12).
    U8,
    /// 16-bit UNORM container with the value in the top `bits` (P010: 10).
    MsbAligned16 { bits: u8 },
}

impl SampleStorage {
    /// Factor turning a sampled UNORM value back into an integer code value.
    pub fn code_scale(self) -> f32 {
        match self {
            SampleStorage::U8 => 255.0,
            SampleStorage::MsbAligned16 { bits } => 65535.0 / (1u32 << (16 - bits as u32)) as f32,
        }
    }
}

pub const TRANSFER_SDR: u32 = 0;
pub const TRANSFER_PQ: u32 = 1;
pub const TRANSFER_HLG: u32 = 2;
pub const TONEMAP_CLIP: u32 = 0;
pub const TONEMAP_BT2390: u32 = 1;
pub const TONEMAP_HABLE: u32 = 2;
pub const TONEMAP_ACES: u32 = 3;

/// HLG nominal peak (BT.2100 reference display).
pub const HLG_PEAK_NITS: f32 = 1000.0;

/// Push constants for the YUV compute pass (112 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct ColorPush {
    /// Rows of the Y'CbCr → R'G'B' matrix (w unused).
    pub m0: [f32; 4],
    pub m1: [f32; 4],
    pub m2: [f32; 4],
    /// `(code_scale, y_offset, y_mul, c_offset)`.
    pub range: [f32; 4],
    /// `(c_mul, exposure_mul, contrast, saturation)`.
    pub adjust: [f32; 4],
    /// `(source_peak_nits, target_peak_nits, sharpen, unused)`.
    pub tone: [f32; 4],
    /// `(transfer, tone_map, gamut_2020_to_709, unused)`.
    pub flags: [u32; 4],
}

impl ColorPush {
    pub fn new(
        info: &ColorInfo,
        storage: SampleStorage,
        display: &DisplayParams,
        c: &Corrections,
    ) -> ColorPush {
        let m = info.matrix.to_rgb();
        let bits = info.bit_depth.clamp(8, 16) as i32;
        let max_code = ((1u32 << bits) - 1) as f32;
        let scale = (1u32 << (bits - 8)) as f32;
        let (y_off, y_range, c_off, c_range) = match info.range {
            ColorRange::Limited => (16.0 * scale, 219.0 * scale, 128.0 * scale, 224.0 * scale),
            ColorRange::Full => (0.0, max_code, 128.0 * scale, max_code),
        };
        let transfer = match info.transfer {
            ColorTransfer::Sdr => TRANSFER_SDR,
            ColorTransfer::Pq => TRANSFER_PQ,
            ColorTransfer::Hlg => TRANSFER_HLG,
        };
        let source_peak = match info.transfer {
            ColorTransfer::Hlg => HLG_PEAK_NITS,
            _ if info.source_peak_nits > 0.0 => info.source_peak_nits,
            _ => 1000.0,
        };
        let tone_map = match (info.transfer, display.tone_map) {
            (ColorTransfer::Sdr, _) | (_, ToneMapOp::Clip) => TONEMAP_CLIP,
            (_, ToneMapOp::Bt2390) => TONEMAP_BT2390,
            (_, ToneMapOp::Hable) => TONEMAP_HABLE,
            (_, ToneMapOp::Aces) => TONEMAP_ACES,
        };
        ColorPush {
            m0: [m[0][0], m[0][1], m[0][2], 0.0],
            m1: [m[1][0], m[1][1], m[1][2], 0.0],
            m2: [m[2][0], m[2][1], m[2][2], 0.0],
            range: [storage.code_scale(), y_off, 1.0 / y_range, c_off],
            adjust: [
                1.0 / c_range,
                c.exposure_ev.exp2(),
                c.contrast.max(0.01),
                c.saturation.max(0.0),
            ],
            tone: [
                source_peak,
                display.target_peak_nits.max(1.0),
                c.sharpen.max(0.0),
                0.0,
            ],
            flags: [
                transfer,
                tone_map,
                (info.primaries == Primaries::Bt2020) as u32,
                0,
            ],
        }
    }

    /// CPU mirror of the compute shader for one pixel. Inputs are the raw
    /// sampled UNORM values; `y_avg4` is the mean of the four luma neighbours
    /// (pass `y` to disable sharpening). Returns display-linear RGB.
    pub fn shade(&self, y: f32, cb: f32, cr: f32, y_avg4: f32) -> [f32; 3] {
        let [code_scale, y_off, y_mul, c_off] = self.range;
        let [c_mul, exposure, contrast, saturation] = self.adjust;
        let [src_peak, dst_peak, sharpen, _] = self.tone;
        let mut yn = (y * code_scale - y_off) * y_mul;
        let yavg = (y_avg4 * code_scale - y_off) * y_mul;
        yn += sharpen * (yn - yavg);
        let cbn = (cb * code_scale - c_off) * c_mul;
        let crn = (cr * code_scale - c_off) * c_mul;
        let v = [yn, cbn, crn];
        let dot = |r: [f32; 4]| r[0] * v[0] + r[1] * v[1] + r[2] * v[2];
        let rgb_p = [dot(self.m0), dot(self.m1), dot(self.m2)].map(|x| x.clamp(0.0, 1.0));

        // Display-linear, in units of display white for SDR, nits for HDR.
        let mut rgb = match self.flags[0] {
            TRANSFER_PQ => rgb_p.map(pq_eotf),
            TRANSFER_HLG => hlg_to_nits(rgb_p, HLG_PEAK_NITS),
            _ => rgb_p.map(srgb_eotf),
        };
        if self.flags[2] != 0 {
            rgb = mat3_mul(&BT2020_TO_BT709, rgb).map(|x| x.max(0.0));
        }
        rgb = rgb.map(|x| x * exposure);
        rgb = if self.flags[0] == TRANSFER_SDR {
            rgb.map(|x| x.min(1.0))
        } else {
            tone_map_rgb(self.flags[1], rgb, src_peak, dst_peak)
        };
        let rgb = rgb.map(|x| apply_contrast(x, contrast));
        apply_saturation(rgb, saturation).map(|x| x.clamp(0.0, 1.0))
    }
}

/// Linear-light BT.2020 → BT.709 primaries.
pub const BT2020_TO_BT709: [[f32; 3]; 3] = [
    [1.660_491, -0.587_641, -0.072_850],
    [-0.124_550, 1.132_9, -0.008_349],
    [-0.018_151, -0.100_579, 1.118_73],
];

pub fn mat3_mul(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|i| m[i][0] * v[0] + m[i][1] * v[1] + m[i][2] * v[2])
}

const PQ_M1: f32 = 2610.0 / 16384.0;
const PQ_M2: f32 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f32 = 3424.0 / 4096.0;
const PQ_C2: f32 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f32 = 2392.0 / 4096.0 * 32.0;

/// SMPTE ST 2084 EOTF: signal `[0,1]` → nits.
pub fn pq_eotf(e: f32) -> f32 {
    let p = e.clamp(0.0, 1.0).powf(1.0 / PQ_M2);
    let num = (p - PQ_C1).max(0.0);
    let den = PQ_C2 - PQ_C3 * p;
    10000.0 * (num / den).powf(1.0 / PQ_M1)
}

/// Inverse ST 2084 EOTF: nits → signal.
pub fn pq_inverse_eotf(nits: f32) -> f32 {
    let y = (nits / 10000.0).clamp(0.0, 1.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y)).powf(PQ_M2)
}

const HLG_A: f32 = 0.178_832_77;
const HLG_B: f32 = 0.284_668_92; // 1 - 4a
const HLG_C: f32 = 0.559_910_7; // 0.5 - a ln(4a)

/// HLG inverse OETF: signal → normalized scene light `[0,1]`.
pub fn hlg_inverse_oetf(e: f32) -> f32 {
    let e = e.clamp(0.0, 1.0);
    if e <= 0.5 {
        e * e / 3.0
    } else {
        (((e - HLG_C) / HLG_A).exp() + HLG_B) / 12.0
    }
}

/// HLG signal → display nits via inverse OETF + BT.2100 OOTF
/// (system gamma 1.2 at `peak` = 1000 nits).
pub fn hlg_to_nits(rgb: [f32; 3], peak: f32) -> [f32; 3] {
    let s = rgb.map(hlg_inverse_oetf);
    let ys = 0.2627 * s[0] + 0.6780 * s[1] + 0.0593 * s[2];
    let gamma = 1.2 + 0.42 * (peak / 1000.0).log10();
    let g = peak * ys.max(1e-6).powf(gamma - 1.0);
    s.map(|x| x * g)
}

/// sRGB EOTF (inverse of the OETF an `_SRGB` swapchain applies on write).
pub fn srgb_eotf(e: f32) -> f32 {
    if e <= 0.040_45 {
        e / 12.92
    } else {
        ((e + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB OETF.
pub fn srgb_oetf(l: f32) -> f32 {
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// BT.2390 EETF mapping `nits` from a `src_peak` master to a `dst_peak`
/// display, returning nits. Identity when the source fits the display.
pub fn bt2390_eetf(nits: f32, src_peak: f32, dst_peak: f32) -> f32 {
    if src_peak <= dst_peak {
        return nits.min(dst_peak);
    }
    let src_pq = pq_inverse_eotf(src_peak);
    let e1 = pq_inverse_eotf(nits) / src_pq;
    let max_lum = pq_inverse_eotf(dst_peak) / src_pq;
    let ks = 1.5 * max_lum - 0.5;
    let e2 = if e1 < ks {
        e1
    } else {
        let t = ((e1 - ks) / (1.0 - ks)).min(1.0);
        let (t2, t3) = (t * t, t * t * t);
        (2.0 * t3 - 3.0 * t2 + 1.0) * ks
            + (t3 - 2.0 * t2 + t) * (1.0 - ks)
            + (-2.0 * t3 + 3.0 * t2) * max_lum
    };
    pq_eotf(e2 * src_pq)
}

fn hable_partial(x: f32) -> f32 {
    const A: f32 = 0.15;
    const B: f32 = 0.50;
    const C: f32 = 0.10;
    const D: f32 = 0.20;
    const E: f32 = 0.02;
    const F: f32 = 0.30;
    ((x * (A * x + C * B) + D * E) / (x * (A * x + B) + D * F)) - E / F
}

/// Hable curve normalized so `white` maps to 1.
pub fn hable(x: f32, white: f32) -> f32 {
    (hable_partial(x.min(white)) / hable_partial(white)).max(0.0)
}

fn aces_partial(x: f32) -> f32 {
    (x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14)
}

/// ACES-fit curve normalized so `white` maps to 1.
pub fn aces(x: f32, white: f32) -> f32 {
    (aces_partial(x.min(white)) / aces_partial(white)).max(0.0)
}

/// Tone-map one luminance-like value (nits) to display-relative `[0,1]`.
pub fn tone_map_value(op: u32, nits: f32, src_peak: f32, dst_peak: f32) -> f32 {
    let x = nits.max(0.0);
    let white = (src_peak / dst_peak).max(1.0);
    let out = match op {
        TONEMAP_BT2390 => bt2390_eetf(x, src_peak, dst_peak) / dst_peak,
        TONEMAP_HABLE => hable(x / dst_peak, white),
        TONEMAP_ACES => aces(x / dst_peak, white),
        _ => x / dst_peak,
    };
    out.clamp(0.0, 1.0)
}

/// Hue-preserving tone map on max(R,G,B).
pub fn tone_map_rgb(op: u32, rgb: [f32; 3], src_peak: f32, dst_peak: f32) -> [f32; 3] {
    let m = rgb[0].max(rgb[1]).max(rgb[2]);
    if m <= 1e-6 {
        return [0.0; 3];
    }
    let s = tone_map_value(op, m, src_peak, dst_peak) / m;
    rgb.map(|x| x * s)
}

/// Contrast as a power curve pivoting at 18 % grey (0 stays 0).
pub fn apply_contrast(x: f32, contrast: f32) -> f32 {
    0.18 * (x.max(0.0) / 0.18).powf(contrast)
}

/// Saturation as a mix with BT.709 luma.
pub fn apply_saturation(rgb: [f32; 3], s: f32) -> [f32; 3] {
    let l = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    rgb.map(|x| l + (x - l) * s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn push_block_size() {
        assert_eq!(std::mem::size_of::<ColorPush>(), 112);
    }

    #[test]
    fn pq_known_values() {
        assert_eq!(pq_eotf(0.0), 0.0);
        assert!(close(pq_eotf(1.0), 10000.0, 0.5));
        assert!(close(pq_inverse_eotf(100.0), 0.508_078, 1e-4));
        assert!(close(pq_inverse_eotf(1000.0), 0.751_827, 1e-4));
        assert!(close(pq_inverse_eotf(203.0), 0.580_690, 1e-4));
        for nits in [0.1, 1.0, 50.0, 400.0, 4000.0] {
            assert!(close(pq_eotf(pq_inverse_eotf(nits)), nits, nits * 1e-3));
        }
    }

    #[test]
    fn hlg_known_values() {
        assert_eq!(hlg_inverse_oetf(0.0), 0.0);
        assert!(close(hlg_inverse_oetf(0.5), 1.0 / 12.0, 1e-6));
        assert!(close(hlg_inverse_oetf(1.0), 1.0, 1e-5));
        // Peak white → nominal peak.
        let w = hlg_to_nits([1.0; 3], 1000.0);
        assert!(close(w[0], 1000.0, 0.5));
        // HLG reference white (75 % signal) ≈ 203 nits on a 1000-nit display.
        let r = hlg_to_nits([0.75; 3], 1000.0);
        assert!(close(r[1], 203.0, 3.0), "{r:?}");
    }

    #[test]
    fn bt709_matrix_on_known_colours() {
        let info = ColorInfo::SDR_709;
        let p = ColorPush::new(
            &info,
            SampleStorage::U8,
            &DisplayParams::default(),
            &Corrections::default(),
        );
        let s = |y: f32, cb: f32, cr: f32| p.shade(y / 255.0, cb / 255.0, cr / 255.0, y / 255.0);
        let white = s(235.0, 128.0, 128.0);
        assert!(white.iter().all(|&c| close(c, 1.0, 1e-5)), "{white:?}");
        assert!(s(16.0, 128.0, 128.0).iter().all(|&c| close(c, 0.0, 1e-6)));
        // 75 % grey in the signal domain round-trips through sRGB.
        let g = s(16.0 + 219.0 * 0.5, 128.0, 128.0);
        assert!(close(srgb_oetf(g[0]), 0.5, 1e-4));
        // BT.709 100 % red ≈ (63, 102, 240).
        let red = s(62.56, 102.34, 240.0);
        assert!(
            close(red[0], 1.0, 1e-3) && red[1] < 1e-3 && red[2] < 1e-3,
            "{red:?}"
        );
        let m = YuvMatrix::Bt709.to_rgb();
        // Pure blue Y'=Kb, Cb=0.5, Cr=-Kb/(2(1-Kr))… check via the matrix directly.
        let (kr, kb) = YuvMatrix::Bt709.kr_kb();
        let cb = 0.5;
        let cr = (0.0 - kb) / (2.0 * (1.0 - kr));
        let b = mat3_mul(&m, [kb, cb, cr]);
        assert!(
            close(b[0], 0.0, 1e-5) && close(b[1], 0.0, 1e-5) && close(b[2], 1.0, 1e-5),
            "{b:?}"
        );
    }

    #[test]
    fn p010_scaling() {
        let mut info = ColorInfo::HDR10;
        info.transfer = ColorTransfer::Sdr;
        info.primaries = Primaries::Bt709;
        info.matrix = YuvMatrix::Bt709;
        let st = SampleStorage::MsbAligned16 { bits: 10 };
        let p = ColorPush::new(
            &info,
            st,
            &DisplayParams::default(),
            &Corrections::default(),
        );
        let to_unorm = |code: u32| ((code << 6) as f32) / 65535.0;
        let w = p.shade(to_unorm(940), to_unorm(512), to_unorm(512), to_unorm(940));
        assert!(w.iter().all(|&c| close(c, 1.0, 1e-4)), "{w:?}");
        let k = p.shade(to_unorm(64), to_unorm(512), to_unorm(512), to_unorm(64));
        assert!(k.iter().all(|&c| c.abs() < 1e-4));
    }

    #[test]
    fn tone_curves_monotonic_and_zero() {
        for op in [TONEMAP_CLIP, TONEMAP_BT2390, TONEMAP_HABLE, TONEMAP_ACES] {
            assert!(
                tone_map_value(op, 0.0, 1000.0, 200.0).abs() < 1e-6,
                "op {op}"
            );
            let mut prev = 0.0;
            for i in 1..=400 {
                let nits = i as f32 * 2.5;
                let v = tone_map_value(op, nits, 1000.0, 200.0);
                assert!(v >= prev - 1e-6, "op {op} not monotonic at {nits}");
                assert!(v <= 1.0);
                prev = v;
            }
            if op != TONEMAP_CLIP {
                assert!(
                    close(tone_map_value(op, 1000.0, 1000.0, 200.0), 1.0, 2e-3),
                    "op {op} peak"
                );
            }
        }
        // BT.2390 leaves shadows untouched (below the knee).
        assert!(close(bt2390_eetf(20.0, 1000.0, 200.0), 20.0, 0.05));
        // Identity when the source fits.
        assert_eq!(bt2390_eetf(150.0, 200.0, 400.0), 150.0);
    }

    #[test]
    fn hdr10_pipeline() {
        let p = ColorPush::new(
            &ColorInfo::HDR10,
            SampleStorage::MsbAligned16 { bits: 10 },
            &DisplayParams::default(),
            &Corrections::default(),
        );
        assert_eq!(p.flags, [TRANSFER_PQ, TONEMAP_BT2390, 1, 0]);
        // Neutral grey at the PQ code for 1000 nits → display white.
        let e = pq_inverse_eotf(1000.0);
        let code = 64.0 + 876.0 * e;
        let y = code * 64.0 / 65535.0;
        let c = 512.0 * 64.0 / 65535.0;
        let out = p.shade(y, c, c, y);
        assert!(out.iter().all(|&v| close(v, 1.0, 5e-3)), "{out:?}");
        let k = p.shade(64.0 * 64.0 / 65535.0, c, c, 64.0 * 64.0 / 65535.0);
        assert!(k.iter().all(|&v| v.abs() < 1e-4));
    }

    #[test]
    fn gamut_white_preserved() {
        let w = mat3_mul(&BT2020_TO_BT709, [1.0; 3]);
        assert!(w.iter().all(|&c| close(c, 1.0, 1e-3)), "{w:?}");
    }

    #[test]
    fn adjustments() {
        assert_eq!(apply_contrast(0.0, 1.5), 0.0);
        assert!(close(apply_contrast(0.18, 1.7), 0.18, 1e-6));
        assert!(apply_contrast(0.5, 1.5) > 0.5);
        let grey = apply_saturation([0.2, 0.5, 0.8], 0.0);
        assert!(close(grey[0], grey[2], 1e-6));
        let same = apply_saturation([0.2, 0.5, 0.8], 1.0);
        assert!(close(same[0], 0.2, 1e-6) && close(same[2], 0.8, 1e-6));
        // Exposure +1 EV doubles linear SDR light (below clip).
        let c = Corrections {
            exposure_ev: 1.0,
            ..Default::default()
        };
        let p = ColorPush::new(
            &ColorInfo::SDR_709,
            SampleStorage::U8,
            &DisplayParams::default(),
            &c,
        );
        let y = (16.0 + 219.0 * 0.3) / 255.0;
        let base = srgb_eotf(0.3);
        assert!(close(
            p.shade(y, 128.0 / 255.0, 128.0 / 255.0, y)[0],
            base * 2.0,
            1e-4
        ));
        // Sharpening increases local contrast.
        let s = Corrections {
            sharpen: 1.0,
            ..Default::default()
        };
        let p = ColorPush::new(
            &ColorInfo::SDR_709,
            SampleStorage::U8,
            &DisplayParams::default(),
            &s,
        );
        let flat = p.shade(0.5, 0.5, 0.5, 0.5)[0];
        let peak = p.shade(0.5, 0.5, 0.5, 0.4)[0];
        assert!(peak > flat);
    }
}
