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

impl Format {
    fn output_capacity(&self, decoded: usize) -> usize {
        match self {
            Self::Raw { channels: 3, .. } => decoded / 3 * 4,
            _ => decoded,
        }
    }
}

enum Compression {
    None,
    Zlib,
    Streaming(Box<ZlibTransfer>),
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
    compression: Compression,
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
            compression: if compressed {
                Compression::Zlib
            } else {
                Compression::None
            },
            wire_limit,
            bytes: Vec::new(),
        })
    }

    pub(crate) fn append_payload(&mut self, command: &Command<'_>) -> Result<(), &'static [u8]> {
        let encoded = command.encoded_payload.len();
        if encoded > MAX_DIRECT_PAYLOAD_BYTES {
            return Err(INVALID);
        }
        if matches!(self.compression, Compression::Zlib)
            && command.controls.get(b'm') == Some(Value::Unsigned(1))
        {
            self.compression = Compression::Streaming(Box::new(ZlibTransfer::new()));
        }
        if let Compression::Streaming(stream) = &mut self.compression {
            let size = self
                .expected
                .expect("Direct zlib transfers have a decoded size");
            return stream.append(
                command,
                self.wire_limit,
                size,
                self.format.output_capacity(size),
                &mut self.bytes,
            );
        }
        let compressed = matches!(self.compression, Compression::Zlib);
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
            // Leave room for in-place RGBA expansion when the RGB buffer reaches its limit.
            let capacity = if capacity == self.wire_limit && !compressed {
                self.format.output_capacity(capacity)
            } else {
                capacity
            };
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
                ) && (compressed || self.expected.is_none())
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
            matches!(self.compression, Compression::None) && self.expected.is_some(),
        )?;
        Ok(())
    }

    pub(crate) fn finish(self, max_bytes: usize) -> Result<Pixels, &'static [u8]> {
        let bytes = match self.compression {
            Compression::Zlib => match self.expected {
                Some(size) => inflate(&self.bytes, size)?,
                None => inflate_limited(&self.bytes, self.wire_limit)?,
            },
            Compression::Streaming(stream) if stream.ended => self.bytes,
            Compression::Streaming(_) => return Err(INVALID),
            Compression::None => {
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
            }
        };
        match self.format {
            Format::Raw {
                dimensions,
                channels,
            } => {
                let (format, bytes) = match channels {
                    1 => (ImageFormat::Indexed, bytes),
                    2 => (ImageFormat::IndexedAlpha, bytes),
                    3 => (ImageFormat::Rgba, expand_rgb(bytes)?),
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

struct ZlibTransfer {
    decoder: Decompress,
    input: Vec<u8>,
    ended: bool,
}

impl ZlibTransfer {
    fn new() -> Self {
        Self {
            decoder: Decompress::new(true),
            input: Vec::new(),
            ended: false,
        }
    }

    fn append(
        &mut self,
        command: &Command<'_>,
        wire_limit: usize,
        size: usize,
        capacity: usize,
        output: &mut Vec<u8>,
    ) -> Result<(), &'static [u8]> {
        let remaining = wire_limit - self.decoder.total_in() as usize;
        let estimated = base64::decoded_len_estimate(command.encoded_payload.len()).min(remaining);
        self.input
            .try_reserve_exact(estimated.saturating_sub(self.input.len()))
            .map_err(|_| QUOTA)?;
        self.input.resize(estimated, 0);
        let length = command
            .decode_payload(&mut self.input)
            .map_err(|error| match error {
                PayloadError::Base64(base64::DecodeSliceError::OutputSliceTooSmall) => QUOTA,
                _ => INVALID,
            })?;
        let mut input = &self.input[..length];
        if self.ended {
            return if input.is_empty() {
                Ok(())
            } else {
                Err(INVALID)
            };
        }
        let more = command.controls.get(b'm') == Some(Value::Unsigned(1));
        while !input.is_empty() || !more {
            let read = self.decoder.total_in();
            let written = self.decoder.total_out() as usize;
            let status = if written == size {
                // Leave room to detect excess output while consuming the checksum.
                self.decoder
                    .decompress(input, &mut [0], FlushDecompress::None)
            } else {
                if written == output.len() {
                    let needed = written.saturating_mul(2).max(8192).min(size);
                    let reserve = if needed == size { capacity } else { needed };
                    output
                        .try_reserve_exact(reserve - written)
                        .map_err(|_| QUOTA)?;
                    output.resize(needed, 0);
                }
                self.decoder
                    .decompress(input, &mut output[written..], FlushDecompress::None)
            }
            .map_err(|_| INVALID)?;
            let consumed = (self.decoder.total_in() - read) as usize;
            input = &input[consumed..];
            let total = self.decoder.total_out() as usize;
            if total > size {
                return Err(INVALID);
            }
            if status == Status::StreamEnd {
                if !input.is_empty() || total != size {
                    return Err(INVALID);
                }
                self.ended = true;
                return Ok(());
            }
            if consumed == 0 && total == written {
                return Err(INVALID);
            }
        }
        Ok(())
    }
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

fn expand_rgb(mut rgb: Vec<u8>) -> Result<Vec<u8>, &'static [u8]> {
    let size = (rgb.len() / 3).checked_mul(4).ok_or(QUOTA)?;
    if rgb.capacity() >= size {
        rgb.resize(size, 0);
        expand_rgb_in_place(&mut rgb);
        return Ok(rgb);
    }
    let mut rgba = zeroed(size)?;
    expand_rgb_pixels(&rgb, &mut rgba);
    Ok(rgba)
}

fn expand_rgb_pixels(rgb: &[u8], rgba: &mut [u8]) {
    let pixels = rgba.as_chunks_mut::<4>().0;
    for (out, &[r, g, b]) in pixels.iter_mut().zip(rgb.as_chunks::<3>().0) {
        *out = [r, g, b, 255];
    }
}

fn expand_rgb_in_place(rgba: &mut [u8]) {
    let mut end = rgba.len() / 4;
    // Work from the end, using disjoint source and destination slices where possible.
    while end > 3 {
        let start = (3 * end).div_ceil(4);
        let (input, output) = rgba.split_at_mut(4 * start);
        expand_rgb_pixels(&input[3 * start..3 * end], &mut output[..4 * (end - start)]);
        end = start;
    }
    for index in (0..end).rev() {
        let [r, g, b] = rgba.as_chunks::<3>().0[index];
        rgba.as_chunks_mut::<4>().0[index] = [r, g, b, 255];
    }
}
