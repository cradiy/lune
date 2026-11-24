use std::io;

use lune_terminal::{
    Key, KeyEvent as TerminalKeyEvent, KeyEventKind, KeyInput, KeyModifiers, Modifiers, MouseEvent,
    PixelPosition,
};
use winit::event::KeyEvent;
use winit::keyboard::{Key as LogicalKey, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

use crate::Session;

pub(super) struct OwnedKeyEvent {
    key: KeyInput,
    shifted_key: Option<char>,
    base_layout_key: Option<char>,
    modifiers: KeyModifiers,
    kind: KeyEventKind,
    text: Option<String>,
}

pub(super) enum Input {
    KeyEvent(OwnedKeyEvent),
    Text(String),
    Raw(Vec<u8>),
    Focus(bool),
    Mouse(MouseEvent, PixelPosition, Modifiers),
    PendingPaste,
    Paste(String),
}

impl Input {
    pub(super) fn queue(&self, session: &mut Session) -> io::Result<()> {
        match self {
            Self::KeyEvent(event) => {
                session.queue_key_event(TerminalKeyEvent {
                    key: event.key,
                    shifted_key: event.shifted_key,
                    base_layout_key: event.base_layout_key,
                    modifiers: event.modifiers,
                    kind: event.kind,
                    text: event.text.as_deref(),
                })?;
            }
            Self::Focus(focused) => {
                session.queue_focus(*focused)?;
            }
            Self::Mouse(event, pixel, modifiers) => {
                session.mouse_input_at(*event, *pixel, *modifiers)?;
            }
            Self::Text(text) => session.queue_input(text.as_bytes())?,
            Self::Raw(bytes) => session.queue_input(bytes)?,
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

fn key_modifiers(state: ModifiersState) -> KeyModifiers {
    KeyModifiers {
        shift: state.shift_key(),
        alt: state.alt_key(),
        control: state.control_key(),
        super_key: state.super_key(),
    }
}

fn base_layout_key(physical: PhysicalKey) -> Option<char> {
    let PhysicalKey::Code(code) = physical else {
        return None;
    };
    Some(match code {
        KeyCode::KeyA => 'a',
        KeyCode::KeyB => 'b',
        KeyCode::KeyC => 'c',
        KeyCode::KeyD => 'd',
        KeyCode::KeyE => 'e',
        KeyCode::KeyF => 'f',
        KeyCode::KeyG => 'g',
        KeyCode::KeyH => 'h',
        KeyCode::KeyI => 'i',
        KeyCode::KeyJ => 'j',
        KeyCode::KeyK => 'k',
        KeyCode::KeyL => 'l',
        KeyCode::KeyM => 'm',
        KeyCode::KeyN => 'n',
        KeyCode::KeyO => 'o',
        KeyCode::KeyP => 'p',
        KeyCode::KeyQ => 'q',
        KeyCode::KeyR => 'r',
        KeyCode::KeyS => 's',
        KeyCode::KeyT => 't',
        KeyCode::KeyU => 'u',
        KeyCode::KeyV => 'v',
        KeyCode::KeyW => 'w',
        KeyCode::KeyX => 'x',
        KeyCode::KeyY => 'y',
        KeyCode::KeyZ => 'z',
        KeyCode::Digit0 => '0',
        KeyCode::Digit1 => '1',
        KeyCode::Digit2 => '2',
        KeyCode::Digit3 => '3',
        KeyCode::Digit4 => '4',
        KeyCode::Digit5 => '5',
        KeyCode::Digit6 => '6',
        KeyCode::Digit7 => '7',
        KeyCode::Digit8 => '8',
        KeyCode::Digit9 => '9',
        KeyCode::Backquote => '`',
        KeyCode::Minus => '-',
        KeyCode::Equal => '=',
        KeyCode::BracketLeft => '[',
        KeyCode::BracketRight => ']',
        KeyCode::Backslash => '\\',
        KeyCode::Semicolon => ';',
        KeyCode::Quote => '\'',
        KeyCode::Comma => ',',
        KeyCode::Period => '.',
        KeyCode::Slash => '/',
        _ => return None,
    })
}

fn single_char(key: &LogicalKey) -> Option<char> {
    let LogicalKey::Character(text) = key else {
        return None;
    };
    let mut chars = text.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

pub(super) fn key(event: &KeyEvent, state: ModifiersState, kitty_active: bool) -> Option<Input> {
    if state.super_key() && !kitty_active {
        return None;
    }
    let modifiers = key_modifiers(state);
    let kind = if !event.state.is_pressed() {
        KeyEventKind::Release
    } else if event.repeat {
        KeyEventKind::Repeat
    } else {
        KeyEventKind::Press
    };

    if event.logical_key == LogicalKey::Named(NamedKey::Space) {
        return Some(Input::KeyEvent(OwnedKeyEvent {
            key: KeyInput::Character(' '),
            shifted_key: None,
            base_layout_key: None,
            modifiers,
            kind,
            text: Some(" ".into()),
        }));
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
    let named = if matches!(
        event.logical_key,
        LogicalKey::Character(_) | LogicalKey::Named(NamedKey::Enter)
    ) {
        keypad.or(special)
    } else {
        special
    };
    if let Some(named) = named {
        return Some(Input::KeyEvent(OwnedKeyEvent {
            key: KeyInput::Named(named),
            shifted_key: None,
            base_layout_key: None,
            modifiers,
            kind,
            text: None,
        }));
    }
    let LogicalKey::Character(logical_text) = &event.logical_key else {
        return None;
    };
    let unmodified = single_char(&event.key_without_modifiers());
    let logical_char = single_char(&event.logical_key);
    let primary = unmodified.or(logical_char);
    if let Some(primary) = primary {
        let shifted_key = logical_char.filter(|&ch| modifiers.shift && ch != primary);
        let text = event
            .text
            .as_deref()
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
        return Some(Input::KeyEvent(OwnedKeyEvent {
            key: KeyInput::Character(primary),
            shifted_key,
            base_layout_key: base_layout_key(event.physical_key),
            modifiers,
            kind,
            text,
        }));
    }
    (kind != KeyEventKind::Release && !modifiers.control && !modifiers.alt && !modifiers.super_key)
        .then(|| Input::Text(logical_text.to_string()))
}
