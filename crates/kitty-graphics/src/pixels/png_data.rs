use std::io::Cursor;

use png::{BitDepth, ColorType, DecodingError, InterlaceInfo, Reader, Transformations};

use super::{ENCODED_OVERHEAD, INVALID, ImageFormat, Pixels, QUOTA, pixel_size, zeroed};

const DISPLAY_GAMMA: f64 = 2.2;
const GAMMA_THRESHOLD: f64 = 0.05;
const GAMMA_SAMPLE_SHIFT: u32 = 5;
const GAMMA_SAMPLE_COUNT: usize = 1 << (16 - GAMMA_SAMPLE_SHIFT);

pub(super) fn decode_png(bytes: &[u8], max_bytes: usize) -> Result<Pixels, &'static [u8]> {
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(bytes),
        png::Limits {
            bytes: max_bytes.saturating_add(ENCODED_OVERHEAD),
        },
    );
    decoder.set_transformations(Transformations::EXPAND);
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_ignore_text_chunk(true);
    let dimensions = decoder.read_header_info().map_err(png_error)?.size();
    let rgba_size = pixel_size(dimensions, 4, max_bytes)?;
    let mut reader = decoder.read_info().map_err(png_error)?;
    if reader
        .info()
        .frame_control()
        .is_some_and(|frame| (frame.width, frame.height) != dimensions)
    {
        return Err(INVALID);
    }
    let gamma = reader
        .info()
        .gamma()
        .map(|gamma| f64::from(gamma.into_scaled()) * (DISPLAY_GAMMA / 100_000.0))
        .filter(|&gamma| gamma > 0.0 && (gamma - 1.0).abs() > GAMMA_THRESHOLD);
    let mut rgba = zeroed(rgba_size, rgba_size)?;
    if reader.output_color_type().1 == BitDepth::Sixteen {
        if let Some(gamma) = gamma {
            let shift = gamma_sample_shift(reader.info());
            let table = gamma_table_16(gamma, shift);
            decode_png_16(&mut reader, &mut rgba, |sample| {
                table[usize::from(u16::from_be_bytes(sample) >> shift)]
            })?;
        } else {
            decode_png_16(&mut reader, &mut rgba, |[high, _]| high)?;
        }
    } else {
        decode_png_8(&mut reader, &mut rgba)?;
        if let Some(exponent) = gamma
            .map(f64::recip)
            .filter(|&exponent| (exponent - 1.0).abs() > GAMMA_THRESHOLD)
        {
            let table: [u8; 256] = std::array::from_fn(|color| {
                (255.0 * (color as f64 / 255.0).powf(exponent)).round() as u8
            });
            for [red, green, blue, _] in rgba.as_chunks_mut::<4>().0 {
                *red = table[usize::from(*red)];
                *green = table[usize::from(*green)];
                *blue = table[usize::from(*blue)];
            }
        }
    }
    reader.finish().map_err(png_error)?;
    Ok(Pixels {
        dimensions,
        format: ImageFormat::Rgba,
        bytes: rgba,
    })
}

fn decode_png_8(reader: &mut Reader<Cursor<&[u8]>>, rgba: &mut [u8]) -> Result<(), &'static [u8]> {
    let size = reader
        .output_buffer_size()
        .filter(|&size| size <= rgba.len())
        .ok_or(QUOTA)?;
    let input_start = rgba.len() - size;
    let output = reader
        .next_frame(&mut rgba[input_start..])
        .map_err(png_error)?;
    if output.bit_depth != BitDepth::Eight {
        return Err(INVALID);
    }
    match output.color_type {
        ColorType::Grayscale => expand_png_pixels(rgba, |[v]| [v, v, v, 255]),
        ColorType::GrayscaleAlpha => expand_png_pixels(rgba, |[v, a]| [v, v, v, a]),
        ColorType::Rgb => expand_png_pixels(rgba, |[r, g, b]| [r, g, b, 255]),
        ColorType::Rgba => {}
        ColorType::Indexed => return Err(INVALID),
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
        ColorType::Indexed => Err(INVALID),
    }
}

fn decode_png_16_rows<const N: usize>(
    reader: &mut Reader<Cursor<&[u8]>>,
    rgba: &mut [u8],
    expand: impl Fn([u8; N]) -> [u8; 4],
) -> Result<(), &'static [u8]> {
    let stride = reader.info().width as usize * 4;
    let mut pass_row = if reader.info().interlaced {
        zeroed(stride, stride)?
    } else {
        Vec::new()
    };
    let mut offset = 0;
    let convert_row = |input: &[u8], output: &mut [u8]| {
        for (&source, target) in input
            .as_chunks::<N>()
            .0
            .iter()
            .zip(output.as_chunks_mut::<4>().0)
        {
            *target = expand(source);
        }
    };
    while let Some(row) = reader.next_interlaced_row().map_err(png_error)? {
        let input = row.data();
        match row.interlace() {
            InterlaceInfo::Adam7(info) => {
                let converted = &mut pass_row[..input.len() / N * 4];
                convert_row(input, converted);
                png::expand_interlaced_row(rgba, stride, converted, info, 32);
            }
            InterlaceInfo::Null(_) => {
                let output = rgba.get_mut(offset..offset + stride).ok_or(INVALID)?;
                convert_row(input, output);
                offset += stride;
            }
        }
    }
    Ok(())
}

fn png_error(error: DecodingError) -> &'static [u8] {
    match error {
        DecodingError::LimitsExceeded => QUOTA,
        _ => INVALID,
    }
}
