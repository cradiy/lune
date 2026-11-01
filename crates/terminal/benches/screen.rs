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

    for columns in [80, 240, 4096] {
        for count in [1, 16] {
            for extended in [false, true] {
                let mut screen = Screen::new(columns, 1).unwrap();
                for _ in 0..columns - count {
                    screen
                        .write(
                            if extended { "e\u{301}" } else { "x" },
                            CellWidth::Single,
                            StyleId(1),
                        )
                        .unwrap();
                }
                screen.set_cursor(0, columns / 4).unwrap();
                let kind = if extended { "extended" } else { "ASCII" };
                // The blank suffix absorbs insertion overflow; deletion restores
                // the original density, so each sample moves the same amount of text.
                measure(
                    &format!("screen {kind} edit / {columns}/{count}"),
                    2,
                    || {
                        let screen = black_box(&mut screen);
                        screen.insert_cells(black_box(count), StyleId(2));
                        screen.delete_cells(black_box(count), StyleId(2));
                    },
                );
            }
            let mut screen = Screen::new(columns, 1).unwrap();
            screen.set_cursor(0, columns / 4).unwrap();
            measure(&format!("screen ECH / {columns}/{count}"), 1, || {
                black_box(&mut screen).erase_cells(black_box(count), StyleId(2));
            });
        }
    }
    let edits: [fn(&mut Screen, usize, StyleId); 3] = [
        Screen::insert_cells,
        Screen::delete_cells,
        Screen::erase_cells,
    ];
    for (name, edit) in ["ICH", "DCH", "ECH"].into_iter().zip(edits) {
        measure_prepared(
            &format!("{name} populated extended / 240/16"),
            || {
                let mut screen = Screen::new(240, 1).unwrap();
                for _ in 0..240 {
                    screen
                        .write("e\u{301}", CellWidth::Single, StyleId(1))
                        .unwrap();
                }
                screen.set_cursor(0, 60).unwrap();
                screen
            },
            |screen| edit(screen, 16, StyleId(2)),
        );
    }

    let line_edits: [fn(&mut Screen, usize, StyleId); 2] =
        [Screen::insert_lines, Screen::delete_lines];
    for (name, edit) in ["IL", "DL"].into_iter().zip(line_edits) {
        let mut screen = Screen::new(120, 24).unwrap();
        screen.set_cursor(0, 17).unwrap();
        measure(&format!("{name} full / 24/1"), 1, || {
            edit(black_box(&mut screen), black_box(1), StyleId(2));
        });
        for rows in [24, 240, 4096] {
            for wrapped in [false, true] {
                let mut screen = Screen::new(120, rows).unwrap();
                screen.set_cursor(rows - 1, 0).unwrap();
                if wrapped {
                    for _ in 0..rows / 4 {
                        screen.line_feed(StyleId(0));
                    }
                }
                screen.set_scroll_region(1..rows - 1).unwrap();
                screen.set_cursor(rows / 2, 17).unwrap();
                for count in [1, 8] {
                    measure(
                        &format!("{name} / {rows}/{count}, wrapped={wrapped}"),
                        1,
                        || {
                            edit(black_box(&mut screen), black_box(count), StyleId(2));
                        },
                    );
                }
            }
        }
        for count in [1, 8] {
            measure_prepared(
                &format!("{name} extended / 240x24/{count}"),
                || {
                    let mut screen = Screen::new(240, 24).unwrap();
                    for row in 0..24 {
                        screen.set_cursor(row, 0).unwrap();
                        for _ in 0..240 {
                            screen
                                .write("e\u{301}", CellWidth::Single, StyleId(1))
                                .unwrap();
                        }
                    }
                    screen.set_scroll_region(1..23).unwrap();
                    screen.set_cursor(12, 17).unwrap();
                    screen
                },
                |screen| edit(screen, count, StyleId(2)),
            );
        }
    }
    for (columns, rows) in [(1, 1), (120, 24), (240, 4096)] {
        for regional in [false, true] {
            if regional && rows == 1 {
                continue;
            }
            for count in [0, 1, 8, usize::MAX] {
                let mut screen = Screen::new(columns, rows).unwrap();
                if regional {
                    screen.set_scroll_region(rows / 4..rows * 3 / 4).unwrap();
                }
                screen.set_cursor(rows / 2, columns - 1).unwrap();
                screen.write("x", CellWidth::Single, StyleId(1)).unwrap();
                measure(
                    &format!("relative / {columns}x{rows}, region={regional}, count={count}"),
                    4,
                    || {
                        black_box(&mut screen).move_cursor_up(black_box(count));
                        black_box(&mut screen).move_cursor_down(black_box(count));
                        black_box(&mut screen).move_cursor_left(black_box(count));
                        black_box(&mut screen).move_cursor_right(black_box(count));
                    },
                );
            }
        }
    }
}
