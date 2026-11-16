#[path = "../tests/support/processor.rs"]
mod support;

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::{CellWidth, ProcessFailure, Screen, StorageError, StyleId};

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
    let mut processor = support::processor();
    let mut samples = [0.0_f64; 3];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(150) {
            for bytes in input.chunks(chunk) {
                processor
                    .advance(black_box(bytes), black_box(&mut screen), &mut text)
                    .unwrap();
            }
            processor.finish(black_box(&mut screen), &mut text).unwrap();
            black_box(screen.cursor());
            black_box(&processor);
            iterations += 1;
        }
        *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * input.len() as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<26} {:>8.3} ns/input byte", samples[1]);
}

fn measure_replies(name: &str, input: &[u8], chunk: usize, write_chunk: usize) {
    let mut screen = Screen::new(120, 24).unwrap();
    let mut processor = support::processor();
    let mut samples = [0.0_f64; 3];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(150) {
            for mut bytes in input.chunks(chunk) {
                loop {
                    let result =
                        processor.advance(black_box(bytes), black_box(&mut screen), &mut text);
                    while !processor.reply_bytes().is_empty() {
                        let count = processor.reply_bytes().len().min(write_chunk);
                        black_box(&processor.reply_bytes()[..count]);
                        assert!(processor.consume_reply(count));
                    }
                    match result {
                        Ok(consumed) => {
                            assert_eq!(consumed, bytes.len());
                            break;
                        }
                        Err(error) => {
                            assert!(matches!(error.error, ProcessFailure::ReplyPending));
                            bytes = &bytes[error.consumed..];
                        }
                    }
                }
            }
            processor.finish(black_box(&mut screen), &mut text).unwrap();
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
    let titles = "\x1b]2;synthetic one\x07\x1b]0;synthetic 🌙;two\x1b\\"
        .as_bytes()
        .repeat(1024);
    let rendition = b"\x1b[1;31mred\x1b[38;2;0;128;255;48:5:17mblue\x1b[0mplain\r\n".repeat(1024);
    let graphics =
        b"\x1b)0\x0elqqqqqqqqqqqqk\r\nx            x\r\nmqqqqqqqqqqqqj\x0f plain\r\n".repeat(1024);
    let cursor = b"\x1b[?25l\x1b[6 q\x1b[?12htext\x1b[?25h\x1b[ q\r\n".repeat(1024);
    let focus = b"\x1b[?1004;2004h\x1b[?1004;2004l".repeat(1024);
    let mouse = b"\x1b[?1000;1006h\x1b[?1002h\x1b[?1003h\x1b[?1006l\x1b[?1000l".repeat(1024);
    let queries = b"row\r\n\x1b[5n\x1b[6n\x1b[2;8H\x1b[6n\x1b[?6h\x1b[6n\x1b[?6l".repeat(1024);
    let color_queries =
        b"\x1b]10;#123456\x07\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]12;?\x1b\\\x1b]110\x07".repeat(512);
    let indexed = b"\x1b]4;1;?;1;#123456;1;?;255;?\x1b\\\x1b]104;1;255\x07".repeat(512);
    let mode_queries = b"\x1b[4h\x1b[4$p\x1b[4l\x1b[4$p\x1b[?1004;1003;1006h\x1b[?1004$p\x1b[?1003$p\x1b[?65535$p\x1b[?1004;1003;1006l".repeat(512);
    let alternate = b"\x1b[31mprimary\r\n\x1b[?1049h\x1b[34m\x1b[2;3Halternate\r\n\x1b[?1048h\x1b[?47l\x1b[?47h\x1b[?1048l\x1b[?1049l\x1b[0m".repeat(512);
    let mut newlines = Vec::from(&b"\x1b[20h"[..]);
    newlines
        .extend(b"abcdefghijklmnopqrstuvwxyz 0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ\n".repeat(1024));
    newlines.extend(b"\x1b[20l");
    measure("ASCII bulk", &ascii, ascii.len());
    measure("ASCII / 17-byte", &ascii, 17);
    measure("ASCII / 1-byte", &ascii, 1);
    measure("scalar width fixture", unicode.as_bytes(), unicode.len());
    measure("controls", &controls, controls.len());
    measure("control strings", &strings, strings.len());
    measure("title updates / bulk", &titles, titles.len());
    measure("title updates / 17-byte", &titles, 17);
    measure("title updates / 1-byte", &titles, 1);
    measure("SGR / bulk", &rendition, rendition.len());
    measure("SGR / 17-byte", &rendition, 17);
    measure("SGR / 1-byte", &rendition, 1);
    measure("DEC graphics / bulk", &graphics, graphics.len());
    measure("DEC graphics / 1-byte", &graphics, 1);
    measure("new-line mode / bulk", &newlines, newlines.len());
    measure("new-line mode / 1-byte", &newlines, 1);
    measure("cursor state / bulk", &cursor, cursor.len());
    measure("cursor state / 1-byte", &cursor, 1);
    measure("focus mode / bulk", &focus, focus.len());
    measure("focus mode / 1-byte", &focus, 1);
    measure("mouse modes / bulk", &mouse, mouse.len());
    measure("mouse modes / 1-byte", &mouse, 1);
    measure_replies("queries / bulk", &queries, queries.len(), 14);
    measure_replies("queries / 17-byte", &queries, 17, 3);
    measure_replies("queries / 1-byte", &queries, 1, 1);
    measure_replies(
        "color queries / bulk",
        &color_queries,
        color_queries.len(),
        25,
    );
    measure_replies("color queries / 17-byte", &color_queries, 17, 3);
    measure_replies("color queries / 1-byte", &color_queries, 1, 1);
    measure_replies("indexed batches / bulk", &indexed, indexed.len(), 28);
    measure_replies("indexed batches / 17-byte", &indexed, 17, 3);
    measure_replies("indexed batches / 1-byte", &indexed, 1, 1);
    measure_replies("mode queries / bulk", &mode_queries, mode_queries.len(), 14);
    measure_replies("mode queries / 17-byte", &mode_queries, 17, 3);
    measure_replies("mode queries / 1-byte", &mode_queries, 1, 1);
    measure("alternate / bulk", &alternate, alternate.len());
    measure("alternate / 17-byte", &alternate, 17);
    measure("alternate / 1-byte", &alternate, 1);
}
