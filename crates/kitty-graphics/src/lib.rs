#![forbid(unsafe_code)]

use base64::{
    DecodeSliceError, Engine,
    alphabet::STANDARD,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};

mod images;

pub use images::{DirectImages, Image, Reply};

const KEY_COUNT: usize = 52;
const MAX_CONTROL_BYTES: usize = KEY_COUNT * 14;
pub const MAX_DIRECT_PAYLOAD_BYTES: usize = 4096;
const MAX_PATH_BYTES: usize = 4096;
const MAX_ENCODED_PAYLOAD_BYTES: usize = MAX_PATH_BYTES.div_ceil(3) * 4;
pub const MAX_DECODED_PAYLOAD_BYTES: usize = MAX_ENCODED_PAYLOAD_BYTES / 4 * 3;
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
}

impl Controls {
    pub fn get(&self, key: u8) -> Option<Value> {
        self.values[key_index(key)?]
    }

    fn only(&self, keys: &[u8]) -> bool {
        self.values.iter().flatten().count()
            == keys.iter().filter(|&&key| self.get(key).is_some()).count()
    }

    fn parse(header: &[u8]) -> Result<Self, DecodeError> {
        let mut values = [None; KEY_COUNT];
        if !header.is_empty() {
            for pair in header.split(|&byte| byte == b',') {
                let [key, b'=', value @ ..] = pair else {
                    return Err(DecodeError::InvalidControl);
                };
                let index = key_index(*key).ok_or(DecodeError::InvalidControl)?;
                if values[index].is_some() {
                    return Err(DecodeError::DuplicateKey(*key));
                }
                values[index] = Some(parse_value(value).ok_or(DecodeError::InvalidControl)?);
            }
        }
        Ok(Self { values })
    }
}

fn key_index(key: u8) -> Option<usize> {
    match key {
        b'A'..=b'Z' => Some(usize::from(key - b'A')),
        b'a'..=b'z' => Some(26 + usize::from(key - b'a')),
        _ => None,
    }
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
    PayloadTooLong,
    InvalidControl,
    DuplicateKey(u8),
}

#[derive(Debug, PartialEq, Eq)]
pub enum PayloadError {
    InvalidChunk,
    Base64(DecodeSliceError),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Command<'a> {
    pub controls: Controls,
    pub encoded_payload: &'a [u8],
}

impl Command<'_> {
    /// Decodes this command's payload block into caller-provided storage.
    pub fn decode_payload(&self, output: &mut [u8]) -> Result<usize, PayloadError> {
        match self.controls.get(b'm') {
            None | Some(Value::Unsigned(0)) => {}
            Some(Value::Unsigned(1))
                if self.encoded_payload.len().is_multiple_of(4)
                    && !self.encoded_payload.ends_with(b"=") => {}
            _ => return Err(PayloadError::InvalidChunk),
        }
        PAYLOAD_ENGINE
            .decode_slice(self.encoded_payload, output)
            .map_err(PayloadError::Base64)
    }
}

enum State {
    Idle,
    Selector,
    Control,
    Payload,
    Ignore,
    Failed(DecodeError),
}

/// Decodes a Kitty APC from the payload spans of a terminal parser.
pub struct Decoder {
    state: State,
    control: [u8; MAX_CONTROL_BYTES],
    control_len: usize,
    payload: [u8; MAX_ENCODED_PAYLOAD_BYTES],
    payload_len: usize,
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            state: State::Idle,
            control: [0; MAX_CONTROL_BYTES],
            control_len: 0,
            payload: [0; MAX_ENCODED_PAYLOAD_BYTES],
            payload_len: 0,
        }
    }
}

impl Decoder {
    pub fn start(&mut self) {
        self.state = State::Selector;
        self.control_len = 0;
        self.payload_len = 0;
    }

    pub fn cancel(&mut self) {
        self.state = State::Idle;
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
        if matches!(self.state, State::Payload)
            && !append(&mut self.payload, &mut self.payload_len, bytes)
        {
            self.state = State::Failed(DecodeError::PayloadTooLong);
        }
    }

    pub fn finish(&mut self) -> Option<Result<Command<'_>, DecodeError>> {
        match core::mem::replace(&mut self.state, State::Idle) {
            State::Control | State::Payload => Some(
                Controls::parse(&self.control[..self.control_len]).and_then(|controls| {
                    if self.payload_len > MAX_DIRECT_PAYLOAD_BYTES
                        && !matches!(controls.get(b't'), Some(Value::Letter(b'f' | b't' | b's')))
                    {
                        return Err(DecodeError::PayloadTooLong);
                    }
                    Ok(Command {
                        controls,
                        encoded_payload: &self.payload[..self.payload_len],
                    })
                }),
            ),
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
