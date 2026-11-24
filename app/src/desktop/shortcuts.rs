use lune_terminal::ScreenBuffer;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Copy,
    Paste,
    OpenDirectory,
    ToggleFullscreen,
    Zoom(Zoom),
    Scroll(Scroll),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Zoom {
    In,
    Out,
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Scroll {
    Oldest,
    Live,
    PageUp,
    PageDown,
}

enum ShortcutKey {
    Letter(&'static str),
    Symbol(&'static str),
    Named(NamedKey),
}

enum Context {
    Window,
    PrimaryScreen,
}

pub(super) struct Binding {
    key: ShortcutKey,
    modifiers: ModifiersState,
    action: Action,
    context: Context,
    repeat: bool,
}

impl Binding {
    const fn new(action: Action, modifiers: ModifiersState, key: ShortcutKey) -> Self {
        Self {
            key,
            modifiers,
            action,
            context: Context::Window,
            repeat: false,
        }
    }

    const fn repeating(mut self) -> Self {
        self.repeat = true;
        self
    }

    const fn primary_screen(mut self) -> Self {
        self.context = Context::PrimaryScreen;
        self
    }

    pub(super) fn action(&self, repeat: bool) -> Option<Action> {
        (!repeat || self.repeat).then_some(self.action)
    }
}

use ModifiersState as Mods;
use ShortcutKey::{Letter, Named, Symbol};

const CTRL_SHIFT: Mods = Mods::CONTROL.union(Mods::SHIFT);
const WAYLAND_BINDINGS: &[Binding] = &[
    Binding::new(Action::Copy, CTRL_SHIFT, Letter("c")),
    Binding::new(Action::Paste, CTRL_SHIFT, Letter("v")),
    Binding::new(Action::OpenDirectory, CTRL_SHIFT, Letter("o")),
    Binding::new(
        Action::ToggleFullscreen,
        Mods::empty(),
        Named(NamedKey::F11),
    ),
    Binding::new(Action::Zoom(Zoom::In), Mods::CONTROL, Symbol("=")).repeating(),
    Binding::new(Action::Zoom(Zoom::In), Mods::CONTROL, Symbol("+")).repeating(),
    Binding::new(Action::Zoom(Zoom::In), CTRL_SHIFT, Symbol("+")).repeating(),
    Binding::new(Action::Zoom(Zoom::Out), Mods::CONTROL, Symbol("-")).repeating(),
    Binding::new(Action::Zoom(Zoom::Reset), Mods::CONTROL, Symbol("0")).repeating(),
    Binding::new(
        Action::Scroll(Scroll::Oldest),
        Mods::SHIFT,
        Named(NamedKey::Home),
    )
    .primary_screen()
    .repeating(),
    Binding::new(
        Action::Scroll(Scroll::Live),
        Mods::SHIFT,
        Named(NamedKey::End),
    )
    .primary_screen()
    .repeating(),
    Binding::new(
        Action::Scroll(Scroll::PageUp),
        Mods::SHIFT,
        Named(NamedKey::PageUp),
    )
    .primary_screen()
    .repeating(),
    Binding::new(
        Action::Scroll(Scroll::PageDown),
        Mods::SHIFT,
        Named(NamedKey::PageDown),
    )
    .primary_screen()
    .repeating(),
];

pub(super) fn binding(
    event: &KeyEvent,
    modifiers: ModifiersState,
    screen: ScreenBuffer,
) -> Option<&'static Binding> {
    find_binding(
        WAYLAND_BINDINGS,
        &event.logical_key,
        || event.key_without_modifiers(),
        modifiers,
        screen,
    )
}

fn find_binding<'a>(
    bindings: &'a [Binding],
    logical: &Key,
    unmodified: impl Fn() -> Key,
    modifiers: ModifiersState,
    screen: ScreenBuffer,
) -> Option<&'a Binding> {
    bindings.iter().find(|binding| {
        binding.modifiers == modifiers
            && (matches!(binding.context, Context::Window) || screen == ScreenBuffer::Primary)
            && match binding.key {
                Letter(letter) => {
                    // A layout modifier can produce a different character from the same base key.
                    matches!(logical, Key::Character(text) if text.eq_ignore_ascii_case(letter))
                        && matches!(unmodified(), Key::Character(text) if text.eq_ignore_ascii_case(letter))
                }
                Symbol(symbol) => matches!(logical, Key::Character(text) if text.as_str() == symbol),
                Named(named) => *logical == Key::Named(named),
            }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_context_and_modifiers_preserve_unrelated_keys() {
        let find = |key: Key, modifiers, screen| {
            find_binding(
                WAYLAND_BINDINGS,
                &key,
                || Key::Character("c".into()),
                modifiers,
                screen,
            )
        };
        let primary = ScreenBuffer::Primary;
        let alternate = ScreenBuffer::Alternate;
        let copy = find(Key::Character("C".into()), CTRL_SHIFT, primary).unwrap();
        assert_eq!(copy.action(false), Some(Action::Copy));
        assert_eq!(copy.action(true), None);
        for modifiers in [
            Mods::CONTROL,
            CTRL_SHIFT | Mods::ALT,
            CTRL_SHIFT | Mods::SUPER,
        ] {
            assert!(find(Key::Character("C".into()), modifiers, primary).is_none());
        }
        assert!(find(Key::Character("€".into()), CTRL_SHIFT, primary).is_none());
        let page_up = Key::Named(NamedKey::PageUp);
        assert_eq!(
            find(page_up.clone(), Mods::SHIFT, primary)
                .unwrap()
                .action(true),
            Some(Action::Scroll(Scroll::PageUp))
        );
        assert!(find(page_up, Mods::SHIFT, alternate).is_none());
        let plus = Key::Character("+".into());
        assert_eq!(
            find(plus, CTRL_SHIFT, primary).unwrap().action(true),
            Some(Action::Zoom(Zoom::In))
        );
    }
}
