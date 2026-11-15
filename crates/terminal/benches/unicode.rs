#[path = "../tests/support/processor.rs"]
mod support;

use lune_terminal::{Screen, UnicodeWriter};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

fn measure(name: &str, input: &[u8], chunk: usize) {
    let mut processor = support::processor();
    let mut screen = Screen::new(120, 24).unwrap();
    let mut writer = UnicodeWriter::default();
    let mut samples = [0.0_f64; 3];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0;
        while start.elapsed() < Duration::from_millis(100) {
            for bytes in input.chunks(chunk) {
                processor
                    .advance(
                        black_box(bytes),
                        black_box(&mut screen),
                        black_box(&mut writer),
                    )
                    .unwrap();
            }
            processor.finish(&mut screen, &mut writer).unwrap();
            black_box(screen.cursor());
            iterations += 1;
        }
        *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * input.len() as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<32} {:>8.3} ns/input byte", samples[1]);
}

fn main() {
    for (name, text) in [
        (
            "ASCII",
            "abcdefghijklmnopqrstuvwxyz 0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ\r\n".repeat(1024),
        ),
        (
            "clusters",
            "水e\u{301}❤\u{fe0f}👩🏽\u{200d}💻🇺🇸1\u{fe0f}\u{20e3}\r\n".repeat(512),
        ),
        (
            "SGR + controls",
            "\x1b[31mred\x1b[0mplain\x1b[2;4Hword\x1b[K\r\n".repeat(1024),
        ),
        (
            "long cluster",
            String::from("e") + &"\u{301}".repeat(400) + "\r\n",
        ),
    ] {
        for chunk in [text.len(), 17, 1] {
            measure(&format!("{name} / chunk={chunk}"), text.as_bytes(), chunk);
        }
    }
}
