use std::num::NonZeroU8;

use crate::Paste;

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

/// Shift, Alt and Control state for one key event.
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

/// Owned key bytes that remain valid across mode changes and short writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct KeySequence {
    bytes: [u8; 7],
    len: NonZeroU8,
}

impl KeySequence {
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len.get())]
    }

    fn new(bytes: &[u8]) -> Self {
        let mut result = Self {
            bytes: [0; 7],
            len: NonZeroU8::new(bytes.len() as u8).expect("Key sequences are nonempty"),
        };
        result.bytes[..bytes.len()].copy_from_slice(bytes);
        result
    }

    fn modified(prefix: &[u8], parameter: u8, final_byte: u8) -> Self {
        let mut result = Self::new(prefix);
        let end = prefix.len();
        result.bytes[end..end + 3].copy_from_slice(&[b';', b'0' + parameter, final_byte]);
        result.len = NonZeroU8::new((end + 3) as u8).unwrap();
        result
    }
}

/// A copyable snapshot of keyboard and paste modes selected by the output stream.
/// Cursor-key and keypad application modes are independent. Default modes use
/// CSI cursor sequences, numeric keypad characters, CR for Return, and DEL for Backspace.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputModes {
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub new_line: bool,
    /// DECBKM: Backspace sends BS when enabled and DEL when disabled.
    pub backarrow: bool,
    /// DEC mode 2004: frame a paste with CSI 200~/201~ markers.
    pub bracketed_paste: bool,
}

impl InputModes {
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
