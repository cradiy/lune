use std::io::Cursor;

use flate2::{Decompress, FlushDecompress, Status};
use png::{BitDepth, ColorType, DecodingError, Transformations};

use crate::source::LocalSource;
use crate::{Controls, Value};

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

    pub(crate) fn append(&mut self, bytes: &[u8]) -> Result<(), &'static [u8]> {
        if bytes.len() > self.wire_limit - self.bytes.len() {
            return Err(if self.compressed || self.expected.is_none() {
                QUOTA
            } else {
                INVALID
            });
        }
        let needed = self.bytes.len() + bytes.len();
        if needed > self.bytes.capacity() {
            let capacity = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(needed)
                .min(self.wire_limit);
            self.bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .map_err(|_| QUOTA)?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
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
                    3 => (ImageFormat::Rgba, expand(&bytes, ColorType::Rgb)?),
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
    let mut rgba = zeroed(size)?;
    let output = reader.next_frame(&mut rgba).map_err(png_error)?;
    reader.finish().map_err(png_error)?;
    if (output.width, output.height) != dimensions || output.bit_depth != BitDepth::Eight {
        return Err(INVALID);
    }
    rgba.truncate(output.buffer_size());
    if output.color_type != ColorType::Rgba {
        rgba = expand(&rgba, output.color_type)?;
    }
    Ok(Pixels {
        dimensions,
        format: ImageFormat::Rgba,
        bytes: rgba,
    })
}

fn png_error(error: DecodingError) -> &'static [u8] {
    match error {
        DecodingError::LimitsExceeded => QUOTA,
        _ => INVALID,
    }
}

fn expand(bytes: &[u8], color: ColorType) -> Result<Vec<u8>, &'static [u8]> {
    let size = (bytes.len() / color.samples())
        .checked_mul(4)
        .ok_or(QUOTA)?;
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(size).map_err(|_| QUOTA)?;
    match color {
        ColorType::Rgb => {
            for &[r, g, b] in bytes.as_chunks::<3>().0 {
                rgba.extend_from_slice(&[r, g, b, 255]);
            }
        }
        ColorType::Grayscale => {
            for &value in bytes {
                rgba.extend_from_slice(&[value, value, value, 255]);
            }
        }
        ColorType::GrayscaleAlpha => {
            for &[value, alpha] in bytes.as_chunks::<2>().0 {
                rgba.extend_from_slice(&[value, value, value, alpha]);
            }
        }
        ColorType::Rgba | ColorType::Indexed => return Err(INVALID),
    }
    Ok(rgba)
}
