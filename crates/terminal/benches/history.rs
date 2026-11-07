use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::{CellWidth, HistoryLimits, MAX_HISTORY_BYTES, RowHistory, Screen, StyleId};

fn measure(name: &str, mut operation: impl FnMut()) {
    let mut samples = [0.0_f64; 3];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(150) {
            for _ in 0..128 {
                operation();
            }
            iterations += 128;
        }
        *sample = start.elapsed().as_nanos() as f64 / iterations as f64;
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<36} {:>9.2} ns/operation", samples[1]);
}

fn main() {
    for columns in [120, 4096] {
        let mut screen = Screen::new(columns, 24).unwrap();
        measure(&format!("ordinary scroll / {columns}"), || {
            black_box(&mut screen).scroll_up(1, StyleId(0))
        });
        let mut disabled = RowHistory::default();
        measure(&format!("disabled history / {columns}"), || {
            black_box(&mut disabled)
                .scroll_up(black_box(&mut screen), StyleId(0))
                .unwrap();
        });
        let mut history = RowHistory::new(HistoryLimits {
            max_rows: 64,
            max_bytes: MAX_HISTORY_BYTES,
        })
        .unwrap();
        for _ in 0..64 {
            history.scroll_up(&mut screen, StyleId(0)).unwrap();
        }
        measure(&format!("saturated capture / {columns}"), || {
            black_box(&mut history)
                .scroll_up(black_box(&mut screen), StyleId(0))
                .unwrap();
        });
        measure(&format!("owned write + capture / {columns}"), || {
            screen.set_cursor(0, 0);
            screen
                .write(black_box("👩\u{200d}💻"), CellWidth::Double, StyleId(1))
                .unwrap();
            history
                .scroll_up(black_box(&mut screen), StyleId(0))
                .unwrap();
        });
        measure(&format!("dense owned row + capture / {columns}"), || {
            screen.set_cursor(0, 0);
            for _ in 0..columns / 2 {
                screen
                    .write(black_box("👩\u{200d}💻"), CellWidth::Double, StyleId(1))
                    .unwrap();
            }
            history
                .scroll_up(black_box(&mut screen), StyleId(0))
                .unwrap();
        });
    }
}
