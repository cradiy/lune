use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::{CellWidth, Grid, Screen, StyleId};

fn measure(name: &str, units: usize, mut operation: impl FnMut()) {
    let mut samples = [0.0_f64; 3];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(200) {
            for _ in 0..64 {
                operation();
            }
            iterations += 64;
        }
        *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * units as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<36} {:>10.2} ns/unit", samples[1]);
}

fn main() {
    let mut grid = Grid::new(240, 80).unwrap();
    measure("grid scalar / cell", 240, || {
        for column in 0..240 {
            black_box(&mut grid)
                .write(0, column, black_box("x"), CellWidth::Single, StyleId(1))
                .unwrap();
        }
    });
    let mut screen = Screen::new(240, 80).unwrap();
    measure("screen scalar + wrap / cell", 240, || {
        for _ in 0..240 {
            black_box(&mut screen)
                .write(black_box("x"), CellWidth::Single, StyleId(1))
                .unwrap();
        }
    });
    for rows in [1, 24, 4096] {
        let mut screen = Screen::new(120, rows).unwrap();
        screen.set_cursor(rows - 1, 0).unwrap();
        let ascii = [b'x'; 240];
        measure(
            &format!("ASCII + scroll / cell, {rows} rows"),
            ascii.len(),
            || {
                black_box(&mut screen)
                    .write_ascii(black_box(&ascii), StyleId(1))
                    .unwrap();
            },
        );
    }
    let mut screen = Screen::new(120, 24).unwrap();
    screen.set_cursor(23, 0).unwrap();
    measure("line feed + scroll", 1, || {
        black_box(&mut screen).line_feed(StyleId(0));
    });
    measure("wide edge + scroll + placement", 1, || {
        let screen = black_box(&mut screen);
        screen.set_cursor(23, 119).unwrap();
        screen
            .write(black_box("水"), CellWidth::Double, StyleId(1))
            .unwrap();
    });
}
