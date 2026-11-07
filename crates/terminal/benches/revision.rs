use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::{CellPosition, CellWidth, Screen, StyleId};

fn measure(name: &str, mut operation: impl FnMut()) {
    let mut samples = [0.0_f64; 3];
    for sample in &mut samples {
        let start = Instant::now();
        let mut count = 0_u64;
        while start.elapsed() < Duration::from_millis(150) {
            for _ in 0..128 {
                operation();
            }
            count += 128;
        }
        *sample = start.elapsed().as_nanos() as f64 / count as f64;
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<32} {:>9.2} ns/update", samples[1]);
}

fn main() {
    for insert in [false, true] {
        let mut screen = Screen::new(120, 24).unwrap();
        screen.write_ascii(&[b'x'; 120], StyleId(1)).unwrap();
        screen.set_insert_mode(insert);
        let position = CellPosition { row: 0, column: 40 };
        screen
            .revise_unit(position, "e\u{301}", CellWidth::Single, StyleId(2))
            .unwrap();
        measure(&format!("same width / insert={insert}"), || {
            black_box(&mut screen)
                .revise_unit(
                    black_box(position),
                    black_box("e\u{301}"),
                    CellWidth::Single,
                    StyleId(2),
                )
                .unwrap();
        });
        let mut width = CellWidth::Single;
        measure(&format!("width change / insert={insert}"), || {
            width = if width == CellWidth::Single {
                CellWidth::Double
            } else {
                CellWidth::Single
            };
            black_box(&mut screen)
                .revise_unit(
                    black_box(position),
                    black_box("❤\u{fe0f}"),
                    width,
                    StyleId(2),
                )
                .unwrap();
        });
    }
    let mut screen = Screen::new(120, 24).unwrap();
    measure("edge write + growing scroll", || {
        screen.set_cursor(23, 119);
        screen.write("❤", CellWidth::Single, StyleId(2)).unwrap();
        black_box(&mut screen)
            .revise_unit(
                CellPosition {
                    row: 23,
                    column: 119,
                },
                black_box("❤\u{fe0f}"),
                CellWidth::Double,
                StyleId(2),
            )
            .unwrap();
    });
}
