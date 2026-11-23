#![forbid(unsafe_code)]

use base64::{
    DecodeSliceError, Engine,
    alphabet::STANDARD,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};

mod animation;
mod delete;
mod features;
mod images;
mod pixels;
mod placeholder;
mod placement;
mod source;

pub use animation::PixelRevision;
pub use delete::{Delete, DeleteTarget};
pub use features::{FeatureReply, MAX_FEATURE_REPLY_BYTES};
pub use images::{
    Image, ImageKey, ImageStore, PixelBounds, PixelUpdate, PixelUpdatePlan, PixelUpdates, Reply,
};
pub use pixels::ImageFormat;
pub use placeholder::{PLACEHOLDER, Placeholder};
pub use placement::{
    EvictionRanking, ImageChange, Placement, PlacementError, PlacementPosition, RelativePlacement,
};

const KEY_COUNT: usize = 52;
const MAX_CONTROL_BYTES: usize = KEY_COUNT * 14;
pub const MAX_DIRECT_PAYLOAD_BYTES: usize = 128 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const INLINE_ENCODED_BYTES: usize = MAX_PATH_BYTES.div_ceil(3) * 4;
const INLINE_DECODED_BYTES: usize = INLINE_ENCODED_BYTES / 4 * 3;
pub const MAX_DECODED_PAYLOAD_BYTES: usize = MAX_DIRECT_PAYLOAD_BYTES / 4 * 3;
const PAYLOAD_ENGINE: GeneralPurpose = GeneralPurpose::new(
    &STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    Unsigned(u32),
    Signed(i32),
    Letter(u8),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Controls {
    values: [Option<Value>; KEY_COUNT],
    present: u64,
}

impl Controls {
    pub fn get(&self, key: u8) -> Option<Value> {
        self.values[key_index(key)?]
    }

    fn only(&self, allowed: u64) -> bool {
        self.present & !allowed == 0
    }

    fn is_feature_query(&self) -> bool {
        self.present & const { control_keys(b"e") } != 0
            && self.get(b'a') == Some(Value::Letter(b'q'))
    }

    fn has_conflicting_image_selectors(&self) -> bool {
        const SELECTORS: u64 = control_keys(b"iI");
        self.present & SELECTORS == SELECTORS
    }

    fn quiet(&self) -> Result<Option<u32>, DecodeError> {
        match self.get(b'q') {
            None => Ok(None),
            Some(Value::Unsigned(value @ 0..=2)) => Ok(Some(value)),
            _ => Err(DecodeError::InvalidControl),
        }
    }

    fn parse(header: &[u8]) -> Result<Self, DecodeError> {
        let mut values = [None; KEY_COUNT];
        let mut present = 0;
        if !header.is_empty() {
            for pair in header.split(|&byte| byte == b',') {
                let [key, b'=', value @ ..] = pair else {
                    return Err(DecodeError::InvalidControl);
                };
                let index = key_index(*key).ok_or(DecodeError::InvalidControl)?;
                let bit = 1 << index;
                if present & bit != 0 {
                    return Err(DecodeError::DuplicateKey(*key));
                }
                values[index] = Some(parse_value(value).ok_or(DecodeError::InvalidControl)?);
                present |= bit;
            }
            // The zero selectors are unset, but they still count for duplicate detection.
            for key in *b"iI" {
                let index = key_index(key).expect("Image selector has a control index");
                if values[index] == Some(Value::Unsigned(0)) {
                    values[index] = None;
                    present &= !(1 << index);
                }
            }
        }
        Ok(Self { values, present })
    }
}

const fn key_index(key: u8) -> Option<usize> {
    match key {
        b'A'..=b'Z' => Some((key - b'A') as usize),
        b'a'..=b'z' => Some(26 + (key - b'a') as usize),
        _ => None,
    }
}

const fn control_keys(keys: &[u8]) -> u64 {
    let mut allowed = 0;
    let mut offset = 0;
    while offset < keys.len() {
        let Some(index) = key_index(keys[offset]) else {
            panic!("Invalid control key")
        };
        allowed |= 1 << index;
        offset += 1;
    }
    allowed
}

fn parse_value(raw: &[u8]) -> Option<Value> {
    match raw {
        [letter] if letter.is_ascii_alphabetic() => Some(Value::Letter(*letter)),
        [b'-', digits @ ..] if digits.iter().all(u8::is_ascii_digit) => {
            str::from_utf8(raw).ok()?.parse().ok().map(Value::Signed)
        }
        digits if digits.iter().all(u8::is_ascii_digit) => {
            str::from_utf8(raw).ok()?.parse().ok().map(Value::Unsigned)
        }
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    ControlTooLong,
    InvalidControl,
    DuplicateKey(u8),
}

#[derive(Debug, PartialEq, Eq)]
pub enum PayloadError {
    TooLong,
    Allocation,
    InvalidChunk,
    Base64(DecodeSliceError),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Command<'a> {
    pub controls: Controls,
    encoded_payload: Result<&'a [u8], PayloadDiscard>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PayloadDiscard {
    TooLong,
    Allocation,
}

impl Command<'_> {
    /// Returns the encoded transfer block, which is empty for feature queries.
    pub fn encoded_payload(&self) -> Result<&[u8], PayloadError> {
        if self.controls.is_feature_query() {
            return Ok(&[]);
        }
        self.encoded_transfer()
    }

    fn encoded_transfer(&self) -> Result<&[u8], PayloadError> {
        self.encoded_payload.map_err(|discard| match discard {
            PayloadDiscard::TooLong => PayloadError::TooLong,
            PayloadDiscard::Allocation => PayloadError::Allocation,
        })
    }

    fn has_payload(&self) -> bool {
        !matches!(self.encoded_payload, Ok([]))
    }

    /// Decodes this command's payload block into caller-provided storage.
    pub fn decode_payload(&self, output: &mut [u8]) -> Result<usize, PayloadError> {
        if self.controls.is_feature_query() {
            return Ok(0);
        }
        self.decode_transfer(output)
    }

    fn decode_transfer(&self, output: &mut [u8]) -> Result<usize, PayloadError> {
        let payload = self.encoded_transfer()?;
        match self.controls.get(b'm') {
            None | Some(Value::Unsigned(0)) => {}
            Some(Value::Unsigned(1))
                if payload.len().is_multiple_of(4) && !payload.ends_with(b"=") => {}
            _ => return Err(PayloadError::InvalidChunk),
        }
        PAYLOAD_ENGINE
            .decode_slice(payload, output)
            .map_err(PayloadError::Base64)
    }
}

enum State {
    Idle,
    Selector,
    Control,
    Payload,
    LargePayload,
    DiscardedPayload(PayloadDiscard),
    Ignore,
    Failed(DecodeError),
}

/// Decodes a Kitty APC from the payload spans of a terminal parser.
pub struct Decoder {
    state: State,
    control: [u8; MAX_CONTROL_BYTES],
    control_len: usize,
    payload: [u8; INLINE_ENCODED_BYTES],
    payload_len: usize,
    large_payload: Vec<u8>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            state: State::Idle,
            control: [0; MAX_CONTROL_BYTES],
            control_len: 0,
            payload: [0; INLINE_ENCODED_BYTES],
            payload_len: 0,
            large_payload: Vec::new(),
        }
    }
}

impl Decoder {
    pub fn start(&mut self) {
        self.state = State::Selector;
        self.control_len = 0;
        self.payload_len = 0;
        self.large_payload.clear();
    }

    /// Cancels the APC and reports whether it may have carried an image chunk.
    pub fn cancel(&mut self) -> bool {
        let state = core::mem::replace(&mut self.state, State::Idle);
        !matches!(state, State::Idle | State::Selector | State::Ignore)
            && !Controls::parse(&self.control[..self.control_len])
                .is_ok_and(|controls| controls.is_feature_query())
    }

    pub fn push(&mut self, mut bytes: &[u8]) {
        if matches!(self.state, State::Selector) {
            let Some((&selector, rest)) = bytes.split_first() else {
                return;
            };
            self.state = if selector == b'G' {
                State::Control
            } else {
                State::Ignore
            };
            bytes = rest;
        }
        if matches!(self.state, State::Control) {
            let end = bytes
                .iter()
                .position(|&byte| byte == b';')
                .unwrap_or(bytes.len());
            if !append(&mut self.control, &mut self.control_len, &bytes[..end]) {
                self.state = State::Failed(DecodeError::ControlTooLong);
                return;
            }
            if end == bytes.len() {
                return;
            }
            self.state = State::Payload;
            bytes = &bytes[end + 1..];
        }
        match self.state {
            State::Payload if !append(&mut self.payload, &mut self.payload_len, bytes) => {
                self.push_large_payload(bytes);
            }
            State::LargePayload => self.push_large_payload(bytes),
            _ => {}
        }
    }

    #[cold]
    fn push_large_payload(&mut self, bytes: &[u8]) {
        let needed = self.payload_len.saturating_add(bytes.len());
        if needed > MAX_DIRECT_PAYLOAD_BYTES {
            self.state = State::DiscardedPayload(PayloadDiscard::TooLong);
            return;
        }
        let first = matches!(self.state, State::Payload);
        if first {
            let controls = match Controls::parse(&self.control[..self.control_len]) {
                Ok(controls) => controls,
                Err(error) => {
                    self.state = State::Failed(error);
                    return;
                }
            };
            if controls.is_feature_query()
                || matches!(controls.get(b't'), Some(Value::Letter(b'f' | b't' | b's')))
            {
                self.state = State::DiscardedPayload(PayloadDiscard::TooLong);
                return;
            }
        }
        if needed > self.large_payload.capacity()
            && self
                .large_payload
                .try_reserve_exact(needed.next_power_of_two() - self.large_payload.len())
                .is_err()
        {
            self.state = State::DiscardedPayload(PayloadDiscard::Allocation);
            return;
        }
        if first {
            self.large_payload
                .extend_from_slice(&self.payload[..self.payload_len]);
        }
        self.large_payload.extend_from_slice(bytes);
        self.payload_len = needed;
        self.state = State::LargePayload;
    }

    pub fn finish(&mut self) -> Option<Result<Command<'_>, DecodeError>> {
        let state = core::mem::replace(&mut self.state, State::Idle);
        match state {
            State::Control | State::Payload | State::LargePayload | State::DiscardedPayload(_) => {
                Some(
                    Controls::parse(&self.control[..self.control_len]).map(|controls| {
                        let encoded_payload = match state {
                            State::LargePayload => Ok(self.large_payload.as_slice()),
                            State::DiscardedPayload(reason) => Err(reason),
                            _ => Ok(&self.payload[..self.payload_len]),
                        };
                        Command {
                            controls,
                            encoded_payload,
                        }
                    }),
                )
            }
            State::Failed(error) => Some(Err(error)),
            _ => None,
        }
    }
}

fn append(buffer: &mut [u8], len: &mut usize, bytes: &[u8]) -> bool {
    let Some(destination) = buffer[*len..].get_mut(..bytes.len()) else {
        return false;
    };
    destination.copy_from_slice(bytes);
    *len += bytes.len();
    true
}

#[cfg(test)]
mod tests;
