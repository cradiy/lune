use std::io::Cursor;

use flate2::{Decompress, FlushDecompress, Status};
use png::{BitDepth, ColorType, DecodingError, Transformations};

use crate::source::LocalSource;
use crate::{Command, Controls, MAX_DIRECT_PAYLOAD_BYTES, PayloadError, Value};

pub(crate) const INVALID: &[u8] = b"EINVAL:Invalid image data";
pub(crate) const QUOTA: &[u8] = b"ENOSPC:Image quota exceeded";
pub(crate) const UNSUPPORTED: &[u8] = b"ENOTSUP:Unsupported graphics command";
pub(crate) const MAX_IMAGE_DIMENSION: u32 = 10_000;
const SHORT: &[u8] = b"ENODATA:Insufficient image data";

const ENCODED_OVERHEAD: usize = 64 * 1024;

enum Format {
    Raw {
        dimensions: (u32, u32),
        channels: usize,
    },
    Png,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Rgba,
    Indexed,
    IndexedAlpha,
}

impl ImageFormat {
    pub fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Rgba => 4,
            Self::Indexed => 1,
            Self::IndexedAlpha => 2,
        }
    }
}

pub(crate) struct Pixels {
    pub(crate) dimensions: (u32, u32),
    pub(crate) format: ImageFormat,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) struct PixelTransfer {
    format: Format,
    expected: Option<usize>,
    compressed: bool,
    wire_limit: usize,
    bytes: Vec<u8>,
}

impl PixelTransfer {
    pub(crate) fn begin(
        controls: &Controls,
        max_bytes: usize,
        direct: bool,
    ) -> Result<Self, &'static [u8]> {
        let compressed = match controls.get(b'o') {
            None => false,
            Some(Value::Letter(b'z')) => true,
            _ => return Err(UNSUPPORTED),
        };
        let encoded_limit = max_bytes.saturating_add(ENCODED_OVERHEAD);
        let format = match controls.get(b'f') {
            Some(Value::Unsigned(100)) => Format::Png,
            value @ (None | Some(Value::Unsigned(8 | 16 | 24 | 32))) => {
                let channels = match value {
                    Some(Value::Unsigned(bits)) => bits as usize / 8,
                    _ => 4,
                };
                let dimensions = (required(controls, b's')?, required(controls, b'v')?);
                Format::Raw {
                    dimensions,
                    channels,
                }
            }
            _ => return Err(UNSUPPORTED),
        };
        let expected = match format {
            Format::Raw {
                dimensions,
                channels,
            } => {
                let stored_channels = if channels == 3 { 4 } else { channels };
                Some(
                    pixel_size(dimensions, stored_channels, max_bytes)? / stored_channels
                        * channels,
                )
            }
            Format::Png if compressed && direct => {
                let size = required(controls, b'S')? as usize;
                if size > encoded_limit {
                    return Err(QUOTA);
                }
                Some(size)
            }
            Format::Png => None,
        };
        let wire_limit = if compressed {
            encoded_limit
        } else {
            expected.unwrap_or(encoded_limit)
        };
        Ok(Self {
            format,
            expected,
            compressed,
            wire_limit,
            bytes: Vec::new(),
        })
    }

    pub(crate) fn append_payload(&mut self, command: &Command<'_>) -> Result<(), &'static [u8]> {
        let encoded = command.encoded_payload.len();
        if encoded > MAX_DIRECT_PAYLOAD_BYTES {
            return Err(INVALID);
        }
        let start = self.bytes.len();
        let decoded = base64::decoded_len_estimate(encoded).min(self.wire_limit - start);
        let needed = start + decoded;
        if needed > self.bytes.capacity() {
            let capacity = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(needed)
                .min(self.wire_limit);
            self.bytes
                .try_reserve_exact(capacity - start)
                .map_err(|_| QUOTA)?;
        }
        self.bytes.resize(needed, 0);
        match command.decode_payload(&mut self.bytes[start..]) {
            Ok(written) => {
                self.bytes.truncate(start + written);
                Ok(())
            }
            Err(error) => {
                self.bytes.truncate(start);
                if matches!(
                    error,
                    PayloadError::Base64(base64::DecodeSliceError::OutputSliceTooSmall)
                ) && (self.compressed || self.expected.is_none())
                {
                    return Err(QUOTA);
                }
                Err(INVALID)
            }
        }
    }

    pub(crate) fn load(&mut self, source: LocalSource, name: &[u8]) -> Result<(), &'static [u8]> {
        self.bytes = source.read(
            name,
            self.wire_limit,
            !self.compressed && self.expected.is_some(),
        )?;
        Ok(())
    }

    pub(crate) fn finish(self, max_bytes: usize) -> Result<Pixels, &'static [u8]> {
        let bytes = if self.compressed {
            if let Some(size) = self.expected {
                inflate(&self.bytes, size)?
            } else {
                inflate_limited(&self.bytes, self.wire_limit)?
            }
        } else {
            if let Some(size) = self.expected
                && self.bytes.len() != size
            {
                return Err(if self.bytes.len() < size {
                    SHORT
                } else {
                    INVALID
                });
            }
            self.bytes
        };
        match self.format {
            Format::Raw {
                dimensions,
                channels,
            } => {
                let (format, bytes) = match channels {
                    1 => (ImageFormat::Indexed, bytes),
                    2 => (ImageFormat::IndexedAlpha, bytes),
                    3 => (ImageFormat::Rgba, expand_rgb(&bytes)?),
                    _ => (ImageFormat::Rgba, bytes),
                };
                Ok(Pixels {
                    dimensions,
                    format,
                    bytes,
                })
            }
            Format::Png => decode_png(&bytes, max_bytes),
        }
    }
}

fn required(controls: &Controls, key: u8) -> Result<u32, &'static [u8]> {
    match controls.get(key) {
        Some(Value::Unsigned(value)) if value != 0 => Ok(value),
        _ => Err(INVALID),
    }
}

fn pixel_size(
    (width, height): (u32, u32),
    channels: usize,
    max_bytes: usize,
) -> Result<usize, &'static [u8]> {
    if width.max(height) > MAX_IMAGE_DIMENSION {
        return Err(INVALID);
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(channels))
        .filter(|&bytes| bytes != 0 && bytes <= max_bytes)
        .ok_or(QUOTA)
}

fn zeroed(size: usize) -> Result<Vec<u8>, &'static [u8]> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(|_| QUOTA)?;
    bytes.resize(size, 0);
    Ok(bytes)
}

fn inflate(bytes: &[u8], size: usize) -> Result<Vec<u8>, &'static [u8]> {
    let mut inflated = zeroed(size)?;
    let mut decoder = Decompress::new(true);
    let mut status = decoder
        .decompress(bytes, &mut inflated, FlushDecompress::Finish)
        .map_err(|_| INVALID)?;
    if status != Status::StreamEnd && decoder.total_out() == size as u64 {
        let remaining = &bytes[decoder.total_in() as usize..];
        status = decoder
            .decompress(remaining, &mut [0], FlushDecompress::Finish)
            .map_err(|_| INVALID)?;
    }
    if status != Status::StreamEnd
        || decoder.total_out() != size as u64
        || decoder.total_in() != bytes.len() as u64
    {
        return Err(INVALID);
    }
    Ok(inflated)
}

fn inflate_limited(bytes: &[u8], limit: usize) -> Result<Vec<u8>, &'static [u8]> {
    let mut decoder = Decompress::new(true);
    let mut block = [0; 8192];
    let mut pixels = Vec::new();
    loop {
        let input = decoder.total_in();
        let output = decoder.total_out();
        let status = decoder
            .decompress(&bytes[input as usize..], &mut block, FlushDecompress::None)
            .map_err(|_| INVALID)?;
        let written = (decoder.total_out() - output) as usize;
        if written > limit - pixels.len() {
            return Err(QUOTA);
        }
        pixels.try_reserve(written).map_err(|_| QUOTA)?;
        pixels.extend_from_slice(&block[..written]);
        if status == Status::StreamEnd {
            return if decoder.total_in() == bytes.len() as u64 {
                Ok(pixels)
            } else {
                Err(INVALID)
            };
        }
        if written == 0 && input == decoder.total_in() {
            return Err(INVALID);
        }
    }
}

fn decode_png(bytes: &[u8], max_bytes: usize) -> Result<Pixels, &'static [u8]> {
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(bytes),
        png::Limits {
            bytes: max_bytes.saturating_add(ENCODED_OVERHEAD),
        },
    );
    decoder.set_transformations(Transformations::normalize_to_color8());
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_ignore_text_chunk(true);
    let dimensions = decoder.read_header_info().map_err(png_error)?.size();
    let rgba_size = pixel_size(dimensions, 4, max_bytes)?;
    let mut reader = decoder.read_info().map_err(png_error)?;
    let size = reader
        .output_buffer_size()
        .filter(|&size| size <= rgba_size)
        .ok_or(QUOTA)?;
    let mut rgba = zeroed(rgba_size)?;
    let input_start = rgba_size - size;
    let output = reader
        .next_frame(&mut rgba[input_start..])
        .map_err(png_error)?;
    reader.finish().map_err(png_error)?;
    if (output.width, output.height) != dimensions || output.bit_depth != BitDepth::Eight {
        return Err(INVALID);
    }
    match output.color_type {
        ColorType::Grayscale => expand_png_pixels(&mut rgba, |[v]| [v, v, v, 255]),
        ColorType::GrayscaleAlpha => expand_png_pixels(&mut rgba, |[v, a]| [v, v, v, a]),
        ColorType::Rgb => expand_png_pixels(&mut rgba, |[r, g, b]| [r, g, b, 255]),
        ColorType::Rgba => {}
        ColorType::Indexed => return Err(INVALID),
    }
    Ok(Pixels {
        dimensions,
        format: ImageFormat::Rgba,
        bytes: rgba,
    })
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

fn png_error(error: DecodingError) -> &'static [u8] {
    match error {
        DecodingError::LimitsExceeded => QUOTA,
        _ => INVALID,
    }
}

fn expand_rgb(bytes: &[u8]) -> Result<Vec<u8>, &'static [u8]> {
    let size = (bytes.len() / 3).checked_mul(4).ok_or(QUOTA)?;
    let mut rgba = zeroed(size)?;
    let pixels = rgba.as_chunks_mut::<4>().0;
    for (out, &[r, g, b]) in pixels.iter_mut().zip(bytes.as_chunks::<3>().0) {
        *out = [r, g, b, 255];
    }
    Ok(rgba)
}
