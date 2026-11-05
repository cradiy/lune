/// Unmodified ANSI control, cursor, editing, function, and numeric-keypad keys.
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

/// A copyable snapshot of keyboard modes selected by the output stream.
/// Cursor-key and keypad application modes are independent. Default modes use
/// CSI cursor sequences, numeric keypad characters, CR for Return, and DEL for Backspace.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputModes {
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub new_line: bool,
    /// DECBKM: Backspace sends BS when enabled and DEL when disabled.
    pub backarrow: bool,
}

impl InputModes {
    /// Encodes a key without allocating. Returned bytes have static storage and
    /// can be retained across mode changes or partial transport writes.
    /// Returns `None` for keypad digits above nine or function keys outside 1–12.
    /// In application keypad mode, keypad Enter sends SS3 M; the main Return key
    /// follows new-line mode.
    #[inline]
    pub fn encode_key(self, key: Key) -> Option<&'static [u8]> {
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
