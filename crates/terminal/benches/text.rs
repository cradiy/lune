use std::{
    fmt::Write,
    hint::black_box,
    ops::Range,
    time::{Duration, Instant},
};

use lune_terminal::{
    CellPosition, CellWidth, HistoryLimits, MAX_HISTORY_BYTES, Screen, ScreenView, StyleId,
};

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
    history_views();
    logical_lines();
}

fn measure_lines(name: &str, rows: usize, mut action: impl FnMut() -> usize) {
    let mut samples = [0.0_f64; 3];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(100) {
            for _ in 0..16 {
                black_box(action());
            }
            iterations += 16;
        }
        *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * rows as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<54} {:>8.3} ns/physical row", samples[1]);
}

fn scan_lines(view: ScreenView<'_>) -> usize {
    let mut start = 0;
    let mut sum = 0;
    for (index, row) in view.iter_rows().enumerate() {
        if row.wrap_end().is_none() || index + 1 == view.rows() {
            sum ^= start * 31 + index + 1;
            start = index + 1;
        }
    }
    sum
}

fn logical_lines() {
    for depth in [0, 4096] {
        for group in [1, 8, 0] {
            let mut screen = Screen::new(120, 24).unwrap();
            screen
                .set_history_limits(HistoryLimits {
                    max_rows: depth,
                    max_bytes: MAX_HISTORY_BYTES,
                })
                .unwrap();
            let rows = depth + 24;
            for row in 0..rows {
                screen.write_ascii(&[b'x'; 120], StyleId(0)).unwrap();
                if row + 1 < rows && group != 0 && (row + 1) % group == 0 {
                    screen.line_feed(StyleId(0)).unwrap();
                    screen.carriage_return();
                }
            }
            let view = screen.view();
            let checksum = |line: lune_terminal::LogicalLine<'_>| {
                let range = line.range();
                range.start * 31 + range.end
            };
            let expected = scan_lines(view);
            assert_eq!(
                view.logical_lines()
                    .fold(0, |sum, line| sum ^ checksum(line)),
                expected
            );
            assert_eq!(
                view.logical_lines()
                    .rev()
                    .fold(0, |sum, line| sum ^ checksum(line)),
                expected
            );
            measure_lines(
                &format!("row scan / history={depth} / group={group}"),
                rows,
                || scan_lines(black_box(view)),
            );
            measure_lines(
                &format!("forward / history={depth} / group={group}"),
                rows,
                || {
                    black_box(view)
                        .logical_lines()
                        .fold(0, |sum, line| sum ^ checksum(line))
                },
            );
            measure_lines(
                &format!("reverse / history={depth} / group={group}"),
                rows,
                || {
                    black_box(view)
                        .logical_lines()
                        .rev()
                        .fold(0, |sum, line| sum ^ checksum(line))
                },
            );
            let row = rows / 2;
            let count = view.logical_line(row).unwrap().range().len();
            measure_lines(
                &format!("lookup / history={depth} / group={group}"),
                count,
                || checksum(black_box(view).logical_line(black_box(row)).unwrap()),
            );
        }
    }
}

fn measure_screen(name: &str, screen: &Screen, range: Range<CellPosition>) {
    let bytes = screen.view().text(range.clone()).unwrap().to_string().len();
    let mut output = String::with_capacity(bytes);
    let mut samples = [0.0_f64; 3];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(100) {
            for _ in 0..16 {
                output.clear();
                let text = black_box(screen)
                    .view()
                    .text(black_box(range.clone()))
                    .unwrap();
                write!(black_box(&mut output), "{}", black_box(text)).unwrap();
                black_box(&output);
            }
            iterations += 16;
        }
        *sample = start.elapsed().as_nanos() as f64 / iterations as f64;
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<36} {:>9.2} ns/selection", samples[1]);
}

fn history_views() {
    for depth in [0, 4096] {
        let mut screen = Screen::new(120, 24).unwrap();
        screen
            .set_history_limits(HistoryLimits {
                max_rows: depth,
                max_bytes: MAX_HISTORY_BYTES,
            })
            .unwrap();
        for _ in 0..depth + 24 {
            screen.write_ascii(&[b'x'; 120], StyleId(0)).unwrap();
        }
        let row = if depth == 0 { 12 } else { depth / 2 };
        measure_screen(
            &format!("eight columns / history={depth}"),
            &screen,
            CellPosition { row, column: 100 }..CellPosition { row, column: 108 },
        );
        let row = screen.view().rows() - 1;
        measure_screen(
            &format!("visible tail / history={depth}"),
            &screen,
            CellPosition { row, column: 100 }..CellPosition { row, column: 108 },
        );
        if depth != 0 {
            measure_screen(
                "history-to-screen join",
                &screen,
                CellPosition {
                    row: depth - 1,
                    column: 119,
                }..CellPosition {
                    row: depth,
                    column: 1,
                },
            );
            measure_screen(
                "full history and screen",
                &screen,
                CellPosition { row: 0, column: 0 }..CellPosition {
                    row: depth + 23,
                    column: 120,
                },
            );
        }
    }
}
