use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::{
    CellWidth, HistoryLimits, MAX_HISTORY_BYTES, Processor, Screen, StyleId, UnicodeWriter,
};

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

fn measure_stream(name: &str, input: &[u8], chunk: usize, enabled: bool) {
    let mut screen = Screen::new(120, 24).unwrap();
    if enabled {
        screen
            .set_history_limits(HistoryLimits {
                max_rows: 128,
                max_bytes: MAX_HISTORY_BYTES,
            })
            .unwrap();
    }
    let mut processor = Processor::default();
    let mut writer = UnicodeWriter::default();
    let mut samples = [0.0_f64; 3];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
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
            black_box(screen.history().retained_bytes());
            iterations += 1;
        }
        *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * input.len() as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "{name} / chunk={chunk} / history={enabled}: {:.3} ns/input byte",
        samples[1]
    );
}

fn main() {
    for columns in [120, 4096] {
        let mut screen = Screen::new(columns, 24).unwrap();
        measure(&format!("ordinary scroll / {columns}"), || {
            black_box(&mut screen).scroll_up(1, StyleId(0)).unwrap()
        });
        screen
            .set_history_limits(HistoryLimits {
                max_rows: 64,
                max_bytes: MAX_HISTORY_BYTES,
            })
            .unwrap();
        for _ in 0..64 {
            screen.scroll_up(1, StyleId(0)).unwrap();
        }
        measure(&format!("saturated capture / {columns}"), || {
            black_box(&mut screen).scroll_up(1, StyleId(0)).unwrap();
        });
        screen.scroll_viewport_up(32);
        measure(&format!("anchored capture / {columns}"), || {
            black_box(&mut screen).scroll_up(1, StyleId(0)).unwrap();
        });
        measure(&format!("viewport snapshot / {columns}"), || {
            black_box(black_box(&screen).viewport());
        });
        measure(&format!("viewport movement / {columns}"), || {
            screen.scroll_viewport_down(1);
            screen.scroll_viewport_up(1);
            black_box(screen.viewport_offset());
        });
        screen.scroll_viewport_to_bottom();
        measure(&format!("owned write + capture / {columns}"), || {
            screen.set_cursor(0, 0);
            screen
                .write(black_box("👩\u{200d}💻"), CellWidth::Double, StyleId(1))
                .unwrap();
            screen.scroll_up(1, StyleId(0)).unwrap();
        });
        measure(&format!("dense owned row + capture / {columns}"), || {
            screen.set_cursor(0, 0);
            for _ in 0..columns / 2 {
                screen
                    .write(black_box("👩\u{200d}💻"), CellWidth::Double, StyleId(1))
                    .unwrap();
            }
            screen.scroll_up(1, StyleId(0)).unwrap();
        });
    }
    for (name, text) in [
        (
            "ASCII wrap",
            "abcdefghijklmnopqrstuvwxyz0123456789".repeat(2048),
        ),
        (
            "Unicode + LF",
            "水e\u{301}❤\u{fe0f}👩\u{200d}💻\r\n".repeat(1024),
        ),
        ("controls", "\x1b[31mred\x1b[0mplain\r\n\x1bD".repeat(1024)),
    ] {
        for chunk in [text.len(), 17, 1] {
            for enabled in [false, true] {
                measure_stream(name, text.as_bytes(), chunk, enabled);
            }
        }
    }
}
