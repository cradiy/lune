use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::{CellWidth, Processor, Screen, StorageError, StyleId};

fn text(screen: &mut Screen, value: &str, style: StyleId) -> Result<(), StorageError> {
    if value.is_ascii() {
        screen.write_ascii(value.as_bytes(), style)
    } else {
        let width = if matches!(value, "水" | "🌙") {
            CellWidth::Double
        } else {
            CellWidth::Single
        };
        screen.write(value, width, style)
    }
}

fn measure(name: &str, input: &[u8], chunk: usize) {
    let mut screen = Screen::new(120, 24).unwrap();
    let mut processor = Processor::default();
    let mut samples = [0.0_f64; 3];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(150) {
            for bytes in input.chunks(chunk) {
                processor
                    .advance(black_box(bytes), black_box(&mut screen), text)
                    .unwrap();
            }
            processor.finish(black_box(&mut screen), text).unwrap();
            black_box(screen.cursor());
            iterations += 1;
        }
        *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * input.len() as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<26} {:>8.3} ns/input byte", samples[1]);
}

fn main() {
    let ascii =
        b"abcdefghijklmnopqrstuvwxyz 0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ\r\n".repeat(1024);
    let unicode = "水🌙é hello\r\n".repeat(2048);
    let controls = b"\x1b[2;4Hword\x1b[0K\x1b[2A\x1b7\x1b[4hXX\x1b[4l\x1b8\r\n".repeat(1024);
    let strings = b"\x1b]0;synthetic title\x07\x1bPsynthetic payload\x1b\\".repeat(2048);
    measure("ASCII bulk", &ascii, ascii.len());
    measure("ASCII / 17-byte", &ascii, 17);
    measure("ASCII / 1-byte", &ascii, 1);
    measure("scalar width fixture", unicode.as_bytes(), unicode.len());
    measure("controls", &controls, controls.len());
    measure("discarded strings", &strings, strings.len());
}
