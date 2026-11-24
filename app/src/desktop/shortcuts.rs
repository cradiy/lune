use std::borrow::Cow;

use lune_terminal::ScreenBuffer;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

use super::split::{Axis, Direction as SplitDirection};
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
    Search,
    Hints,
    PreviousPrompt,
    NextPrompt,
    CopyLastOutput,
    ReloadConfig,
    NewWindow,
    NewTab,
    CloseTab,
    NextTab,
    PreviousTab,
    SelectTab(usize),
    SelectLastTab,
    MoveTabLeft,
    MoveTabRight,
    SplitPane(Axis),
    FocusPane(SplitDirection),
    ResizePane(SplitDirection),
    TogglePaneZoom,
}

impl Action {
    const fn default_context(self) -> Context {
        match self {
            Self::Scroll(_) | Self::PreviousPrompt | Self::NextPrompt | Self::CopyLastOutput => {
                Context::PrimaryScreen
            }
            _ => Context::Window,
        }
    }

    const fn default_repeat(self) -> bool {
        matches!(
            self,
            Self::Zoom(_)
                | Self::Scroll(_)
                | Self::PreviousPrompt
                | Self::NextPrompt
                | Self::ResizePane(_)
        )
    }
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ShortcutKey {
    Letter(Cow<'static, str>),
    Symbol(Cow<'static, str>),
    Named(NamedKey),
    Space,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Context {
    Window,
    PrimaryScreen,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum KeybindAction {
    Action(Action),
    Unbind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Keybind {
    key: ShortcutKey,
    modifiers: ModifiersState,
    action: KeybindAction,
}

pub(super) struct Keymap {
    bindings: Vec<Binding>,
}

impl Keymap {
    pub(super) fn from_overrides(overrides: &[Keybind]) -> Self {
        let mut bindings = WAYLAND_BINDINGS.to_vec();
        for rule in overrides {
            bindings.retain(|existing| {
                !(existing.modifiers == rule.modifiers && existing.key == rule.key)
            });
            if let KeybindAction::Action(action) = rule.action {
                bindings.push(Binding {
                    key: rule.key.clone(),
                    modifiers: rule.modifiers,
                    action,
                    context: action.default_context(),
                    repeat: action.default_repeat(),
                });
            }
        }
        Self { bindings }
    }

    pub(super) fn binding(
        &self,
        event: &KeyEvent,
        modifiers: ModifiersState,
        screen: ScreenBuffer,
    ) -> Option<&Binding> {
        find_binding(
            &self.bindings,
            &event.logical_key,
            || event.key_without_modifiers(),
            modifiers,
            screen,
        )
    }
}

use ModifiersState as Mods;
use ShortcutKey::Named;

const fn letter(text: &'static str) -> ShortcutKey {
    ShortcutKey::Letter(Cow::Borrowed(text))
}

const fn symbol(text: &'static str) -> ShortcutKey {
    ShortcutKey::Symbol(Cow::Borrowed(text))
}

const CTRL_SHIFT: Mods = Mods::CONTROL.union(Mods::SHIFT);
const CTRL_ALT: Mods = Mods::CONTROL.union(Mods::ALT);
const CTRL_ALT_SHIFT: Mods = CTRL_ALT.union(Mods::SHIFT);
const WAYLAND_BINDINGS: &[Binding] = &[
    Binding::new(Action::Copy, CTRL_SHIFT, letter("c")),
    Binding::new(Action::Paste, CTRL_SHIFT, letter("v")),
    Binding::new(Action::OpenDirectory, CTRL_SHIFT, letter("o")),
    Binding::new(Action::NewWindow, CTRL_SHIFT, letter("n")),
    Binding::new(Action::SplitPane(Axis::Columns), CTRL_SHIFT, letter("d")),
    Binding::new(Action::SplitPane(Axis::Rows), CTRL_SHIFT, letter("e")),
    Binding::new(Action::TogglePaneZoom, CTRL_SHIFT, letter("z")),
    Binding::new(
        Action::FocusPane(SplitDirection::Left),
        CTRL_ALT,
        Named(NamedKey::ArrowLeft),
    ),
    Binding::new(
        Action::FocusPane(SplitDirection::Right),
        CTRL_ALT,
        Named(NamedKey::ArrowRight),
    ),
    Binding::new(
        Action::FocusPane(SplitDirection::Up),
        CTRL_ALT,
        Named(NamedKey::ArrowUp),
    ),
    Binding::new(
        Action::FocusPane(SplitDirection::Down),
        CTRL_ALT,
        Named(NamedKey::ArrowDown),
    ),
    Binding::new(
        Action::FocusPane(SplitDirection::Left),
        CTRL_ALT,
        letter("h"),
    ),
    Binding::new(
        Action::FocusPane(SplitDirection::Right),
        CTRL_ALT,
        letter("l"),
    ),
    Binding::new(Action::FocusPane(SplitDirection::Up), CTRL_ALT, letter("k")),
    Binding::new(
        Action::FocusPane(SplitDirection::Down),
        CTRL_ALT,
        letter("j"),
    ),
    Binding::new(
        Action::ResizePane(SplitDirection::Left),
        CTRL_ALT_SHIFT,
        Named(NamedKey::ArrowLeft),
    )
    .repeating(),
    Binding::new(
        Action::ResizePane(SplitDirection::Right),
        CTRL_ALT_SHIFT,
        Named(NamedKey::ArrowRight),
    )
    .repeating(),
    Binding::new(
        Action::ResizePane(SplitDirection::Up),
        CTRL_ALT_SHIFT,
        Named(NamedKey::ArrowUp),
    )
    .repeating(),
    Binding::new(
        Action::ResizePane(SplitDirection::Down),
        CTRL_ALT_SHIFT,
        Named(NamedKey::ArrowDown),
    )
    .repeating(),
    Binding::new(
        Action::ResizePane(SplitDirection::Left),
        CTRL_ALT_SHIFT,
        letter("h"),
    )
    .repeating(),
    Binding::new(
        Action::ResizePane(SplitDirection::Right),
        CTRL_ALT_SHIFT,
        letter("l"),
    )
    .repeating(),
    Binding::new(
        Action::ResizePane(SplitDirection::Up),
        CTRL_ALT_SHIFT,
        letter("k"),
    )
    .repeating(),
    Binding::new(
        Action::ResizePane(SplitDirection::Down),
        CTRL_ALT_SHIFT,
        letter("j"),
    )
    .repeating(),
    Binding::new(Action::NewTab, CTRL_SHIFT, letter("t")),
    Binding::new(Action::CloseTab, CTRL_SHIFT, letter("w")),
    Binding::new(Action::PreviousTab, CTRL_SHIFT, Named(NamedKey::ArrowLeft)),
    Binding::new(Action::NextTab, CTRL_SHIFT, Named(NamedKey::ArrowRight)),
    Binding::new(Action::PreviousTab, CTRL_SHIFT, letter("h")),
    Binding::new(Action::NextTab, CTRL_SHIFT, letter("l")),
    Binding::new(Action::NextTab, Mods::CONTROL, Named(NamedKey::Tab)),
    Binding::new(Action::PreviousTab, CTRL_SHIFT, Named(NamedKey::Tab)),
    Binding::new(Action::SelectTab(0), Mods::ALT, symbol("1")),
    Binding::new(Action::SelectTab(1), Mods::ALT, symbol("2")),
    Binding::new(Action::SelectTab(2), Mods::ALT, symbol("3")),
    Binding::new(Action::SelectTab(3), Mods::ALT, symbol("4")),
    Binding::new(Action::SelectTab(4), Mods::ALT, symbol("5")),
    Binding::new(Action::SelectTab(5), Mods::ALT, symbol("6")),
    Binding::new(Action::SelectTab(6), Mods::ALT, symbol("7")),
    Binding::new(Action::SelectTab(7), Mods::ALT, symbol("8")),
    Binding::new(Action::SelectLastTab, Mods::ALT, symbol("9")),
    Binding::new(Action::MoveTabLeft, CTRL_SHIFT, Named(NamedKey::PageUp)),
    Binding::new(Action::MoveTabRight, CTRL_SHIFT, Named(NamedKey::PageDown)),
    Binding::new(Action::ToggleVi, CTRL_SHIFT, ShortcutKey::Space),
    Binding::new(Action::Search, CTRL_SHIFT, letter("f")),
    Binding::new(Action::Hints, CTRL_SHIFT, letter("s")),
    Binding::new(Action::PreviousPrompt, CTRL_SHIFT, Named(NamedKey::ArrowUp))
        .primary_screen()
        .repeating(),
    Binding::new(Action::NextPrompt, CTRL_SHIFT, Named(NamedKey::ArrowDown))
        .primary_screen()
        .repeating(),
    Binding::new(Action::CopyLastOutput, CTRL_SHIFT, letter("g")).primary_screen(),
    Binding::new(Action::ReloadConfig, CTRL_SHIFT, symbol(",")),
    Binding::new(Action::ReloadConfig, CTRL_SHIFT, symbol("<")),
    Binding::new(
        Action::ToggleFullscreen,
        Mods::empty(),
        Named(NamedKey::F11),
    ),
    Binding::new(Action::Zoom(Zoom::In), Mods::CONTROL, symbol("=")).repeating(),
    Binding::new(Action::Zoom(Zoom::In), Mods::CONTROL, symbol("+")).repeating(),
    Binding::new(Action::Zoom(Zoom::In), CTRL_SHIFT, symbol("+")).repeating(),
    Binding::new(Action::Zoom(Zoom::Out), Mods::CONTROL, symbol("-")).repeating(),
    Binding::new(Action::Zoom(Zoom::Reset), Mods::CONTROL, symbol("0")).repeating(),
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
        Self::vi(Command::Move(motion), symbol(key)).repeating()
    }

    const fn vi_control(key: &'static str, command: Command) -> Self {
        Self::new(command, Mods::CONTROL, letter(key)).repeating()
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
    Binding::vi(Command::TopPrefix, symbol("g")),
    Binding::vi(Command::Visual(VisualKind::Cell), symbol("v")),
    Binding::vi(Command::Visual(VisualKind::Line), symbol("V")),
    Binding::vi_control("v", Command::Visual(VisualKind::Block)),
    Binding::vi(Command::Yank, symbol("y")),
    Binding::vi(Command::InnerOrExit, symbol("i")),
    Binding::vi(Command::AroundOrExit, symbol("a")),
    Binding::vi(Command::Exit, symbol("q")),
    Binding::vi(Command::Cancel, Named(NamedKey::Escape)),
    Binding::vi(Command::Search(Direction::Forward), symbol("/")),
    Binding::vi(Command::Search(Direction::Backward), symbol("?")),
    Binding::vi(Command::RepeatSearch(false), symbol("n")).repeating(),
    Binding::vi(Command::RepeatSearch(true), symbol("N")).repeating(),
];

pub(super) fn parse_keybind(value: &str) -> Result<Keybind, &'static str> {
    let (combo, action_name) = value
        .rsplit_once('=')
        .ok_or("keybind must be formatted as <keys>=<action>")?;
    let combo = combo.trim();
    let action_name = action_name.trim();
    if combo.is_empty() || action_name.is_empty() {
        return Err("keybind must be formatted as <keys>=<action>");
    }
    let action = parse_action(action_name)?;
    let (modifiers, key) = parse_combo(combo)?;
    Ok(Keybind {
        key,
        modifiers,
        action,
    })
}

fn parse_combo(combo: &str) -> Result<(ModifiersState, ShortcutKey), &'static str> {
    // Support trailing '+' key in combos such as `ctrl++` or `+`.
    let (mod_part, key_token) = if combo == "+" {
        ("", "+")
    } else if let Some(prefix) = combo.strip_suffix("++") {
        (prefix, "+")
    } else if let Some((mods, key)) = combo.rsplit_once('+') {
        (mods, key)
    } else {
        ("", combo)
    };

    let mut modifiers = Mods::empty();
    if !mod_part.is_empty() {
        for token in mod_part.split('+') {
            let token = token.trim();
            let flag =
                if token.eq_ignore_ascii_case("ctrl") || token.eq_ignore_ascii_case("control") {
                    Mods::CONTROL
                } else if token.eq_ignore_ascii_case("shift") {
                    Mods::SHIFT
                } else if token.eq_ignore_ascii_case("alt") {
                    Mods::ALT
                } else if token.eq_ignore_ascii_case("super")
                    || token.eq_ignore_ascii_case("meta")
                    || token.eq_ignore_ascii_case("cmd")
                {
                    Mods::SUPER
                } else {
                    return Err("keybind modifier must be ctrl, shift, alt, or super");
                };
            if modifiers.contains(flag) {
                return Err("duplicate modifier in keybind");
            }
            modifiers |= flag;
        }
    }

    let key = parse_key(key_token.trim())?;
    Ok((modifiers, key))
}

fn parse_key(token: &str) -> Result<ShortcutKey, &'static str> {
    if token.is_empty() {
        return Err("keybind is missing a key");
    }
    let lower = token.to_ascii_lowercase();
    let named = match lower.as_str() {
        "space" => return Ok(ShortcutKey::Space),
        "plus" => return Ok(ShortcutKey::Symbol(Cow::Borrowed("+"))),
        "minus" => return Ok(ShortcutKey::Symbol(Cow::Borrowed("-"))),
        "equal" | "equals" => return Ok(ShortcutKey::Symbol(Cow::Borrowed("="))),
        "comma" => return Ok(ShortcutKey::Symbol(Cow::Borrowed(","))),
        "period" => return Ok(ShortcutKey::Symbol(Cow::Borrowed("."))),
        "slash" => return Ok(ShortcutKey::Symbol(Cow::Borrowed("/"))),
        "backslash" => return Ok(ShortcutKey::Symbol(Cow::Borrowed("\\"))),
        "semicolon" => return Ok(ShortcutKey::Symbol(Cow::Borrowed(";"))),
        "enter" | "return" => NamedKey::Enter,
        "tab" => NamedKey::Tab,
        "escape" | "esc" => NamedKey::Escape,
        "backspace" => NamedKey::Backspace,
        "delete" => NamedKey::Delete,
        "insert" => NamedKey::Insert,
        "home" => NamedKey::Home,
        "end" => NamedKey::End,
        "pageup" | "page_up" => NamedKey::PageUp,
        "pagedown" | "page_down" => NamedKey::PageDown,
        "up" | "arrow_up" => NamedKey::ArrowUp,
        "down" | "arrow_down" => NamedKey::ArrowDown,
        "left" | "arrow_left" => NamedKey::ArrowLeft,
        "right" | "arrow_right" => NamedKey::ArrowRight,
        "f1" => NamedKey::F1,
        "f2" => NamedKey::F2,
        "f3" => NamedKey::F3,
        "f4" => NamedKey::F4,
        "f5" => NamedKey::F5,
        "f6" => NamedKey::F6,
        "f7" => NamedKey::F7,
        "f8" => NamedKey::F8,
        "f9" => NamedKey::F9,
        "f10" => NamedKey::F10,
        "f11" => NamedKey::F11,
        "f12" => NamedKey::F12,
        _ => {
            let mut chars = token.chars();
            let ch = chars.next().ok_or("invalid keybind key")?;
            if chars.next().is_some() || ch.is_control() {
                return Err("unknown key in keybind");
            }
            if ch.is_ascii_alphabetic() {
                return Ok(ShortcutKey::Letter(Cow::Owned(
                    ch.to_ascii_lowercase().to_string(),
                )));
            }
            return Ok(ShortcutKey::Symbol(Cow::Owned(ch.to_string())));
        }
    };
    Ok(ShortcutKey::Named(named))
}

fn parse_action(name: &str) -> Result<KeybindAction, &'static str> {
    let normalized = name.to_ascii_lowercase().replace('-', "_");
    let action = match normalized.as_str() {
        "unbind" | "none" => return Ok(KeybindAction::Unbind),
        "copy" => Action::Copy,
        "paste" => Action::Paste,
        "open_directory" => Action::OpenDirectory,
        "toggle_fullscreen" => Action::ToggleFullscreen,
        "zoom_in" => Action::Zoom(Zoom::In),
        "zoom_out" => Action::Zoom(Zoom::Out),
        "zoom_reset" => Action::Zoom(Zoom::Reset),
        "scroll_oldest" | "scroll_to_top" => Action::Scroll(Scroll::Oldest),
        "scroll_live" | "scroll_to_bottom" => Action::Scroll(Scroll::Live),
        "scroll_page_up" => Action::Scroll(Scroll::PageUp),
        "scroll_page_down" => Action::Scroll(Scroll::PageDown),
        "toggle_vi" => Action::ToggleVi,
        "search" | "find" => Action::Search,
        "hints" | "quick_select" => Action::Hints,
        "previous_prompt" => Action::PreviousPrompt,
        "next_prompt" => Action::NextPrompt,
        "copy_last_output" => Action::CopyLastOutput,
        "reload_config" => Action::ReloadConfig,
        "new_window" => Action::NewWindow,
        "new_tab" => Action::NewTab,
        "close_tab" | "close_pane" => Action::CloseTab,
        "next_tab" => Action::NextTab,
        "previous_tab" => Action::PreviousTab,
        "select_tab_1" => Action::SelectTab(0),
        "select_tab_2" => Action::SelectTab(1),
        "select_tab_3" => Action::SelectTab(2),
        "select_tab_4" => Action::SelectTab(3),
        "select_tab_5" => Action::SelectTab(4),
        "select_tab_6" => Action::SelectTab(5),
        "select_tab_7" => Action::SelectTab(6),
        "select_tab_8" => Action::SelectTab(7),
        "select_tab_9" | "select_last_tab" => Action::SelectLastTab,
        "move_tab_left" => Action::MoveTabLeft,
        "move_tab_right" => Action::MoveTabRight,
        "split_right" | "split_columns" => Action::SplitPane(Axis::Columns),
        "split_down" | "split_rows" => Action::SplitPane(Axis::Rows),
        "focus_pane_left" => Action::FocusPane(SplitDirection::Left),
        "focus_pane_right" => Action::FocusPane(SplitDirection::Right),
        "focus_pane_up" => Action::FocusPane(SplitDirection::Up),
        "focus_pane_down" => Action::FocusPane(SplitDirection::Down),
        "resize_pane_left" => Action::ResizePane(SplitDirection::Left),
        "resize_pane_right" => Action::ResizePane(SplitDirection::Right),
        "resize_pane_up" => Action::ResizePane(SplitDirection::Up),
        "resize_pane_down" => Action::ResizePane(SplitDirection::Down),
        "toggle_pane_zoom" => Action::TogglePaneZoom,
        _ => return Err("unknown keybind action"),
    };
    Ok(KeybindAction::Action(action))
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
            && match &binding.key {
                ShortcutKey::Letter(letter) => {
                    // A layout modifier can produce a different character from the same base key.
                    matches!(logical, Key::Character(text) if text.eq_ignore_ascii_case(letter))
                        && matches!(unmodified(), Key::Character(text) if text.eq_ignore_ascii_case(letter))
                }
                ShortcutKey::Symbol(symbol) => {
                    matches!(logical, Key::Character(text) if text.as_str() == symbol.as_ref())
                }
                ShortcutKey::Named(named) => *logical == Key::Named(*named),
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

    #[test]
    fn custom_keybinds_parse_override_and_unbind() {
        let rules = [
            parse_keybind("ctrl+shift+d=unbind").unwrap(),
            parse_keybind("alt+v=split_right").unwrap(),
            parse_keybind("ctrl+==zoom_in").unwrap(),
            parse_keybind("ctrl++=zoom_in").unwrap(),
        ];
        let map = Keymap::from_overrides(&rules);
        assert!(
            find_binding(
                &map.bindings,
                &Key::Character("d".into()),
                || Key::Character("d".into()),
                CTRL_SHIFT,
                ScreenBuffer::Primary,
            )
            .is_none()
        );
        let split = find_binding(
            &map.bindings,
            &Key::Character("v".into()),
            || Key::Character("v".into()),
            Mods::ALT,
            ScreenBuffer::Primary,
        )
        .unwrap();
        assert_eq!(split.action(false), Some(Action::SplitPane(Axis::Columns)));
        assert!(parse_keybind("ctrl+shift+t").is_err());
        assert!(parse_keybind("bogus+t=new_tab").is_err());
        assert!(parse_keybind("ctrl+t=unknown_action").is_err());
        assert!(parse_keybind("ctrl+ctrl+t=new_tab").is_err());
    }
}
