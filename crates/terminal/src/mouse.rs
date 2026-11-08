use std::num::NonZeroU8;

use crate::{CellPosition, InputModes, MAX_DIMENSION, Modifiers};

/// Mouse events selected by DEC modes 1000, 1002 and 1003.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseTracking {
    #[default]
    Disabled,
    Buttons,
    ButtonMotion,
    AnyMotion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left = 0,
    Middle = 1,
    Right = 2,
}

/// Motion events carry one held button, or `None` when no button is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEvent {
    Press(MouseButton),
    Release(MouseButton),
    Motion(Option<MouseButton>),
    WheelUp,
    WheelDown,
}

/// Owned mouse report bytes, retained by the caller across partial writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct MouseSequence {
    bytes: [u8; 18],
    len: NonZeroU8,
}

impl MouseSequence {
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len.get())]
    }

    fn push(&mut self, byte: u8) {
        self.bytes[usize::from(self.len.get())] = byte;
        self.len = NonZeroU8::new(self.len.get() + 1).unwrap();
    }

    fn number(&mut self, mut value: u16) {
        let mut digits = [0; 5];
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        let end = usize::from(self.len.get());
        self.bytes[end..end + digits.len() - start].copy_from_slice(&digits[start..]);
        self.len = NonZeroU8::new((end + digits.len() - start) as u8).unwrap();
    }
}

impl InputModes {
    /// Encodes a cell event without allocating; filtered events and out-of-range positions return `None`.
    /// The caller supplies cell changes, held-button state, and coordinates within the active grid.
    pub fn encode_mouse(
        self,
        event: MouseEvent,
        position: CellPosition,
        modifiers: Modifiers,
    ) -> Option<MouseSequence> {
        if self.mouse_tracking == MouseTracking::Disabled
            || position.row >= MAX_DIMENSION
            || position.column >= MAX_DIMENSION
        {
            return None;
        }
        let mut code = match event {
            MouseEvent::Press(button) | MouseEvent::Release(button) => button as u8,
            MouseEvent::WheelUp => 64,
            MouseEvent::WheelDown => 65,
            MouseEvent::Motion(button) => {
                if self.mouse_tracking != MouseTracking::AnyMotion
                    && (self.mouse_tracking != MouseTracking::ButtonMotion || button.is_none())
                {
                    return None;
                }
                32 + button.map_or(3, |button| button as u8)
            }
        };
        let release = matches!(event, MouseEvent::Release(_));
        if release && !self.sgr_mouse {
            code = 3;
        }
        code += 4 * u8::from(modifiers.shift)
            + 8 * u8::from(modifiers.alt)
            + 16 * u8::from(modifiers.control);
        let mut sequence = MouseSequence {
            bytes: [0; 18],
            len: NonZeroU8::new(3).unwrap(),
        };
        if self.sgr_mouse {
            sequence.bytes[..3].copy_from_slice(b"\x1b[<");
            sequence.number(u16::from(code));
            sequence.push(b';');
            sequence.number((position.column + 1) as u16);
            sequence.push(b';');
            sequence.number((position.row + 1) as u16);
            sequence.push(if release { b'm' } else { b'M' });
        } else {
            if position.row >= 223 || position.column >= 223 {
                return None;
            }
            sequence.bytes[..3].copy_from_slice(b"\x1b[M");
            sequence.push(code + 32);
            sequence.push(position.column as u8 + 33);
            sequence.push(position.row as u8 + 33);
        }
        Some(sequence)
    }
}
