use lune_terminal::ScreenBuffer;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

use super::vi::{Command, Direction, Motion, VisualKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Copy,
    Paste,
    OpenDirectory,
    ToggleFullscreen,
    Zoom(Zoom),
    Scroll(Scroll),
    ToggleVi,
    PreviousPrompt,
    NextPrompt,
    CopyLastOutput,
    NewTab,
    CloseTab,
    NextTab,
    PreviousTab,
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
    Space,
}

enum Context {
    Window,
    PrimaryScreen,
}

pub(super) struct Binding<A = Action> {
    key: ShortcutKey,
    modifiers: ModifiersState,
    action: A,
    context: Context,
    repeat: bool,
}

impl<A: Copy> Binding<A> {
    const fn new(action: A, modifiers: ModifiersState, key: ShortcutKey) -> Self {
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

    pub(super) fn action(&self, repeat: bool) -> Option<A> {
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
    Binding::new(Action::NewTab, CTRL_SHIFT, Letter("t")),
    Binding::new(Action::CloseTab, CTRL_SHIFT, Letter("w")),
    Binding::new(Action::PreviousTab, CTRL_SHIFT, Named(NamedKey::ArrowLeft)),
    Binding::new(Action::NextTab, CTRL_SHIFT, Named(NamedKey::ArrowRight)),
    Binding::new(Action::PreviousTab, CTRL_SHIFT, Letter("h")),
    Binding::new(Action::NextTab, CTRL_SHIFT, Letter("l")),
    Binding::new(Action::NextTab, Mods::CONTROL, Named(NamedKey::Tab)),
    Binding::new(Action::PreviousTab, CTRL_SHIFT, Named(NamedKey::Tab)),
    Binding::new(Action::ToggleVi, CTRL_SHIFT, ShortcutKey::Space),
    Binding::new(Action::PreviousPrompt, CTRL_SHIFT, Named(NamedKey::ArrowUp))
        .primary_screen()
        .repeating(),
    Binding::new(Action::NextPrompt, CTRL_SHIFT, Named(NamedKey::ArrowDown))
        .primary_screen()
        .repeating(),
    Binding::new(Action::CopyLastOutput, CTRL_SHIFT, Letter("g")).primary_screen(),
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

impl Binding<Command> {
    const fn vi(command: Command, key: ShortcutKey) -> Self {
        Self::new(command, Mods::empty(), key)
    }

    const fn motion(motion: Motion, key: &'static str) -> Self {
        Self::vi(Command::Move(motion), Symbol(key)).repeating()
    }

    const fn vi_control(key: &'static str, command: Command) -> Self {
        Self::new(command, Mods::CONTROL, Letter(key)).repeating()
    }
}

const VI_BINDINGS: &[Binding<Command>] = &[
    Binding::motion(Motion::Left, "h"),
    Binding::motion(Motion::Down, "j"),
    Binding::motion(Motion::Up, "k"),
    Binding::motion(Motion::Right, "l"),
    Binding::motion(Motion::First, "0"),
    Binding::motion(Motion::FirstOccupied, "^"),
    Binding::motion(Motion::Last, "$"),
    Binding::motion(Motion::NextWord, "w"),
    Binding::motion(Motion::PreviousWord, "b"),
    Binding::motion(Motion::EndWord, "e"),
    Binding::motion(Motion::High, "H"),
    Binding::motion(Motion::Middle, "M"),
    Binding::motion(Motion::Low, "L"),
    Binding::motion(Motion::Bottom, "G"),
    Binding::vi_control("u", Command::Move(Motion::Page(-1))),
    Binding::vi_control("d", Command::Move(Motion::Page(1))),
    Binding::vi_control("b", Command::Move(Motion::Page(-2))),
    Binding::vi_control("f", Command::Move(Motion::Page(2))),
    Binding::vi_control("k", Command::Scroll(3)),
    Binding::vi_control("j", Command::Scroll(-3)),
    Binding::vi(Command::TopPrefix, Symbol("g")),
    Binding::vi(Command::Visual(VisualKind::Cell), Symbol("v")),
    Binding::vi(Command::Visual(VisualKind::Line), Symbol("V")),
    Binding::vi(Command::Yank, Symbol("y")),
    Binding::vi(Command::InnerOrExit, Symbol("i")),
    Binding::vi(Command::AroundOrExit, Symbol("a")),
    Binding::vi(Command::Exit, Symbol("q")),
    Binding::vi(Command::Cancel, Named(NamedKey::Escape)),
    Binding::vi(Command::Search(Direction::Forward), Symbol("/")),
    Binding::vi(Command::Search(Direction::Backward), Symbol("?")),
    Binding::vi(Command::RepeatSearch(false), Symbol("n")).repeating(),
    Binding::vi(Command::RepeatSearch(true), Symbol("N")).repeating(),
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

pub(super) fn vi_binding(
    event: &KeyEvent,
    mut modifiers: ModifiersState,
    screen: ScreenBuffer,
) -> Option<&'static Binding<Command>> {
    if matches!(event.logical_key, Key::Character(_)) && !modifiers.control_key() {
        modifiers.remove(Mods::SHIFT);
    }
    find_binding(
        VI_BINDINGS,
        &event.logical_key,
        || event.key_without_modifiers(),
        modifiers,
        screen,
    )
}

fn find_binding<'a, A>(
    bindings: &'a [Binding<A>],
    logical: &Key,
    unmodified: impl Fn() -> Key,
    modifiers: ModifiersState,
    screen: ScreenBuffer,
) -> Option<&'a Binding<A>> {
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
                ShortcutKey::Space => {
                    *logical == Key::Named(NamedKey::Space)
                        || matches!(logical, Key::Character(text) if text.as_str() == " ")
                }
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
