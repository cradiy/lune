use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::{InputModes, Key, KeySequence, Modifiers};

fn measure_focus(name: &str, modes: InputModes) {
    let mut samples = [0.0_f64; 5];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(100) {
            for _ in 0..128 {
                for focused in black_box([true, false]) {
                    black_box(black_box(modes).encode_focus(black_box(focused)));
                }
            }
            iterations += 128;
        }
        *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * 2.0);
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<26} {:>8.3} ns/event", samples[2]);
}

fn measure_characters(name: &str, modifiers: Modifiers, characters: &[char]) {
    let mut samples = [0.0_f64; 5];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(100) {
            for _ in 0..128 {
                for &character in black_box(characters) {
                    black_box(KeySequence::from_character(
                        black_box(character),
                        black_box(modifiers),
                    ));
                }
            }
            iterations += 128;
        }
        *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * characters.len() as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<26} {:>8.3} ns/key", samples[2]);
}

fn measure(name: &str, modes: InputModes, modifiers: Modifiers, keys: &[Key]) {
    let mut samples = [0.0_f64; 5];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(100) {
            for _ in 0..128 {
                for &key in black_box(keys) {
                    black_box(black_box(modes).encode_key(black_box(key), black_box(modifiers)));
                }
            }
            iterations += 128;
        }
        *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * keys.len() as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<26} {:>8.3} ns/key", samples[2]);
}

fn main() {
    for focus_reporting in [false, true] {
        measure_focus(
            if focus_reporting {
                "focus enabled"
            } else {
                "focus disabled"
            },
            InputModes {
                focus_reporting,
                ..InputModes::default()
            },
        );
    }
    let cursor = [Key::Up, Key::Down, Key::Right, Key::Left, Key::Enter];
    let controls = [Key::Tab, Key::Escape, Key::Backspace, Key::Enter];
    let editing = [
        Key::Home,
        Key::End,
        Key::Insert,
        Key::Delete,
        Key::PageUp,
        Key::PageDown,
    ];
    let functions = std::array::from_fn::<_, 12, _>(|n| Key::Function(n as u8 + 1));
    let keypad = [
        Key::KeypadDigit(0),
        Key::KeypadDigit(5),
        Key::KeypadDigit(9),
        Key::KeypadDecimal,
        Key::KeypadComma,
        Key::KeypadMinus,
        Key::KeypadMultiply,
        Key::KeypadAdd,
        Key::KeypadDivide,
        Key::KeypadEqual,
        Key::KeypadEnter,
    ];
    for (name, modes) in [
        ("normal", InputModes::default()),
        (
            "application",
            InputModes {
                application_cursor: true,
                application_keypad: true,
                new_line: false,
                backarrow: true,
                bracketed_paste: false,
                focus_reporting: false,
            },
        ),
        (
            "numeric / new-line",
            InputModes {
                new_line: true,
                ..InputModes::default()
            },
        ),
    ] {
        measure(
            &format!("{name} cursor"),
            modes,
            Modifiers::default(),
            &cursor,
        );
        measure(
            &format!("{name} controls"),
            modes,
            Modifiers::default(),
            &controls,
        );
        measure(
            &format!("{name} editing"),
            modes,
            Modifiers::default(),
            &editing,
        );
        measure(
            &format!("{name} functions"),
            modes,
            Modifiers::default(),
            &functions,
        );
        measure(
            &format!("{name} keypad"),
            modes,
            Modifiers::default(),
            &keypad,
        );
    }
    for (name, modifiers) in [
        (
            "shift",
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        ),
        (
            "control",
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
        ),
        (
            "shift/alt/control",
            Modifiers {
                shift: true,
                alt: true,
                control: true,
            },
        ),
    ] {
        let modes = InputModes::default();
        measure(&format!("{name} cursor"), modes, modifiers, &cursor[..4]);
        measure(&format!("{name} editing"), modes, modifiers, &editing);
        measure(&format!("{name} functions"), modes, modifiers, &functions);
    }
    for (name, characters) in [
        ("ASCII", &['a', 'A', ' ', '2', '?', '~'][..]),
        ("Unicode", &['é', '水', '🦀', '\u{301}', '\u{10ffff}'][..]),
        ("control aliases", &['c', 'D', ' ', '3', '/', '?'][..]),
    ] {
        for (suffix, modifiers) in [
            ("plain", Modifiers::default()),
            (
                "alt",
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
            ),
            (
                "ctrl",
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
            ),
            (
                "alt/ctrl",
                Modifiers {
                    alt: true,
                    control: true,
                    ..Modifiers::default()
                },
            ),
        ] {
            measure_characters(&format!("{name} {suffix}"), modifiers, characters);
        }
    }
}
