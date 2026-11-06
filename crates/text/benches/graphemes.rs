use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_text::{GraphemeBuffer, TextUpdate, WidthPolicy};

fn measure(name: &str, input: &str, scalars: bool) {
    let mut samples = [0.0_f64; 3];
    let mut buffer = GraphemeBuffer::new(WidthPolicy::Narrow);
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0;
        while start.elapsed() < Duration::from_millis(150) {
            buffer.clear();
            let mut accept = |update: TextUpdate<'_>| {
                match update {
                    TextUpdate::Ascii(text) => {
                        black_box(text);
                    }
                    TextUpdate::Start(unit) | TextUpdate::Extend(unit) => {
                        black_box(unit);
                    }
                }
                Ok::<_, ()>(())
            };
            if scalars {
                for scalar in input.chars() {
                    buffer
                        .advance(black_box(scalar.encode_utf8(&mut [0; 4])), &mut accept)
                        .unwrap();
                }
            } else {
                buffer.advance(black_box(input), &mut accept).unwrap();
            }
            black_box(buffer.current());
            iterations += 1;
        }
        *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * input.len() as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<28} {:>8.3} ns/input byte", samples[1]);
}

fn main() {
    for (name, text) in [
        ("ASCII", "The quick brown fox 123! ".repeat(512)),
        ("CJK", "月光水色".repeat(512)),
        ("combining", "e\u{301}o\u{308}".repeat(512)),
        ("emoji", "👩🏽\u{200d}💻🇺🇸1\u{fe0f}\u{20e3}".repeat(256)),
        ("mixed", "text水e\u{301}❤\u{fe0f} ".repeat(512)),
        ("long cluster", String::from("é") + &"\u{301}".repeat(511)),
        (
            "selector chain",
            String::from("❤") + &"\u{fe0f}".repeat(340),
        ),
    ] {
        measure(&format!("{name} bulk"), &text, false);
        measure(&format!("{name} scalar"), &text, true);
    }
}
