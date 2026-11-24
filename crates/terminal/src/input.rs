use std::num::NonZeroU8;

use crate::{MouseFormat, MouseTracking, Paste};

/// Maximum depth of each buffer's Kitty keyboard flag stack.
pub const MAX_KITTY_KEYBOARD_STACK: usize = 16;

/// Progressive enhancement flags for the Kitty keyboard protocol.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KittyKeyboardFlags(u8);

impl KittyKeyboardFlags {
    pub const DISAMBIGUATE: u8 = 1 << 0;
    pub const REPORT_EVENTS: u8 = 1 << 1;
    pub const REPORT_ALTERNATE: u8 = 1 << 2;
    pub const REPORT_ALL_KEYS: u8 = 1 << 3;
    pub const REPORT_TEXT: u8 = 1 << 4;
    pub const ALL: u8 = 0x1f;

    #[inline]
    pub const fn new(bits: u8) -> Self {
        Self(bits & Self::ALL)
    }

    #[inline]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn contains(self, mask: u8) -> bool {
        self.0 & mask == mask
    }

    #[inline]
    pub const fn disambiguate(self) -> bool {
        self.contains(Self::DISAMBIGUATE)
    }

    #[inline]
    pub const fn report_events(self) -> bool {
        self.contains(Self::REPORT_EVENTS)
    }

    #[inline]
    pub const fn report_alternate(self) -> bool {
        self.contains(Self::REPORT_ALTERNATE)
    }

    #[inline]
    pub const fn report_all_keys(self) -> bool {
        self.contains(Self::REPORT_ALL_KEYS)
    }

    #[inline]
    pub const fn report_text(self) -> bool {
        self.contains(Self::REPORT_TEXT)
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct KittyKeyboardStack {
    entries: [KittyKeyboardFlags; MAX_KITTY_KEYBOARD_STACK],
    len: u8,
}

impl KittyKeyboardStack {
    #[inline]
    pub(crate) fn current(&self) -> KittyKeyboardFlags {
        if self.len == 0 {
            KittyKeyboardFlags::default()
        } else {
            self.entries[usize::from(self.len) - 1]
        }
    }

    pub(crate) fn push(&mut self, flags: KittyKeyboardFlags) {
        let len = usize::from(self.len);
        if len == MAX_KITTY_KEYBOARD_STACK {
            self.entries.copy_within(1.., 0);
            self.entries[MAX_KITTY_KEYBOARD_STACK - 1] = flags;
        } else {
            self.entries[len] = flags;
            self.len += 1;
        }
    }

    pub(crate) fn pop(&mut self, count: usize) {
        let count = count.min(usize::from(self.len)) as u8;
        self.len -= count;
    }

    pub(crate) fn set(&mut self, flags: KittyKeyboardFlags, mode: u16) -> bool {
        let current = self.current().bits();
        let next = match mode {
            1 => flags.bits(),
            2 => current | flags.bits(),
            3 => current & !flags.bits(),
            _ => return false,
        };
        let next = KittyKeyboardFlags::new(next);
        if self.len == 0 {
            self.entries[0] = next;
            self.len = 1;
        } else {
            self.entries[usize::from(self.len) - 1] = next;
        }
        true
    }
}

/// Terminal keys with protocol-defined encodings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Right,
    Left,
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
    Function(u8),
    Enter,
    Tab,
    Escape,
    Backspace,
    KeypadDigit(u8),
    KeypadDecimal,
    KeypadComma,
    KeypadMinus,
    KeypadMultiply,
    KeypadAdd,
    KeypadDivide,
    KeypadEqual,
    KeypadEnter,
}

/// Shift, Alt and Control state for legacy key and mouse events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

impl Modifiers {
    #[inline]
    fn parameter(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.control)
    }
}

/// Modifier state for rich keyboard events, including Super.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyModifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
    pub super_key: bool,
}

impl From<Modifiers> for KeyModifiers {
    #[inline]
    fn from(value: Modifiers) -> Self {
        Self {
            shift: value.shift,
            alt: value.alt,
            control: value.control,
            super_key: false,
        }
    }
}

impl KeyModifiers {
    #[inline]
    fn kitty_parameter(self) -> u8 {
        1 + u8::from(self.shift)
            + 2 * u8::from(self.alt)
            + 4 * u8::from(self.control)
            + 8 * u8::from(self.super_key)
    }

    #[inline]
    fn any(self) -> bool {
        self.shift || self.alt || self.control || self.super_key
    }

    #[inline]
    fn has_non_shift(self) -> bool {
        self.alt || self.control || self.super_key
    }

    #[inline]
    fn legacy(self) -> Option<Modifiers> {
        (!self.super_key).then_some(Modifiers {
            shift: self.shift,
            alt: self.alt,
            control: self.control,
        })
    }
}

/// Physical key action kind for keyboard protocol reporting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyEventKind {
    #[default]
    Press,
    Repeat,
    Release,
}

/// A named functional key or an unshifted Unicode key scalar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyInput {
    Named(Key),
    Character(char),
}

/// A complete keyboard event for legacy or Kitty keyboard protocol encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent<'a> {
    pub key: KeyInput,
    pub shifted_key: Option<char>,
    pub base_layout_key: Option<char>,
    pub modifiers: KeyModifiers,
    pub kind: KeyEventKind,
    pub text: Option<&'a str>,
}

const KEY_SEQUENCE_CAPACITY: usize = 64;

/// Owned key bytes that remain valid across mode changes and short writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct KeySequence {
    bytes: [u8; KEY_SEQUENCE_CAPACITY],
    len: NonZeroU8,
}

impl KeySequence {
    /// Encodes a layout-resolved scalar with legacy Control mapping and an Alt ESC prefix.
    /// Shift is already reflected in the scalar; unmapped Control combinations return `None`.
    pub fn from_character(character: char, modifiers: Modifiers) -> Option<Self> {
        let mut result = Self {
            bytes: [0; KEY_SEQUENCE_CAPACITY],
            len: NonZeroU8::MIN,
        };
        let start = usize::from(modifiers.alt);
        if modifiers.alt {
            result.bytes[0] = 0x1b;
        }
        let len = if modifiers.control {
            result.bytes[start] = match character {
                '\0'..='\x1f' | '\x7f' => character as u8,
                '@'..='~' | ' ' => character as u8 & 0x1f,
                '2' => 0,
                '3'..='7' => character as u8 - b'3' + 0x1b,
                '8' | '?' => 0x7f,
                '/' => 0x1f,
                _ => return None,
            };
            1
        } else {
            character.encode_utf8(&mut result.bytes[start..]).len()
        };
        result.len = NonZeroU8::new((start + len) as u8).unwrap();
        Some(result)
    }

    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len.get())]
    }

    fn new(bytes: &[u8]) -> Self {
        debug_assert!(!bytes.is_empty() && bytes.len() <= KEY_SEQUENCE_CAPACITY);
        let mut result = Self {
            bytes: [0; KEY_SEQUENCE_CAPACITY],
            len: NonZeroU8::new(bytes.len() as u8).expect("Key sequences are nonempty"),
        };
        result.bytes[..bytes.len()].copy_from_slice(bytes);
        result
    }

    fn try_from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > KEY_SEQUENCE_CAPACITY {
            return None;
        }
        Some(Self::new(bytes))
    }

    fn modified(prefix: &[u8], parameter: u8, final_byte: u8) -> Self {
        let mut result = Self::new(prefix);
        let end = prefix.len();
        result.bytes[end..end + 3].copy_from_slice(&[b';', b'0' + parameter, final_byte]);
        result.len = NonZeroU8::new((end + 3) as u8).unwrap();
        result
    }
}

struct SequenceBuilder {
    bytes: [u8; KEY_SEQUENCE_CAPACITY],
    len: usize,
    overflow: bool,
}

impl SequenceBuilder {
    fn new() -> Self {
        Self {
            bytes: [0; KEY_SEQUENCE_CAPACITY],
            len: 0,
            overflow: false,
        }
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        if self.overflow || bytes.len() > KEY_SEQUENCE_CAPACITY - self.len {
            self.overflow = true;
            return;
        }
        self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    fn push_byte(&mut self, byte: u8) {
        self.push_bytes(&[byte]);
    }

    fn push_u32(&mut self, mut value: u32) {
        let mut digits = [0u8; 10];
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.push_bytes(&digits[start..]);
    }

    fn finish(self) -> Option<KeySequence> {
        if self.overflow || self.len == 0 {
            return None;
        }
        Some(KeySequence {
            bytes: self.bytes,
            len: NonZeroU8::new(self.len as u8)?,
        })
    }
}

/// A copyable snapshot of input modes selected by the output stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputModes {
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub new_line: bool,
    /// DECBKM: Backspace sends BS when enabled and DEL when disabled.
    pub backarrow: bool,
    /// DEC mode 2004: frame a paste with CSI 200~/201~ markers.
    pub bracketed_paste: bool,
    /// DEC mode 1004: report terminal focus changes.
    pub focus_reporting: bool,
    pub mouse_tracking: MouseTracking,
    pub mouse_format: MouseFormat,
    /// DEC mode 1007: send cursor keys for wheel events in the alternate buffer.
    pub alternate_scroll: bool,
    /// Active Kitty keyboard protocol enhancement flags for the active screen buffer.
    pub kitty_keyboard: KittyKeyboardFlags,
}

impl InputModes {
    /// Encodes a focus change when reporting is enabled.
    #[inline]
    pub fn encode_focus(self, focused: bool) -> Option<&'static [u8]> {
        if !self.focus_reporting {
            None
        } else if focused {
            Some(b"\x1b[I")
        } else {
            Some(b"\x1b[O")
        }
    }

    /// Borrows the payload with this snapshot's paste framing.
    /// Payload bytes and line endings are preserved.
    #[inline]
    pub fn encode_paste(self, payload: &str) -> Paste<'_> {
        Paste::new(payload, self.bracketed_paste)
    }

    /// Encodes navigation/editing/function modifiers and Shift-Tab without allocating.
    /// Unsupported key/modifier pairs and invalid key numbers return `None`.
    #[inline]
    pub fn encode_key(self, key: Key, modifiers: Modifiers) -> Option<KeySequence> {
        let base = self.unmodified_key(key)?;
        let parameter = modifiers.parameter();
        if parameter == 1 {
            return Some(KeySequence::new(base));
        }
        Some(match key {
            Key::Up
            | Key::Down
            | Key::Right
            | Key::Left
            | Key::Home
            | Key::End
            | Key::Function(1..=4) => {
                KeySequence::modified(b"\x1b[1", parameter, *base.last().unwrap())
            }
            Key::Insert | Key::Delete | Key::PageUp | Key::PageDown | Key::Function(5..=12) => {
                KeySequence::modified(&base[..base.len() - 1], parameter, b'~')
            }
            Key::Tab if parameter == 2 => KeySequence::new(b"\x1b[Z"),
            _ => return None,
        })
    }

    /// Encodes a key event using active Kitty keyboard protocol flags, falling back
    /// to legacy encoding when Kitty keyboard flags are inactive.
    pub fn encode_key_event(self, event: KeyEvent<'_>) -> Option<KeySequence> {
        let flags = self.kitty_keyboard;
        if flags.is_empty() {
            if event.kind == KeyEventKind::Release {
                return None;
            }
            let modifiers = event.modifiers.legacy()?;
            return match event.key {
                KeyInput::Named(key) => self.encode_key(key, modifiers),
                KeyInput::Character(character) => {
                    if !modifiers.control
                        && !modifiers.alt
                        && let Some(text) = event.text
                        && !text.is_empty()
                    {
                        return KeySequence::try_from_bytes(text.as_bytes());
                    }
                    let scalar = if modifiers.shift {
                        event.shifted_key.unwrap_or(character)
                    } else {
                        character
                    };
                    KeySequence::from_character(scalar, modifiers)
                }
            };
        }

        if event.kind == KeyEventKind::Release && !flags.report_events() {
            return None;
        }
        let event_code = if flags.report_events() {
            match event.kind {
                KeyEventKind::Press => None,
                KeyEventKind::Repeat => Some(2u8),
                KeyEventKind::Release => Some(3u8),
            }
        } else {
            None
        };
        let mods = event.modifiers.kitty_parameter();

        match event.key {
            KeyInput::Character(character) => {
                if !flags.report_all_keys()
                    && !event.modifiers.has_non_shift()
                    && event_code.is_none()
                {
                    if let Some(text) = event.text
                        && !text.is_empty()
                    {
                        return KeySequence::try_from_bytes(text.as_bytes());
                    }
                    let scalar = if event.modifiers.shift {
                        event.shifted_key.unwrap_or(character)
                    } else {
                        character
                    };
                    let mut utf8 = [0u8; 4];
                    return Some(KeySequence::new(scalar.encode_utf8(&mut utf8).as_bytes()));
                }
                let shifted = (flags.report_alternate() && event.modifiers.shift)
                    .then_some(event.shifted_key)
                    .flatten()
                    .filter(|&s| s != character);
                let base = flags
                    .report_alternate()
                    .then_some(event.base_layout_key)
                    .flatten()
                    .filter(|&b| b != character);
                let text = (flags.report_text()
                    && flags.report_all_keys()
                    && event.kind != KeyEventKind::Release)
                    .then_some(event.text)
                    .flatten();
                encode_kitty_csi_u(character as u32, shifted, base, mods, event_code, text)
            }
            KeyInput::Named(key) => {
                self.encode_kitty_named(key, event.modifiers, event.kind, mods, event_code, flags)
            }
        }
    }

    fn encode_kitty_named(
        self,
        key: Key,
        modifiers: KeyModifiers,
        kind: KeyEventKind,
        mods: u8,
        event_code: Option<u8>,
        flags: KittyKeyboardFlags,
    ) -> Option<KeySequence> {
        match key {
            Key::Enter | Key::Tab | Key::Backspace => {
                if !flags.report_all_keys() && !modifiers.any() && event_code.is_none() {
                    return self.encode_key(key, Modifiers::default());
                }
                let code = match key {
                    Key::Enter => 13,
                    Key::Tab => 9,
                    Key::Backspace => 127,
                    _ => unreachable!(),
                };
                encode_kitty_csi_u(code, None, None, mods, event_code, None)
            }
            Key::Escape => {
                if flags.disambiguate()
                    || flags.report_all_keys()
                    || modifiers.any()
                    || event_code.is_some()
                {
                    encode_kitty_csi_u(27, None, None, mods, event_code, None)
                } else {
                    Some(KeySequence::new(b"\x1b"))
                }
            }
            Key::Up | Key::Down | Key::Right | Key::Left | Key::Home | Key::End => {
                let final_byte = match key {
                    Key::Up => b'A',
                    Key::Down => b'B',
                    Key::Right => b'C',
                    Key::Left => b'D',
                    Key::Home => b'H',
                    Key::End => b'F',
                    _ => unreachable!(),
                };
                if mods == 1 && event_code.is_none() {
                    if flags.report_all_keys() {
                        Some(KeySequence::new(&[0x1b, b'[', final_byte]))
                    } else {
                        self.encode_key(key, Modifiers::default())
                    }
                } else {
                    encode_kitty_functional(1, final_byte, mods, event_code)
                }
            }
            Key::Function(number @ (1 | 2 | 4)) => {
                let final_byte = match number {
                    1 => b'P',
                    2 => b'Q',
                    4 => b'S',
                    _ => unreachable!(),
                };
                if mods == 1 && event_code.is_none() {
                    if flags.disambiguate() || flags.report_all_keys() {
                        Some(KeySequence::new(&[0x1b, b'[', final_byte]))
                    } else {
                        self.encode_key(key, Modifiers::default())
                    }
                } else {
                    encode_kitty_functional(1, final_byte, mods, event_code)
                }
            }
            Key::Function(3) => {
                if !flags.disambiguate()
                    && !flags.report_all_keys()
                    && mods == 1
                    && event_code.is_none()
                {
                    self.encode_key(key, Modifiers::default())
                } else {
                    encode_kitty_tilde(13, mods, event_code)
                }
            }
            Key::Insert | Key::Delete | Key::PageUp | Key::PageDown | Key::Function(5..=12) => {
                let code = match key {
                    Key::Insert => 2,
                    Key::Delete => 3,
                    Key::PageUp => 5,
                    Key::PageDown => 6,
                    Key::Function(5) => 15,
                    Key::Function(6) => 17,
                    Key::Function(7) => 18,
                    Key::Function(8) => 19,
                    Key::Function(9) => 20,
                    Key::Function(10) => 21,
                    Key::Function(11) => 23,
                    Key::Function(12) => 24,
                    _ => unreachable!(),
                };
                encode_kitty_tilde(code, mods, event_code)
            }
            Key::Function(_) => None,
            Key::KeypadDigit(digit) => {
                if digit > 9 {
                    return None;
                }
                if !flags.disambiguate()
                    && !flags.report_all_keys()
                    && !modifiers.any()
                    && event_code.is_none()
                {
                    return self.encode_key(key, Modifiers::default());
                }
                let digit_byte = [b'0' + digit];
                let digit_str = std::str::from_utf8(&digit_byte).ok();
                let text = (flags.report_text()
                    && flags.report_all_keys()
                    && kind != KeyEventKind::Release)
                    .then_some(digit_str)
                    .flatten();
                encode_kitty_csi_u(57399 + u32::from(digit), None, None, mods, event_code, text)
            }
            Key::KeypadDecimal
            | Key::KeypadDivide
            | Key::KeypadMultiply
            | Key::KeypadMinus
            | Key::KeypadAdd
            | Key::KeypadEnter
            | Key::KeypadEqual
            | Key::KeypadComma => {
                if !flags.disambiguate()
                    && !flags.report_all_keys()
                    && !modifiers.any()
                    && event_code.is_none()
                {
                    return self.encode_key(key, Modifiers::default());
                }
                let (code, symbol) = match key {
                    Key::KeypadDecimal => (57409, Some(".")),
                    Key::KeypadDivide => (57410, Some("/")),
                    Key::KeypadMultiply => (57411, Some("*")),
                    Key::KeypadMinus => (57412, Some("-")),
                    Key::KeypadAdd => (57413, Some("+")),
                    Key::KeypadEnter => (57414, None),
                    Key::KeypadEqual => (57415, Some("=")),
                    Key::KeypadComma => (57416, Some(",")),
                    _ => unreachable!(),
                };
                let text = (flags.report_text()
                    && flags.report_all_keys()
                    && kind != KeyEventKind::Release)
                    .then_some(symbol)
                    .flatten();
                encode_kitty_csi_u(code, None, None, mods, event_code, text)
            }
        }
    }

    #[inline]
    fn unmodified_key(self, key: Key) -> Option<&'static [u8]> {
        Some(match key {
            Key::Up => {
                if self.application_cursor {
                    b"\x1bOA"
                } else {
                    b"\x1b[A"
                }
            }
            Key::Down => {
                if self.application_cursor {
                    b"\x1bOB"
                } else {
                    b"\x1b[B"
                }
            }
            Key::Right => {
                if self.application_cursor {
                    b"\x1bOC"
                } else {
                    b"\x1b[C"
                }
            }
            Key::Left => {
                if self.application_cursor {
                    b"\x1bOD"
                } else {
                    b"\x1b[D"
                }
            }
            Key::Home => {
                if self.application_cursor {
                    b"\x1bOH"
                } else {
                    b"\x1b[H"
                }
            }
            Key::End => {
                if self.application_cursor {
                    b"\x1bOF"
                } else {
                    b"\x1b[F"
                }
            }
            Key::Insert => b"\x1b[2~",
            Key::Delete => b"\x1b[3~",
            Key::PageUp => b"\x1b[5~",
            Key::PageDown => b"\x1b[6~",
            Key::Function(number) => {
                const FUNCTIONS: [&[u8]; 12] = [
                    b"\x1bOP",
                    b"\x1bOQ",
                    b"\x1bOR",
                    b"\x1bOS",
                    b"\x1b[15~",
                    b"\x1b[17~",
                    b"\x1b[18~",
                    b"\x1b[19~",
                    b"\x1b[20~",
                    b"\x1b[21~",
                    b"\x1b[23~",
                    b"\x1b[24~",
                ];
                return FUNCTIONS.get(usize::from(number.checked_sub(1)?)).copied();
            }
            Key::Enter => {
                if self.new_line {
                    b"\r\n"
                } else {
                    b"\r"
                }
            }
            Key::Tab => b"\t",
            Key::Escape => b"\x1b",
            Key::Backspace => {
                if self.backarrow {
                    b"\x08"
                } else {
                    b"\x7f"
                }
            }
            Key::KeypadEnter if self.application_keypad => b"\x1bOM",
            Key::KeypadEnter => {
                if self.new_line {
                    b"\r\n"
                } else {
                    b"\r"
                }
            }
            Key::KeypadDecimal => {
                if self.application_keypad {
                    b"\x1bOn"
                } else {
                    b"."
                }
            }
            Key::KeypadComma => {
                if self.application_keypad {
                    b"\x1bOl"
                } else {
                    b","
                }
            }
            Key::KeypadMinus => {
                if self.application_keypad {
                    b"\x1bOm"
                } else {
                    b"-"
                }
            }
            Key::KeypadMultiply => {
                if self.application_keypad {
                    b"\x1bOj"
                } else {
                    b"*"
                }
            }
            Key::KeypadAdd => {
                if self.application_keypad {
                    b"\x1bOk"
                } else {
                    b"+"
                }
            }
            Key::KeypadDivide => {
                if self.application_keypad {
                    b"\x1bOo"
                } else {
                    b"/"
                }
            }
            Key::KeypadEqual => {
                if self.application_keypad {
                    b"\x1bOX"
                } else {
                    b"="
                }
            }
            Key::KeypadDigit(digit) => {
                const DIGITS: [[&[u8]; 10]; 2] = [
                    [b"0", b"1", b"2", b"3", b"4", b"5", b"6", b"7", b"8", b"9"],
                    [
                        b"\x1bOp", b"\x1bOq", b"\x1bOr", b"\x1bOs", b"\x1bOt", b"\x1bOu",
                        b"\x1bOv", b"\x1bOw", b"\x1bOx", b"\x1bOy",
                    ],
                ];
                return DIGITS[usize::from(self.application_keypad)]
                    .get(usize::from(digit))
                    .copied();
            }
        })
    }
}

fn encode_kitty_csi_u(
    code: u32,
    shifted: Option<char>,
    base: Option<char>,
    mods: u8,
    event_code: Option<u8>,
    text: Option<&str>,
) -> Option<KeySequence> {
    let mut builder = SequenceBuilder::new();
    builder.push_bytes(b"\x1b[");
    builder.push_u32(code);
    if let Some(shifted) = shifted {
        builder.push_byte(b':');
        builder.push_u32(shifted as u32);
        if let Some(base) = base {
            builder.push_byte(b':');
            builder.push_u32(base as u32);
        }
    } else if let Some(base) = base {
        builder.push_bytes(b"::");
        builder.push_u32(base as u32);
    }

    let has_text = text.is_some_and(|text| text.chars().any(is_kitty_text_scalar));
    if mods != 1 || event_code.is_some() || has_text {
        builder.push_byte(b';');
        builder.push_u32(u32::from(mods));
        if let Some(event) = event_code {
            builder.push_byte(b':');
            builder.push_u32(u32::from(event));
        }
    }
    if has_text && let Some(text) = text {
        builder.push_byte(b';');
        let mut first = true;
        for ch in text.chars().filter(|&ch| is_kitty_text_scalar(ch)) {
            if !first {
                builder.push_byte(b':');
            }
            first = false;
            builder.push_u32(ch as u32);
        }
    }
    builder.push_byte(b'u');
    builder.finish()
}

fn encode_kitty_functional(
    number: u32,
    final_byte: u8,
    mods: u8,
    event_code: Option<u8>,
) -> Option<KeySequence> {
    let mut builder = SequenceBuilder::new();
    builder.push_bytes(b"\x1b[");
    builder.push_u32(number);
    builder.push_byte(b';');
    builder.push_u32(u32::from(mods));
    if let Some(event) = event_code {
        builder.push_byte(b':');
        builder.push_u32(u32::from(event));
    }
    builder.push_byte(final_byte);
    builder.finish()
}

fn encode_kitty_tilde(number: u32, mods: u8, event_code: Option<u8>) -> Option<KeySequence> {
    let mut builder = SequenceBuilder::new();
    builder.push_bytes(b"\x1b[");
    builder.push_u32(number);
    if mods != 1 || event_code.is_some() {
        builder.push_byte(b';');
        builder.push_u32(u32::from(mods));
        if let Some(event) = event_code {
            builder.push_byte(b':');
            builder.push_u32(u32::from(event));
        }
    }
    builder.push_byte(b'~');
    builder.finish()
}

#[inline]
fn is_kitty_text_scalar(ch: char) -> bool {
    let code = ch as u32;
    code >= 0x20 && !(0x7f..=0x9f).contains(&code)
}
