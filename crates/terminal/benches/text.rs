use std::{
    fmt::Write,
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::{CellPosition, CellWidth, Screen, StyleId};

fn main() {
    let long = "e\u{301}".repeat(341);
    for (name, text, width, soft, partial) in [
        ("ASCII hard rows", "x", CellWidth::Single, false, false),
        ("ASCII soft rows", "x", CellWidth::Single, true, false),
        ("UTF-8 scalar", "é", CellWidth::Single, true, false),
        ("wide scalar", "水", CellWidth::Double, true, false),
        (
            "short owned text",
            "e\u{301}",
            CellWidth::Single,
            true,
            false,
        ),
        (
            "emoji sequence",
            "👩\u{200d}💻",
            CellWidth::Double,
            true,
            false,
        ),
        (
            "long owned text",
            long.as_str(),
            CellWidth::Single,
            true,
            false,
        ),
        (
            "partial owned row",
            "e\u{301}",
            CellWidth::Single,
            true,
            true,
        ),
    ] {
        let mut screen = Screen::new(240, 24).unwrap();
        screen.set_auto_wrap(soft);
        let step = if width == CellWidth::Single { 1 } else { 2 };
        for row in 0..24 {
            if !soft {
                screen.set_cursor(row, 0);
            }
            for _ in (0..240).step_by(step) {
                screen.write(text, width, StyleId(1)).unwrap();
            }
        }
        let range = if partial {
            CellPosition {
                row: 12,
                column: 200,
            }..CellPosition {
                row: 12,
                column: 208,
            }
        } else {
            CellPosition { row: 0, column: 0 }..CellPosition {
                row: 23,
                column: 240,
            }
        };
        let view = screen.grid().text(range).unwrap();
        let bytes = view.to_string().len();
        let mut output = String::with_capacity(bytes);
        let mut samples = [0.0_f64; 3];
        for sample in &mut samples {
            let start = Instant::now();
            let mut iterations = 0_u64;
            while start.elapsed() < Duration::from_millis(100) {
                for _ in 0..16 {
                    output.clear();
                    write!(black_box(&mut output), "{}", black_box(view)).unwrap();
                    black_box(&output);
                }
                iterations += 16;
            }
            *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * bytes as f64);
        }
        samples.sort_by(f64::total_cmp);
        println!("{name:<24} {:>8.3} ns/output byte", samples[1]);
    }
}
