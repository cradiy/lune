use std::io;

use lune_terminal::{Key, Modifiers, MouseEvent, PixelPosition};
use winit::event::KeyEvent;
use winit::keyboard::{Key as LogicalKey, KeyCode, ModifiersState, NamedKey, PhysicalKey};

use crate::Session;

pub(super) enum Input {
    Key(Key, Modifiers, bool),
    Character(char, Modifiers, bool),
    Text(String),
    Focus(bool),
    Mouse(MouseEvent, PixelPosition, Modifiers),
    PendingPaste,
    Paste(String),
}

impl Input {
    pub(super) fn queue(&self, session: &mut Session) -> io::Result<()> {
        match self {
            Self::Key(key, modifiers, repeat) => {
                session.queue_key(*key, *modifiers, *repeat)?;
            }
            Self::Character(ch, modifiers, repeat) => {
                session.queue_character(*ch, *modifiers, *repeat)?;
            }
            Self::Focus(focused) => {
                session.queue_focus(*focused)?;
            }
            Self::Mouse(event, pixel, modifiers) => {
                session.mouse_input_at(*event, *pixel, *modifiers)?;
            }
            Self::Text(text) => session.queue_input(text.as_bytes())?,
            Self::Paste(text) => session.queue_paste(text)?,
            Self::PendingPaste => return Err(io::ErrorKind::WouldBlock.into()),
        }
        Ok(())
    }
}

pub(super) fn terminal_modifiers(state: ModifiersState) -> Modifiers {
    Modifiers {
        shift: state.shift_key(),
        alt: state.alt_key(),
        control: state.control_key(),
    }
}

pub(super) fn key(event: &KeyEvent, state: ModifiersState) -> Option<Input> {
    if state.super_key() {
        return None;
    }
    let modifiers = terminal_modifiers(state);
    if event.logical_key == LogicalKey::Named(NamedKey::Space) {
        return Some(Input::Character(' ', modifiers, event.repeat));
    }
    let keypad = match event.physical_key {
        PhysicalKey::Code(code) => match code {
            KeyCode::Numpad0 => Some(Key::KeypadDigit(0)),
            KeyCode::Numpad1 => Some(Key::KeypadDigit(1)),
            KeyCode::Numpad2 => Some(Key::KeypadDigit(2)),
            KeyCode::Numpad3 => Some(Key::KeypadDigit(3)),
            KeyCode::Numpad4 => Some(Key::KeypadDigit(4)),
            KeyCode::Numpad5 => Some(Key::KeypadDigit(5)),
            KeyCode::Numpad6 => Some(Key::KeypadDigit(6)),
            KeyCode::Numpad7 => Some(Key::KeypadDigit(7)),
            KeyCode::Numpad8 => Some(Key::KeypadDigit(8)),
            KeyCode::Numpad9 => Some(Key::KeypadDigit(9)),
            KeyCode::NumpadAdd => Some(Key::KeypadAdd),
            KeyCode::NumpadSubtract => Some(Key::KeypadMinus),
            KeyCode::NumpadMultiply => Some(Key::KeypadMultiply),
            KeyCode::NumpadDivide => Some(Key::KeypadDivide),
            KeyCode::NumpadComma => Some(Key::KeypadComma),
            KeyCode::NumpadDecimal => Some(Key::KeypadDecimal),
            KeyCode::NumpadEnter => Some(Key::KeypadEnter),
            KeyCode::NumpadEqual => Some(Key::KeypadEqual),
            _ => None,
        },
        _ => None,
    };
    let special = match &event.logical_key {
        LogicalKey::Named(named) => match named {
            NamedKey::ArrowUp => Some(Key::Up),
            NamedKey::ArrowDown => Some(Key::Down),
            NamedKey::ArrowLeft => Some(Key::Left),
            NamedKey::ArrowRight => Some(Key::Right),
            NamedKey::Home => Some(Key::Home),
            NamedKey::End => Some(Key::End),
            NamedKey::Insert => Some(Key::Insert),
            NamedKey::Delete => Some(Key::Delete),
            NamedKey::PageUp => Some(Key::PageUp),
            NamedKey::PageDown => Some(Key::PageDown),
            NamedKey::Enter => Some(Key::Enter),
            NamedKey::Tab => Some(Key::Tab),
            NamedKey::Escape => Some(Key::Escape),
            NamedKey::Backspace => Some(Key::Backspace),
            NamedKey::F1 => Some(Key::Function(1)),
            NamedKey::F2 => Some(Key::Function(2)),
            NamedKey::F3 => Some(Key::Function(3)),
            NamedKey::F4 => Some(Key::Function(4)),
            NamedKey::F5 => Some(Key::Function(5)),
            NamedKey::F6 => Some(Key::Function(6)),
            NamedKey::F7 => Some(Key::Function(7)),
            NamedKey::F8 => Some(Key::Function(8)),
            NamedKey::F9 => Some(Key::Function(9)),
            NamedKey::F10 => Some(Key::Function(10)),
            NamedKey::F11 => Some(Key::Function(11)),
            NamedKey::F12 => Some(Key::Function(12)),
            _ => None,
        },
        _ => None,
    };
    let key = if matches!(
        event.logical_key,
        LogicalKey::Character(_) | LogicalKey::Named(NamedKey::Enter)
    ) {
        keypad.or(special)
    } else {
        special
    };
    if let Some(key) = key {
        return Some(Input::Key(key, modifiers, event.repeat));
    }
    let LogicalKey::Character(text) = &event.logical_key else {
        return None;
    };
    let mut chars = text.chars();
    let first = chars.next()?;
    if chars.next().is_none() {
        return Some(Input::Character(first, modifiers, event.repeat));
    }
    (!modifiers.control && !modifiers.alt).then(|| Input::Text(text.to_string()))
}
