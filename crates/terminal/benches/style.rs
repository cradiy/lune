use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::Style;
use lune_vt::{Event, MAX_PARAMETERS, Parameter, Parser};

fn measure(name: &str, mut operation: impl FnMut()) {
    let mut samples = [0.0_f64; 5];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(100) {
            for _ in 0..128 {
                operation();
            }
            iterations += 128;
        }
        *sample = start.elapsed().as_nanos() as f64 / iterations as f64;
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<26} {:>8.3} ns/sequence", samples[2]);
}

fn main() {
    for (name, wire, accepted) in [
        (
            "basic attributes",
            &b"\x1b[1;2;3;4;5;7;8;9;31;104m"[..],
            true,
        ),
        (
            "selective resets",
            &b"\x1b[22;23;24;25;27;28;29;39;49m"[..],
            true,
        ),
        ("indexed colors", &b"\x1b[38;5;255;48:5:0m"[..], true),
        (
            "RGB / semicolon",
            &b"\x1b[38;2;0;128;255;48;2;255;0;1m"[..],
            true,
        ),
        (
            "RGB / colon",
            &b"\x1b[38:2::0:128:255;48:2:0:255:0:1m"[..],
            true,
        ),
        ("rejected RGB", &b"\x1b[1;3;38;2;1;256;3m"[..], false),
    ] {
        let mut parameters = [Parameter::default(); MAX_PARAMETERS];
        let mut length = 0;
        Parser::default().advance(wire, |event| {
            if let Event::Csi(sequence) = event {
                length = sequence.parameters.len();
                parameters[..length].copy_from_slice(sequence.parameters);
            }
        });
        let mut style = Style::default();
        assert_eq!(style.apply_sgr(&parameters[..length]), accepted);
        measure(name, || {
            black_box(black_box(&mut style).apply_sgr(black_box(&parameters[..length])));
        });
    }
}
