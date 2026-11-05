/// Unmodified ANSI cursor, Return, and numeric-keypad keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Right,
    Left,
    Enter,
    KeypadDigit(u8),
    KeypadDecimal,
    KeypadComma,
    KeypadMinus,
    KeypadEnter,
}

/// A copyable snapshot of keyboard modes selected by the output stream.
/// Cursor-key and keypad application modes are independent. Default modes use
/// CSI cursor sequences, numeric keypad characters, and CR for Return.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputModes {
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub new_line: bool,
}

impl InputModes {
    /// Encodes a key without allocating. Returned bytes have static storage and
    /// can be retained across mode changes or partial transport writes.
    /// Returns `None` for keypad digit values above nine. In application keypad
    /// mode, keypad Enter sends SS3 M; the main Return key follows new-line mode.
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
            Key::Enter => {
                if self.new_line {
                    b"\r\n"
                } else {
                    b"\r"
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
