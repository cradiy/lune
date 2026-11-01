use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::{CellWidth, EraseMode, Grid, Screen, StyleId};

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

fn measure_prepared(
    name: &str,
    mut prepare: impl FnMut() -> Screen,
    mut operation: impl FnMut(&mut Screen),
) {
    let mut samples = [0.0_f64; 7];
    for sample in &mut samples {
        let mut screens: Vec<_> = (0..16).map(|_| prepare()).collect();
        let start = Instant::now();
        for screen in &mut screens {
            operation(black_box(screen));
        }
        *sample = start.elapsed().as_nanos() as f64 / screens.len() as f64;
        black_box(&screens);
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<36} {:>10.2} ns/unit", samples[3]);
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

    for rows in [24, 240, 4096] {
        for wrapped in [false, true] {
            let mut screen = Screen::new(120, rows).unwrap();
            screen.set_cursor(rows - 1, 0).unwrap();
            if wrapped {
                for _ in 0..rows / 2 {
                    screen.line_feed(StyleId(0));
                }
            }
            screen.set_scroll_region(1..rows - 1).unwrap();
            screen.set_cursor(rows - 2, 0).unwrap();
            measure(&format!("region LF / {rows}, wrapped={wrapped}"), 1, || {
                black_box(&mut screen).line_feed(StyleId(0));
            });
            let ascii = [b'x'; 240];
            measure(
                &format!("region ASCII / {rows}, wrapped={wrapped}"),
                ascii.len(),
                || {
                    black_box(&mut screen)
                        .write_ascii(black_box(&ascii), StyleId(1))
                        .unwrap();
                },
            );
            measure(
                &format!("region wide / {rows}, wrapped={wrapped}"),
                1,
                || {
                    let screen = black_box(&mut screen);
                    screen.set_cursor(rows - 2, 119).unwrap();
                    screen
                        .write(black_box("水"), CellWidth::Double, StyleId(1))
                        .unwrap();
                },
            );
            screen.set_cursor(1, 0).unwrap();
            measure(&format!("region RI / {rows}, wrapped={wrapped}"), 1, || {
                black_box(&mut screen).reverse_index(StyleId(0));
            });
        }
    }

    for (columns, rows) in [(120, 24), (120, 4096), (4096, 240)] {
        let mut screen = Screen::new(columns, rows).unwrap();
        screen.set_scroll_region(1..rows - 1).unwrap();
        screen.set_cursor(rows / 2, columns / 2).unwrap();
        for mode in [EraseMode::FromCursor, EraseMode::ToCursor, EraseMode::All] {
            measure(&format!("EL {mode:?} / {columns}x{rows}"), 1, || {
                black_box(&mut screen).erase_line(mode, StyleId(1));
            });
            measure(&format!("ED {mode:?} / {columns}x{rows}"), 1, || {
                black_box(&mut screen).erase_display(mode, StyleId(1));
            });
        }
    }

    for display in [false, true] {
        let name = if display {
            "ED populated extended / 240x24"
        } else {
            "EL populated extended / 240x24"
        };
        measure_prepared(
            name,
            || {
                let mut screen = Screen::new(240, 24).unwrap();
                let rows = if display { 0..24 } else { 12..13 };
                for row in rows {
                    screen.set_cursor(row, 0).unwrap();
                    for _ in 0..240 {
                        screen
                            .write("e\u{301}", CellWidth::Single, StyleId(1))
                            .unwrap();
                    }
                }
                screen.set_cursor(12, 120).unwrap();
                screen
            },
            |screen| {
                if display {
                    screen.erase_display(EraseMode::All, StyleId(2));
                } else {
                    screen.erase_line(EraseMode::All, StyleId(2));
                }
            },
        );
    }
}
