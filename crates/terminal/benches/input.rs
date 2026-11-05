use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::{InputModes, Key};

fn measure(name: &str, modes: InputModes, keys: &[Key]) {
    let mut samples = [0.0_f64; 5];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(100) {
            for _ in 0..128 {
                for &key in black_box(keys) {
                    black_box(black_box(modes).encode_key(black_box(key)));
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
    let cursor = [Key::Up, Key::Down, Key::Right, Key::Left, Key::Enter];
    let keypad = [
        Key::KeypadDigit(0),
        Key::KeypadDigit(5),
        Key::KeypadDigit(9),
        Key::KeypadDecimal,
        Key::KeypadComma,
        Key::KeypadMinus,
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
        measure(&format!("{name} cursor"), modes, &cursor);
        measure(&format!("{name} keypad"), modes, &keypad);
    }
}
