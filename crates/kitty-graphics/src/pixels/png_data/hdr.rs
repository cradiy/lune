use std::sync::LazyLock;

use moxcms::{
    CicpColorPrimaries, CicpProfile, ColorProfile, MatrixCoefficients, TransferCharacteristics,
};
use png::CodingIndependentCodePoints;

use super::{SRGB_STEPS, encode_linear, normalize_narrow};

const REFERENCE_WHITE: f32 = 203.0;
const HLG_PEAK: f32 = 1_000.0;
const EXPOSURE_SHIFT: u32 = 18;
const EXPOSURE_STRIDE: u32 = 1 << EXPOSURE_SHIFT;
const EXPOSURE_START: u32 = 95 << 23; // 2^-32
const EXPOSURE_END: u32 = 4.0f32.to_bits();
const EXPOSURE_LEN: usize = ((EXPOSURE_END - EXPOSURE_START) >> EXPOSURE_SHIFT) as usize + 1;
const SAMPLE_STRIDE: usize = 16;
const SRGB_LUMINANCE: [f32; 3] = [0.212_639, 0.715_169, 0.072_192];
const IDENTITY: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

pub(super) struct HdrTransform {
    samples: Vec<f32>,
    matrix: [[f32; 3]; 3],
    luminance: [f32; 3],
    hlg_gain: Option<&'static [f32; EXPOSURE_LEN]>,
}

impl HdrTransform {
    pub(super) fn new(
        codes: CodingIndependentCodePoints,
        srgb: &ColorProfile,
        gray: bool,
        wide: bool,
    ) -> Option<Self> {
        let primaries = CicpColorPrimaries::try_from(codes.color_primaries).ok()?;
        let transfer = TransferCharacteristics::try_from(codes.transfer_function).ok()?;
        let decode = match transfer {
            TransferCharacteristics::Smpte2084 => pq_luminance,
            TransferCharacteristics::Hlg => hlg_scene_light,
            _ => return None,
        };
        if matches!(
            primaries,
            CicpColorPrimaries::Reserved
                | CicpColorPrimaries::Unspecified
                | CicpColorPrimaries::Xyz
        ) {
            return None;
        }
        let matrix = if gray || primaries == CicpColorPrimaries::Bt709 {
            IDENTITY
        } else {
            let profile = ColorProfile::new_from_cicp(CicpProfile {
                color_primaries: primaries,
                transfer_characteristics: TransferCharacteristics::Linear,
                matrix_coefficients: MatrixCoefficients::Identity,
                full_range: true,
            });
            profile.cicp.as_ref()?;
            profile.transform_matrix(srgb).to_f32().v
        };
        let size = if wide { 65536 } else { 256 };
        let range_scale = if wide { 256.0 } else { 1.0 };
        let mut samples = Vec::new();
        samples.try_reserve_exact(size).ok()?;
        let linear = |sample: usize| {
            let level = if codes.is_video_full_range_image {
                sample as f32 / (size - 1) as f32
            } else {
                normalize_narrow(sample as f32, range_scale)
            };
            decode(f64::from(level.abs())).copysign(f64::from(level)) as f32
        };
        if wide {
            // Interpolate the transfer curve once, keeping a single lookup per channel.
            let mut first = linear(0);
            for end in (SAMPLE_STRIDE..=size).step_by(SAMPLE_STRIDE) {
                let last = linear(end);
                let step = (last - first) / SAMPLE_STRIDE as f32;
                samples.extend((0..SAMPLE_STRIDE).map(|delta| first + step * delta as f32));
                first = last;
            }
        } else {
            samples.extend((0..size).map(linear));
        }
        let [red_y, green_y, blue_y] = SRGB_LUMINANCE;
        let luminance = std::array::from_fn(|axis| {
            red_y * matrix[0][axis] + green_y * matrix[1][axis] + blue_y * matrix[2][axis]
        });
        Some(Self {
            samples,
            matrix,
            luminance,
            hlg_gain: (transfer == TransferCharacteristics::Hlg).then(|| &*HLG_GAIN),
        })
    }

    #[inline]
    pub(super) fn convert(&self, samples: [usize; 3], encode: &[u8; SRGB_STEPS + 1]) -> [u8; 3] {
        let [r, g, b] = samples.map(|sample| self.samples[sample]);
        let [red_y, green_y, blue_y] = self.luminance;
        let luminance = red_y * r + green_y * g + blue_y * b;
        if luminance <= 0.0 {
            return [0; 3];
        }
        let gain = if let Some(table) = self.hlg_gain {
            hlg_gain(luminance, table)
        } else {
            tone_gain(luminance, 1.0 / REFERENCE_WHITE)
        };
        self.matrix
            .map(|[x, y, z]| encode_linear((x * r + y * g + z * b) * gain, encode))
    }
}

// Sample in floating-point order to keep relative precision near black.
static HLG_GAIN: LazyLock<[f32; EXPOSURE_LEN]> = LazyLock::new(|| {
    std::array::from_fn(|index| {
        let luminance = f32::from_bits(EXPOSURE_START + index as u32 * EXPOSURE_STRIDE);
        exact_hlg_gain(luminance)
    })
});

#[inline]
fn hlg_gain(luminance: f32, table: &[f32; EXPOSURE_LEN]) -> f32 {
    let offset = luminance.to_bits().wrapping_sub(EXPOSURE_START);
    let index = (offset >> EXPOSURE_SHIFT) as usize;
    if index + 1 < EXPOSURE_LEN {
        let first = table[index];
        let weight = (offset & (EXPOSURE_STRIDE - 1)) as f32 / EXPOSURE_STRIDE as f32;
        first + (table[index + 1] - first) * weight
    } else {
        exact_hlg_gain(luminance)
    }
}

fn exact_hlg_gain(luminance: f32) -> f32 {
    // Apply the BT.2100 HLG system gamma for a 1,000 cd/m² reference display.
    tone_gain(
        luminance,
        (HLG_PEAK / REFERENCE_WHITE) * luminance.powf(0.2),
    )
}

#[inline]
fn tone_gain(luminance: f32, exposure: f32) -> f32 {
    let relative_white = luminance * exposure;
    // A continuous, unit-slope knee reserves the top quarter for highlights.
    if relative_white <= 0.75 {
        exposure
    } else {
        (1.0 - 0.0625 / (relative_white - 0.5)) / luminance
    }
}

fn pq_luminance(encoded: f64) -> f64 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 32.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 128.0;
    const C3: f64 = 2392.0 / 128.0;
    let power = encoded.powf(1.0 / M2);
    let normalized = ((power - C1).max(0.0) / (C2 - C3 * power)).powf(1.0 / M1);
    normalized * 10_000.0
}

fn hlg_scene_light(encoded: f64) -> f64 {
    const A: f64 = 0.178_832_77;
    const B: f64 = 1.0 - 4.0 * A;
    const C: f64 = 0.559_910_73;
    if encoded <= 0.5 {
        encoded * encoded / 3.0
    } else {
        (((encoded - C) / A).exp() + B) / 12.0
    }
}
