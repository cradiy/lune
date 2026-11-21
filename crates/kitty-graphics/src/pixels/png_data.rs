use std::sync::{Arc, LazyLock};
use std::{borrow::Cow, io::Cursor};

use moxcms::{
    CicpColorPrimaries, CicpProfile, ColorProfile, DataColorSpace, Layout, MatrixCoefficients,
    ToneReprCurve, TransferCharacteristics, Transform8BitExecutor, Transform16BitExecutor,
    TransformOptions, XyY, Xyzd,
};
use png::{
    BitDepth, CodingIndependentCodePoints, ColorType, DecodingError, InterlaceInfo, Reader,
    ScaledFloat, SourceChromaticities, Transformations,
};

use super::{ENCODED_OVERHEAD, INVALID, ImageFormat, Pixels, QUOTA, pixel_size, zeroed};

mod hdr;
use hdr::HdrTransform;

const BAD_PNG: &[u8] = b"EBADPNG:Invalid PNG image data";
const DISPLAY_GAMMA: f64 = 2.2;
const GAMMA_THRESHOLD: f64 = 0.05;
const GAMMA_SAMPLE_SHIFT: u32 = 5;
const GAMMA_SAMPLE_COUNT: usize = 1 << (16 - GAMMA_SAMPLE_SHIFT);
const MAX_CACHED_PROFILE_BYTES: usize = 64 * 1024;
// Half a table step changes the encoded toe of sRGB by less than one byte value.
const SRGB_STEPS: usize = 8192;

#[derive(Clone)]
enum ProfileTransform {
    Srgb,
    NarrowSrgb,
    EightBit(Layout, Arc<Transform8BitExecutor>),
    SixteenBit(Arc<Transform16BitExecutor>),
    NarrowCurve(Arc<Vec<u8>>),
    Narrow(Arc<NarrowTransform>),
    Hdr(Arc<HdrTransform>),
}

#[derive(Default)]
pub(crate) struct PngProfiles(Option<CachedProfile>);

struct CachedProfile {
    source: ProfileSource<'static>,
    depth: BitDepth,
    layout: Layout,
    transform: ProfileTransform,
}

#[derive(PartialEq)]
enum ProfileSource<'a> {
    Cicp(CodingIndependentCodePoints),
    Embedded(Cow<'a, [u8]>),
    Colorimetry(SourceChromaticities, ScaledFloat),
}

impl ProfileSource<'_> {
    fn cache_key(&self) -> Option<ProfileSource<'static>> {
        match self {
            Self::Cicp(codes) => Some(ProfileSource::Cicp(*codes)),
            Self::Embedded(bytes) if bytes.len() <= MAX_CACHED_PROFILE_BYTES => {
                let mut key = Vec::new();
                key.try_reserve_exact(bytes.len()).ok()?;
                key.extend_from_slice(bytes);
                Some(ProfileSource::Embedded(Cow::Owned(key)))
            }
            Self::Embedded(_) => None,
            Self::Colorimetry(chromaticities, gamma) => {
                Some(ProfileSource::Colorimetry(*chromaticities, *gamma))
            }
        }
    }
}

pub(super) fn decode_png(
    bytes: &[u8],
    max_bytes: usize,
    profiles: &mut PngProfiles,
) -> Result<Pixels, &'static [u8]> {
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(bytes),
        png::Limits {
            bytes: max_bytes.saturating_add(ENCODED_OVERHEAD),
        },
    );
    decoder.set_transformations(Transformations::EXPAND);
    decoder.set_ignore_text_chunk(true);
    decoder.ignore_checksums(false);
    let dimensions = decoder.read_header_info().map_err(png_error)?.size();
    let rgba_size = pixel_size(dimensions, 4, max_bytes).map_err(|error| match error {
        INVALID => b"ENOMEM:PNG image is too large",
        _ => error,
    })?;
    let mut reader = decoder.read_info().map_err(png_error)?;
    if reader
        .info()
        .frame_control()
        .is_some_and(|frame| (frame.width, frame.height) != dimensions)
    {
        return Err(BAD_PNG);
    }
    let info = reader.info();
    let depth = reader.output_color_type().1;
    let gamma = info.gamma();
    let profile = profiles.select(info, gamma, depth);
    let gamma = if profile.is_some() {
        None
    } else {
        gamma
            .map(|gamma| f64::from(gamma.into_scaled()) * (DISPLAY_GAMMA / 100_000.0))
            .filter(|&gamma| gamma > 0.0 && (gamma - 1.0).abs() > GAMMA_THRESHOLD)
    };
    let mut rgba = zeroed(rgba_size, rgba_size)?;
    if depth == BitDepth::Sixteen {
        match &profile {
            Some(ProfileTransform::NarrowSrgb) => {
                decode_png_16(&mut reader, &mut rgba, |sample| {
                    srgb_byte(normalize_narrow(
                        f32::from(u16::from_be_bytes(sample)),
                        256.0,
                    ))
                })?;
            }
            Some(ProfileTransform::SixteenBit(transform)) => {
                decode_png_profile_16(&mut reader, &mut rgba, &**transform)?;
            }
            Some(ProfileTransform::NarrowCurve(table)) => {
                let table: &[u8; 65536] = table.as_slice().try_into().expect("16-bit curve table");
                decode_png_16(&mut reader, &mut rgba, |sample| {
                    table[usize::from(u16::from_be_bytes(sample))]
                })?;
            }
            Some(ProfileTransform::Narrow(transform)) => {
                let encode = &SRGB_BYTES;
                decode_with_rgb_transform_16(&mut reader, &mut rgba, |samples| {
                    transform.convert(samples, encode)
                })?;
            }
            Some(ProfileTransform::Hdr(transform)) => {
                let encode = &SRGB_BYTES;
                decode_with_rgb_transform_16(&mut reader, &mut rgba, |samples| {
                    transform.convert(samples, encode)
                })?;
            }
            _ => {
                if let Some(gamma) = gamma {
                    let shift = gamma_sample_shift(reader.info());
                    let table = gamma_table_16(gamma, shift);
                    decode_png_16(&mut reader, &mut rgba, |sample| {
                        table[usize::from(u16::from_be_bytes(sample) >> shift)]
                    })?;
                } else {
                    decode_png_16(&mut reader, &mut rgba, |[high, _]| high)?;
                }
            }
        }
    } else {
        let curve = match &profile {
            Some(ProfileTransform::NarrowCurve(table)) => {
                Some(table.as_slice().try_into().expect("8-bit curve table"))
            }
            _ => None,
        };
        decode_png_8(&mut reader, &mut rgba, curve)?;
        let table: Option<[u8; 256]> = if matches!(profile, Some(ProfileTransform::NarrowSrgb)) {
            Some(std::array::from_fn(|color| {
                srgb_byte(normalize_narrow(color as f32, 1.0))
            }))
        } else {
            gamma
                .map(f64::recip)
                .filter(|&exponent| (exponent - 1.0).abs() > GAMMA_THRESHOLD)
                .map(|exponent| {
                    std::array::from_fn(|color| {
                        (255.0 * (color as f64 / 255.0).powf(exponent)).round() as u8
                    })
                })
        };
        if let Some(table) = table {
            map_rgb_bytes(&mut rgba, &table);
        }
    }
    reader.finish().map_err(png_error)?;
    if depth != BitDepth::Sixteen {
        match profile {
            Some(ProfileTransform::EightBit(layout, transform)) => {
                convert_profile_8(&mut rgba, layout, &*transform)?;
            }
            Some(ProfileTransform::Narrow(transform)) => {
                let encode = &SRGB_BYTES;
                convert_rgb_8(&mut rgba, |samples| transform.convert(samples, encode));
            }
            Some(ProfileTransform::Hdr(transform)) => {
                let encode = &SRGB_BYTES;
                convert_rgb_8(&mut rgba, |samples| transform.convert(samples, encode));
            }
            _ => {}
        }
    }
    Ok(Pixels {
        dimensions,
        format: ImageFormat::Rgba,
        bytes: rgba,
    })
}

impl PngProfiles {
    fn select(
        &mut self,
        info: &png::Info<'_>,
        gamma: Option<ScaledFloat>,
        depth: BitDepth,
    ) -> Option<ProfileTransform> {
        let mut transform = |source| self.transform(source, info.color_type, depth);
        info.coding_independent_code_points
            .filter(|codes| {
                codes.matrix_coefficients == 0
                    && (codes.is_video_full_range_image
                        || info.color_type == ColorType::Indexed
                        || matches!(info.bit_depth, BitDepth::Eight | BitDepth::Sixteen))
            })
            .and_then(|codes| {
                if codes.color_primaries == CicpColorPrimaries::Bt709 as u8
                    && codes.transfer_function == TransferCharacteristics::Srgb as u8
                {
                    Some(if codes.is_video_full_range_image {
                        ProfileTransform::Srgb
                    } else {
                        ProfileTransform::NarrowSrgb
                    })
                } else {
                    transform(ProfileSource::Cicp(codes))
                }
            })
            .or_else(|| {
                transform(ProfileSource::Embedded(Cow::Borrowed(
                    info.icc_profile.as_deref()?,
                )))
            })
            .or_else(|| info.srgb.map(|_| ProfileTransform::Srgb))
            .or_else(|| transform(ProfileSource::Colorimetry(info.chromaticities()?, gamma?)))
    }

    #[cold]
    fn transform(
        &mut self,
        source: ProfileSource<'_>,
        color: ColorType,
        depth: BitDepth,
    ) -> Option<ProfileTransform> {
        static SRGB: LazyLock<ColorProfile> = LazyLock::new(ColorProfile::new_srgb);
        let gray = matches!(color, ColorType::Grayscale | ColorType::GrayscaleAlpha);
        let space = if gray {
            DataColorSpace::Gray
        } else {
            DataColorSpace::Rgb
        };
        let wide = depth == BitDepth::Sixteen;
        let narrow =
            matches!(&source, ProfileSource::Cicp(codes) if !codes.is_video_full_range_image);
        let layout = match (gray, wide || narrow) {
            (true, _) => Layout::Gray,
            (false, true) => Layout::Rgb,
            (false, false) => Layout::Rgba,
        };
        if let Some(cached) = &self.0
            && cached.depth == depth
            && cached.layout == layout
            && cached.source == source
        {
            return Some(cached.transform.clone());
        }
        let transform = if let ProfileSource::Cicp(codes) = &source
            && matches!(
                TransferCharacteristics::try_from(codes.transfer_function),
                Ok(TransferCharacteristics::Smpte2084 | TransferCharacteristics::Hlg)
            ) {
            ProfileTransform::Hdr(Arc::new(HdrTransform::new(*codes, &SRGB, gray, wide)?))
        } else {
            let profile = match &source {
                ProfileSource::Cicp(codes) => cicp_profile(*codes, gray)?,
                ProfileSource::Embedded(bytes) => ColorProfile::new_from_slice(bytes).ok()?,
                ProfileSource::Colorimetry(chromaticities, gamma) => {
                    colorimetry_profile(*chromaticities, *gamma, gray)?
                }
            };
            if profile.color_space != space {
                return None;
            }
            if narrow {
                narrow_transform(profile, &SRGB, wide)?
            } else if wide {
                ProfileTransform::SixteenBit(
                    profile
                        .create_transform_16bit(layout, &SRGB, Layout::Rgb, Default::default())
                        .ok()?,
                )
            } else {
                let dest = if gray { Layout::Rgb } else { Layout::Rgba };
                ProfileTransform::EightBit(
                    layout,
                    profile
                        .create_transform_8bit(layout, &SRGB, dest, Default::default())
                        .ok()?,
                )
            }
        };
        if let Some(source) = source.cache_key() {
            self.0 = Some(CachedProfile {
                source,
                depth,
                layout,
                transform: transform.clone(),
            });
        }
        Some(transform)
    }
}

fn cicp_profile(codes: CodingIndependentCodePoints, gray: bool) -> Option<ColorProfile> {
    let color_primaries = CicpColorPrimaries::try_from(codes.color_primaries).ok()?;
    let transfer_characteristics =
        TransferCharacteristics::try_from(codes.transfer_function).ok()?;
    if color_primaries == CicpColorPrimaries::Xyz {
        return None;
    }
    let mut profile = ColorProfile::new_from_cicp(CicpProfile {
        color_primaries,
        transfer_characteristics,
        matrix_coefficients: MatrixCoefficients::Identity,
        full_range: codes.is_video_full_range_image,
    });
    profile.cicp.as_ref()?;
    if gray {
        profile.color_space = DataColorSpace::Gray;
        profile.gray_trc = profile.red_trc.take();
    }
    Some(profile)
}

fn colorimetry_profile(
    chromaticities: SourceChromaticities,
    gamma: ScaledFloat,
    gray: bool,
) -> Option<ColorProfile> {
    let gamma = gamma.into_value();
    if gamma == 0.0 {
        return None;
    }
    let white = chromaticity(chromaticities.white)?;
    if white.y == 0.0 {
        return None;
    }
    let white = XyY::new(white.x, white.y, 1.0);
    let curve = ToneReprCurve::Parametric(vec![gamma.recip()]);
    if gray {
        let mut profile = ColorProfile::new_gray_with_gamma(1.0);
        profile.gray_trc = Some(curve);
        return Some(profile);
    }

    let mut profile = ColorProfile::new_srgb();
    profile.update_rgb_colorimetry_triplet(
        white,
        chromaticity(chromaticities.red)?,
        chromaticity(chromaticities.green)?,
        chromaticity(chromaticities.blue)?,
    );
    let determinant = profile.rgb_to_xyz_matrix().determinant()?;
    if !determinant.is_finite() || determinant.abs() < 1e-12 {
        return None;
    }
    profile.media_white_point = Some(white.to_xyzd());
    profile.red_trc = Some(curve.clone());
    profile.green_trc = Some(curve.clone());
    profile.blue_trc = Some(curve);
    Some(profile)
}

fn chromaticity((x, y): (ScaledFloat, ScaledFloat)) -> Option<Xyzd> {
    let x = x.into_scaled();
    let y = y.into_scaled();
    let z = 100_000u32.checked_sub(x.checked_add(y)?)?;
    Some(Xyzd::new(
        f64::from(x) / 100_000.0,
        f64::from(y) / 100_000.0,
        f64::from(z) / 100_000.0,
    ))
}

#[cold]
fn convert_profile_8(
    rgba: &mut [u8],
    layout: Layout,
    transform: &Transform8BitExecutor,
) -> Result<(), &'static [u8]> {
    if layout == Layout::Gray {
        let levels: [u8; 256] = std::array::from_fn(|i| i as u8);
        let mut rgb = [[0; 3]; 256];
        transform
            .transform(&levels, rgb.as_flattened_mut())
            .map_err(|_| BAD_PNG)?;
        for pixel in rgba.as_chunks_mut::<4>().0 {
            let gray = usize::from(pixel[0]);
            pixel[..3].copy_from_slice(&rgb[gray]);
        }
    } else {
        let mut converted = [0; 4096];
        for block in rgba.chunks_mut(converted.len()) {
            let output = &mut converted[..block.len()];
            transform.transform(block, output).map_err(|_| BAD_PNG)?;
            for (pixel, color) in block
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(output.as_chunks::<4>().0)
            {
                pixel[..3].copy_from_slice(&color[..3]);
            }
        }
    }
    Ok(())
}

#[inline]
fn normalize_narrow(sample: f32, scale: f32) -> f32 {
    (sample - 16.0 * scale) / (219.0 * scale)
}

#[inline]
fn srgb_byte(value: f32) -> u8 {
    (value * 255.0).round() as u8
}

struct NarrowTransform {
    linear_samples: Vec<f32>,
    matrix: [[f32; 3]; 3],
}

fn narrow_transform(
    mut profile: ColorProfile,
    srgb: &ColorProfile,
    wide: bool,
) -> Option<ProfileTransform> {
    static LINEAR_GRAY: LazyLock<ColorProfile> =
        LazyLock::new(|| ColorProfile::new_gray_with_gamma(1.0));
    let cicp = profile.cicp.as_ref()?;
    let matrix = (profile.color_space == DataColorSpace::Rgb
        && cicp.color_primaries != CicpColorPrimaries::Bt709)
        .then(|| profile.transform_matrix(srgb).to_f32().v);
    if profile.color_space == DataColorSpace::Rgb {
        profile.color_space = DataColorSpace::Gray;
        profile.gray_trc = profile.red_trc.take();
    }
    let transform = profile
        .create_transform_f32(
            Layout::Gray,
            &LINEAR_GRAY,
            Layout::Gray,
            TransformOptions {
                allow_extended_range_rgb_xyz: true,
                ..Default::default()
            },
        )
        .ok()?;
    if let Some(matrix) = matrix {
        Some(ProfileTransform::Narrow(Arc::new(NarrowTransform {
            linear_samples: narrow_table(&*transform, wide, |value| value)?,
            matrix,
        })))
    } else {
        let encode = &SRGB_BYTES;
        let table = narrow_table(&*transform, wide, |value| encode_linear(value, encode))?;
        Some(ProfileTransform::NarrowCurve(Arc::new(table)))
    }
}

fn narrow_table<T>(
    transform: &moxcms::TransformF32Executor,
    wide: bool,
    map: impl Fn(f32) -> T,
) -> Option<Vec<T>> {
    let size = if wide { 65536 } else { 256 };
    let scale = if wide { 256.0 } else { 1.0 };
    let mut table = Vec::new();
    table.try_reserve_exact(size).ok()?;
    let mut encoded = [0.0; 256];
    let mut linear = [0.0; 256];
    for start in (0..size).step_by(encoded.len()) {
        for (index, value) in encoded.iter_mut().enumerate() {
            *value = normalize_narrow((start + index) as f32, scale);
        }
        transform.transform(&encoded, &mut linear).ok()?;
        table.extend(linear.into_iter().map(&map));
    }
    Some(table)
}

impl NarrowTransform {
    #[inline]
    fn convert(&self, samples: [usize; 3], encode: &[u8; SRGB_STEPS + 1]) -> [u8; 3] {
        let [r, g, b] = samples.map(|sample| self.linear_samples[sample]);
        self.matrix
            .map(|[a, b_coeff, c]| encode_linear(a * r + b_coeff * g + c * b, encode))
    }
}

#[inline]
fn encode_linear(value: f32, encode: &[u8; SRGB_STEPS + 1]) -> u8 {
    let level = (value * SRGB_STEPS as f32 + 0.5) as usize;
    encode[level.min(SRGB_STEPS)]
}

fn map_rgb_bytes(rgba: &mut [u8], table: &[u8; 256]) {
    for [r, g, b, _] in rgba.as_chunks_mut::<4>().0 {
        *r = table[usize::from(*r)];
        *g = table[usize::from(*g)];
        *b = table[usize::from(*b)];
    }
}

static SRGB_BYTES: LazyLock<[u8; SRGB_STEPS + 1]> = LazyLock::new(|| {
    std::array::from_fn(|index| {
        let value = index as f32 / SRGB_STEPS as f32;
        srgb_byte(if value <= 0.003_130_8 {
            12.92 * value
        } else {
            1.055 * value.powf(1.0 / 2.4) - 0.055
        })
    })
});

#[cold]
fn convert_rgb_8(rgba: &mut [u8], convert: impl Fn([usize; 3]) -> [u8; 3]) {
    for pixel in rgba.as_chunks_mut::<4>().0 {
        let [r, g, b, _] = pixel.map(usize::from);
        pixel[..3].copy_from_slice(&convert([r, g, b]));
    }
}

#[cold]
fn decode_with_rgb_transform_16(
    reader: &mut Reader<Cursor<&[u8]>>,
    rgba: &mut [u8],
    convert: impl Fn([usize; 3]) -> [u8; 3],
) -> Result<(), &'static [u8]> {
    let samples = |rgb: [u16; 3], alpha| {
        let [r, g, b] = convert(rgb.map(usize::from));
        [r, g, b, alpha]
    };
    match reader.output_color_type().0 {
        ColorType::Grayscale => decode_png_16_rows(reader, rgba, |gray| {
            samples([u16::from_be_bytes(gray); 3], 255)
        }),
        ColorType::GrayscaleAlpha => decode_png_16_rows(reader, rgba, |[g, lo, alpha, _]| {
            samples([u16::from_be_bytes([g, lo]); 3], alpha)
        }),
        ColorType::Rgb => decode_png_16_rows(reader, rgba, |[r, rl, g, gl, b, bl]| {
            samples([[r, rl], [g, gl], [b, bl]].map(u16::from_be_bytes), 255)
        }),
        ColorType::Rgba => decode_png_16_rows(reader, rgba, |[r, rl, g, gl, b, bl, alpha, _]| {
            samples([[r, rl], [g, gl], [b, bl]].map(u16::from_be_bytes), alpha)
        }),
        ColorType::Indexed => Err(BAD_PNG),
    }
}

#[cold]
fn decode_png_profile_16(
    reader: &mut Reader<Cursor<&[u8]>>,
    rgba: &mut [u8],
    transform: &Transform16BitExecutor,
) -> Result<(), &'static [u8]> {
    match reader.output_color_type().0 {
        ColorType::Grayscale => convert_profile_rows_16::<2, 1>(reader, rgba, transform),
        ColorType::GrayscaleAlpha => convert_profile_rows_16::<4, 1>(reader, rgba, transform),
        ColorType::Rgb => convert_profile_rows_16::<6, 3>(reader, rgba, transform),
        ColorType::Rgba => convert_profile_rows_16::<8, 3>(reader, rgba, transform),
        ColorType::Indexed => Err(BAD_PNG),
    }
}

fn convert_profile_rows_16<const N: usize, const CHANNELS: usize>(
    reader: &mut Reader<Cursor<&[u8]>>,
    rgba: &mut [u8],
    transform: &Transform16BitExecutor,
) -> Result<(), &'static [u8]> {
    const BATCH_PIXELS: usize = 256;
    let mut samples = [0; BATCH_PIXELS * 3];
    let mut converted = [0; BATCH_PIXELS * 3];
    decode_png_rows(reader, rgba, N, |input, output| {
        for (block, rgba) in input
            .chunks(BATCH_PIXELS * N)
            .zip(output.chunks_mut(BATCH_PIXELS * 4))
        {
            let pixels = block.as_chunks::<N>().0;
            let count = pixels.len();
            let source = &mut samples[..count * CHANNELS];
            for (pixel, samples) in pixels.iter().zip(source.as_chunks_mut::<CHANNELS>().0) {
                for (sample, &value) in samples.iter_mut().zip(pixel.as_chunks::<2>().0) {
                    *sample = u16::from_be_bytes(value);
                }
            }
            let rgb = &mut converted[..count * 3];
            transform.transform(source, rgb).map_err(|_| BAD_PNG)?;
            for ((out, &[r, g, b]), pixel) in rgba
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(rgb.as_chunks::<3>().0)
                .zip(pixels)
            {
                let alpha = pixel.get(CHANNELS * 2).copied().unwrap_or(255);
                *out = [(r >> 8) as u8, (g >> 8) as u8, (b >> 8) as u8, alpha];
            }
        }
        Ok(())
    })
}

fn decode_png_8(
    reader: &mut Reader<Cursor<&[u8]>>,
    rgba: &mut [u8],
    curve: Option<&[u8; 256]>,
) -> Result<(), &'static [u8]> {
    let size = reader
        .output_buffer_size()
        .filter(|&size| size <= rgba.len())
        .ok_or(QUOTA)?;
    let input_start = rgba.len() - size;
    let output = reader
        .next_frame(&mut rgba[input_start..])
        .map_err(png_error)?;
    if output.bit_depth != BitDepth::Eight {
        return Err(BAD_PNG);
    }
    if let Some(curve) = curve {
        expand_png_8(rgba, output.color_type, |sample| curve[usize::from(sample)])?;
        if output.color_type == ColorType::Rgba {
            map_rgb_bytes(rgba, curve);
        }
    } else {
        expand_png_8(rgba, output.color_type, |sample| sample)?;
    }
    Ok(())
}

fn expand_png_8(
    rgba: &mut [u8],
    color: ColorType,
    map: impl Fn(u8) -> u8,
) -> Result<(), &'static [u8]> {
    match color {
        ColorType::Grayscale => expand_png_pixels(rgba, |[gray]| {
            let gray = map(gray);
            [gray, gray, gray, 255]
        }),
        ColorType::GrayscaleAlpha => expand_png_pixels(rgba, |[gray, alpha]| {
            let gray = map(gray);
            [gray, gray, gray, alpha]
        }),
        ColorType::Rgb => expand_png_pixels(rgba, |[r, g, b]| [map(r), map(g), map(b), 255]),
        ColorType::Rgba => {}
        ColorType::Indexed => return Err(BAD_PNG),
    }
    Ok(())
}

fn expand_png_pixels<const N: usize>(rgba: &mut [u8], expand: impl Fn([u8; N]) -> [u8; 4]) {
    // Packed decoded pixels occupy the tail. Each output pixel ends at or before the next input.
    let count = rgba.len() / 4;
    let start = count * (4 - N);
    for index in 0..count {
        let input = start + index * N;
        let pixel = rgba[input..][..N].try_into().expect("A full input pixel");
        rgba[index * 4..][..4].copy_from_slice(&expand(pixel));
    }
}

fn gamma_sample_shift(info: &png::Info<'_>) -> u32 {
    let color_channels = if matches!(info.color_type, ColorType::Rgb | ColorType::Rgba) {
        3
    } else {
        1
    };
    let significant = info
        .sbit
        .as_ref()
        .and_then(|bits| bits.get(..color_channels))
        .and_then(|bits| bits.iter().copied().max())
        .filter(|&bits| bits != 0)
        .unwrap_or(16);
    (16u32.saturating_sub(u32::from(significant))).clamp(GAMMA_SAMPLE_SHIFT, 8)
}

fn gamma_table_16(gamma: f64, shift: u32) -> [u8; GAMMA_SAMPLE_COUNT] {
    // Find the source threshold for each output level instead of exponentiating each sample.
    let mut table = [255; GAMMA_SAMPLE_COUNT];
    let mut start = 0;
    let max = (1u32 << (16 - shift)) - 1;
    for level in 0..255u32 {
        let midpoint = f64::from(level * 257 + 128) / 65535.0;
        let sample = (65535.0 * midpoint.powf(gamma)).round() as u32;
        let end = ((sample * max + 32768) / 65535 + 1) as usize;
        table[start..end].fill(level as u8);
        start = end;
    }
    table
}

fn decode_png_16(
    reader: &mut Reader<Cursor<&[u8]>>,
    rgba: &mut [u8],
    color: impl Fn([u8; 2]) -> u8,
) -> Result<(), &'static [u8]> {
    match reader.output_color_type().0 {
        ColorType::Grayscale => decode_png_16_rows(reader, rgba, |gray| {
            let value = color(gray);
            [value, value, value, 255]
        }),
        ColorType::GrayscaleAlpha => decode_png_16_rows(reader, rgba, |[g, lo, alpha, _]| {
            let value = color([g, lo]);
            [value, value, value, alpha]
        }),
        ColorType::Rgb => decode_png_16_rows(reader, rgba, |[r, rl, g, gl, b, bl]| {
            [color([r, rl]), color([g, gl]), color([b, bl]), 255]
        }),
        ColorType::Rgba => decode_png_16_rows(reader, rgba, |[r, rl, g, gl, b, bl, alpha, _]| {
            [color([r, rl]), color([g, gl]), color([b, bl]), alpha]
        }),
        ColorType::Indexed => Err(BAD_PNG),
    }
}

fn decode_png_16_rows<const N: usize>(
    reader: &mut Reader<Cursor<&[u8]>>,
    rgba: &mut [u8],
    expand: impl Fn([u8; N]) -> [u8; 4],
) -> Result<(), &'static [u8]> {
    decode_png_rows(reader, rgba, N, |input, output| {
        for (&source, target) in input
            .as_chunks::<N>()
            .0
            .iter()
            .zip(output.as_chunks_mut::<4>().0)
        {
            *target = expand(source);
        }
        Ok(())
    })
}

fn decode_png_rows(
    reader: &mut Reader<Cursor<&[u8]>>,
    rgba: &mut [u8],
    pixel_bytes: usize,
    mut convert_row: impl FnMut(&[u8], &mut [u8]) -> Result<(), &'static [u8]>,
) -> Result<(), &'static [u8]> {
    let stride = reader.info().width as usize * 4;
    let mut pass_row = if reader.info().interlaced {
        zeroed(stride, stride)?
    } else {
        Vec::new()
    };
    let mut offset = 0;
    while let Some(row) = reader.next_interlaced_row().map_err(png_error)? {
        let input = row.data();
        match row.interlace() {
            InterlaceInfo::Adam7(info) => {
                let converted = &mut pass_row[..input.len() / pixel_bytes * 4];
                convert_row(input, converted)?;
                png::expand_interlaced_row(rgba, stride, converted, info, 32);
            }
            InterlaceInfo::Null(_) => {
                let output = rgba.get_mut(offset..offset + stride).ok_or(BAD_PNG)?;
                convert_row(input, output)?;
                offset += stride;
            }
        }
    }
    Ok(())
}

fn png_error(error: DecodingError) -> &'static [u8] {
    match error {
        DecodingError::LimitsExceeded => QUOTA,
        _ => BAD_PNG,
    }
}
