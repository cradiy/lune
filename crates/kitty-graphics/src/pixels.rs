use flate2::{Decompress, FlushDecompress, Status};

use crate::source::LocalSource;
use crate::{Command, Controls, PayloadError, Value};

mod png_data;
use png_data::decode_png;

pub(crate) const INVALID: &[u8] = b"EINVAL:Invalid image data";
pub(crate) const QUOTA: &[u8] = b"ENOSPC:Image quota exceeded";
pub(crate) const UNSUPPORTED: &[u8] = b"ENOTSUP:Unsupported graphics command";
pub(crate) const MAX_IMAGE_DIMENSION: u32 = 10_000;
const SHORT: &[u8] = b"ENODATA:Insufficient image data";

const ENCODED_OVERHEAD: usize = 64 * 1024;
const MAX_EAGER_INFLATE_BYTES: usize = 64 * 1024;

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
        let encoded = command.encoded_payload().map_err(|_| INVALID)?.len();
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
        let output_capacity = match self.compression {
            Compression::None => self.expected.map(|size| self.format.output_capacity(size)),
            _ => None,
        };
        self.bytes = source.read(name, self.wire_limit, output_capacity)?;
        Ok(())
    }

    pub(crate) fn finish(self, max_bytes: usize) -> Result<Pixels, &'static [u8]> {
        let bytes = match self.compression {
            Compression::Zlib => match self.expected {
                Some(size) => inflate(&self.bytes, size, self.format.output_capacity(size))?,
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

fn zeroed(size: usize, capacity: usize) -> Result<Vec<u8>, &'static [u8]> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|_| QUOTA)?;
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
        let encoded = command.encoded_payload().map_err(|_| INVALID)?.len();
        let estimated = base64::decoded_len_estimate(encoded).min(remaining);
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
        let input = &self.input[..length];
        if self.ended {
            return if input.is_empty() {
                Ok(())
            } else {
                Err(INVALID)
            };
        }
        let more = command.controls.get(b'm') == Some(Value::Unsigned(1));
        self.ended = inflate_chunk(&mut self.decoder, input, size, capacity, output, more)?;
        Ok(())
    }
}

fn inflate_chunk(
    decoder: &mut Decompress,
    mut input: &[u8],
    size: usize,
    capacity: usize,
    output: &mut Vec<u8>,
    more: bool,
) -> Result<bool, &'static [u8]> {
    while !input.is_empty() || !more {
        let read = decoder.total_in();
        let written = decoder.total_out() as usize;
        let status = if written == size {
            // Leave room to detect excess output while consuming the checksum.
            decoder.decompress(input, &mut [0], FlushDecompress::None)
        } else {
            if written == output.len() {
                let needed = written.saturating_mul(2).max(8192).min(size);
                let reserve = if needed == size { capacity } else { needed };
                output
                    .try_reserve_exact(reserve - written)
                    .map_err(|_| QUOTA)?;
                output.resize(needed, 0);
            }
            decoder.decompress(input, &mut output[written..], FlushDecompress::None)
        }
        .map_err(|_| INVALID)?;
        let consumed = (decoder.total_in() - read) as usize;
        input = &input[consumed..];
        let total = decoder.total_out() as usize;
        if total > size {
            return Err(INVALID);
        }
        if status == Status::StreamEnd {
            if !input.is_empty() || total != size {
                return Err(INVALID);
            }
            return Ok(true);
        }
        if consumed == 0 && total == written {
            return Err(INVALID);
        }
    }
    Ok(false)
}

fn inflate(bytes: &[u8], size: usize, capacity: usize) -> Result<Vec<u8>, &'static [u8]> {
    // Grow large outputs as the decoder produces bytes.
    let mut inflated = if size <= MAX_EAGER_INFLATE_BYTES {
        zeroed(size, capacity)?
    } else {
        Vec::new()
    };
    inflate_chunk(
        &mut Decompress::new(true),
        bytes,
        size,
        capacity,
        &mut inflated,
        false,
    )?;
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
        let needed = pixels.len() + written;
        if needed > pixels.capacity() {
            let capacity = pixels.capacity().saturating_mul(2).max(needed).min(limit);
            pixels
                .try_reserve_exact(capacity - pixels.len())
                .map_err(|_| QUOTA)?;
        }
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

fn expand_rgb_pixels(rgb: &[u8], rgba: &mut [u8]) {
    let pixels = rgba.as_chunks_mut::<4>().0;
    for (out, &[r, g, b]) in pixels.iter_mut().zip(rgb.as_chunks::<3>().0) {
        *out = [r, g, b, 255];
    }
}

fn expand_rgb(mut rgba: Vec<u8>) -> Result<Vec<u8>, &'static [u8]> {
    let mut end = rgba.len() / 3;
    let size = end.checked_mul(4).ok_or(QUOTA)?;
    rgba.try_reserve_exact(size - rgba.len())
        .map_err(|_| QUOTA)?;
    rgba.resize(size, 0);
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
    Ok(rgba)
}
