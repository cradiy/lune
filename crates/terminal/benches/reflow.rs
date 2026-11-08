use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::{
    CellPosition, CellWidth, HistoryLimits, LogicalLine, MAX_HISTORY_BYTES, ReflowAffinity,
    ReflowAnchor, Screen, StyleId,
};

fn measure(name: &str, line: LogicalLine<'_>, columns: usize) {
    let source_columns: usize = line
        .iter_rows()
        .map(|row| row.wrap_end().unwrap_or(row.cells().len()))
        .sum();
    assert_eq!(
        line.reflow(columns)
            .unwrap()
            .map(|row| row.used_columns())
            .sum::<usize>(),
        source_columns
    );
    let mut samples = [0.0_f64; 3];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(100) {
            for _ in 0..16 {
                let mut sum = 0;
                for row in black_box(line).reflow(black_box(columns)).unwrap() {
                    let end = row.source_range().end;
                    sum ^= row.used_columns() + end.row + end.column;
                }
                black_box(sum);
            }
            iterations += 16;
        }
        *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * source_columns as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "{name:<48} columns={columns:<3} {:>8.3} ns/source column",
        samples[1]
    );
}

fn measure_owned(name: &str, line: LogicalLine<'_>, columns: usize) {
    let source_columns: usize = line
        .iter_rows()
        .map(|row| row.wrap_end().unwrap_or(row.cells().len()))
        .sum();
    let mut samples = [0.0_f64; 3];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(100) {
            for row in black_box(line).reflow(black_box(columns)).unwrap() {
                let owned = row.materialize(StyleId(9)).unwrap();
                black_box(&owned);
            }
            iterations += 1;
        }
        *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * source_columns as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "materialize {name:<36} columns={columns:<3} {:>8.3} ns/source column",
        samples[1]
    );
}

fn stream_positions(
    line: LogicalLine<'_>,
    columns: usize,
    points: [ReflowAnchor<'_>; 3],
) -> [Option<CellPosition>; 3] {
    let mut result = [None; 3];
    let mut next = 0;
    for (index, row) in line.reflow(columns).unwrap().enumerate() {
        while next < points.len() {
            let Some(column) = row.map_anchor(points[next]).unwrap() else {
                break;
            };
            if row.wrap_end().is_some() && column == row.used_columns() {
                break;
            }
            result[next] = Some(CellPosition { row: index, column });
            next += 1;
        }
        if next == points.len() {
            break;
        }
    }
    result
}

fn time_mapping(name: &str, mut action: impl FnMut() -> usize) {
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
        *sample = start.elapsed().as_nanos() as f64 / iterations as f64;
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<62} {:>10.2} ns/query or batch", samples[1]);
}

fn measure_mapping(name: &str, line: LogicalLine<'_>, columns: usize) {
    let range = line.range();
    let points = [range.start, range.start + range.len() / 2, range.end - 1]
        .map(|row| CellPosition { row, column: 0 });
    let expected = points.map(|point| {
        Some(
            line.map_position(columns, point, ReflowAffinity::After)
                .unwrap(),
        )
    });
    let anchors = points.map(|point| line.anchor(point).unwrap());
    assert_eq!(stream_positions(line, columns, anchors), expected);
    for (label, point) in ["first", "middle", "last"].into_iter().zip(points) {
        time_mapping(&format!("map {label} / {name} / columns={columns}"), || {
            let result = black_box(line)
                .map_position(black_box(columns), black_box(point), ReflowAffinity::After)
                .unwrap();
            result.row ^ result.column
        });
    }
    time_mapping(
        &format!("map three prepared anchors / {name} / columns={columns}"),
        || {
            stream_positions(black_box(line), black_box(columns), black_box(anchors))
                .into_iter()
                .flatten()
                .fold(0, |sum, point| sum ^ point.row ^ point.column)
        },
    );
}

fn source(name: &str, depth: usize, long: &str) -> Screen {
    let mut screen = Screen::new(119, 24).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: depth,
            max_bytes: MAX_HISTORY_BYTES,
        })
        .unwrap();
    let units = if name == "CJK" { 59 } else { 119 };
    for index in 0..(depth + 24) * units {
        let (text, width) = match name {
            "ASCII" => ("x", CellWidth::Single),
            "CJK" => ("水", CellWidth::Double),
            "owned" => ("e\u{301}", CellWidth::Single),
            "long cluster" => (long, CellWidth::Single),
            _ => match index % 4 {
                0 => ("x", CellWidth::Single),
                1 => ("水", CellWidth::Double),
                2 => ("e\u{301}", CellWidth::Single),
                _ => ("👩\u{200d}💻", CellWidth::Double),
            },
        };
        screen
            .write(text, width, StyleId(index as u32 % 4))
            .unwrap();
    }
    screen
}

fn measure_transaction(name: &str, depth: usize, long: &str, columns: usize) {
    let mut samples = [0.0_f64; 5];
    for sample in &mut samples {
        let mut screen = source(name, depth, long);
        screen.scroll_viewport_up(depth / 2);
        let start = Instant::now();
        screen.reflow(black_box(columns), 24, StyleId(9)).unwrap();
        *sample = start.elapsed().as_nanos() as f64;
        black_box(&screen);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "transaction {name:<12} history={depth:<4} columns={columns:<3} {:>10.2} us/resize",
        samples[2] / 1000.0
    );
}

fn main() {
    let long = String::from("e") + &"\u{301}".repeat(511);
    for (name, depth) in [
        ("ASCII", 0),
        ("ASCII", 4096),
        ("CJK", 0),
        ("CJK", 4096),
        ("owned", 0),
        ("owned", 4096),
        ("mixed", 4096),
        ("long cluster", 0),
    ] {
        let screen = source(name, depth, &long);
        let line = screen.view().logical_line(0).unwrap();
        if name == "ASCII" {
            measure(&format!("{name} / history={depth}"), line, 1);
        }
        for columns in [2, 80, 240] {
            measure(&format!("{name} / history={depth}"), line, columns);
            measure_owned(&format!("{name} / history={depth}"), line, columns);
            if name == "ASCII" || name == "mixed" {
                measure_mapping(&format!("{name} / history={depth}"), line, columns);
            }
        }
        for columns in [2, 80, 240] {
            measure_transaction(name, depth, &long, columns);
        }
    }
}
