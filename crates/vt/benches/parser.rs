use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_vt::{Event, Parser};

fn measure(name: &str, input: &[u8], chunk_size: usize) {
    let mut samples = [0.0_f64; 3];

    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        let mut parser = Parser::default();
        let mut emit = |event: Event<'_>| {
            black_box(event);
        };

        while start.elapsed() < Duration::from_millis(200) {
            for chunk in black_box(input).chunks(chunk_size) {
                parser.advance(chunk, &mut emit);
            }
            parser.finish(&mut emit);
            iterations += 1;
        }

        *sample = input.len() as f64 * iterations as f64
            / start.elapsed().as_secs_f64()
            / (1024.0 * 1024.0);
    }

    samples.sort_by(f64::total_cmp);
    let nanos_per_byte = 1e9 / (samples[1] * 1024.0 * 1024.0);
    println!(
        "{name:<28} {:>10.1} MiB/s  {nanos_per_byte:>6.2} ns/B",
        samples[1]
    );
}

fn main() {
    let ascii =
        b"abcdefghijklmnopqrstuvwxyz 0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ\r\n".repeat(1024);
    let unicode = "终端解析 🌙 cafe\u{301} Ελληνικά\r\n"
        .repeat(2048)
        .into_bytes();
    let csi = b"\x1b[?25l\x1b[38:2::12:34:56mtext\x1b[0m\r\n\x1b[12;40H".repeat(2048);
    let strings = b"\x1b]0;synthetic title\x07\x1bPsynthetic payload\x1b\\".repeat(2048);
    let dcs = b"\x1bP?1:2;;0$qsynthetic\x00\x07\n\xc2\x9c\x7f\xff\x1b\\".repeat(2048);

    measure("ascii", &ascii, ascii.len());
    measure("utf8", &unicode, unicode.len());
    measure("csi", &csi, csi.len());
    measure("csi / 17-byte", &csi, 17);
    measure("csi / 1-byte", &csi, 1);
    measure("DCS / bulk", &dcs, dcs.len());
    measure("DCS / 17-byte", &dcs, 17);
    measure("DCS / 1-byte", &dcs, 1);
    measure("control strings", &strings, strings.len());
    measure("control strings / 17-byte", &strings, 17);
    measure("control strings / 1-byte", &strings, 1);
    measure("ascii / 17-byte", &ascii, 17);
    measure("ascii / 1-byte", &ascii, 1);
    measure("utf8 / 1-byte", &unicode, 1);

    for size in [1024, 65536, 1048576] {
        let invalid_utf8 = b"\xf4\x90\x80\x80".repeat(size / 4);
        measure(
            &format!("invalid utf8 / {size}"),
            &invalid_utf8,
            invalid_utf8.len(),
        );

        let mut parameter = b"\x1b[".to_vec();
        parameter.extend(std::iter::repeat_n(b'0', size));
        parameter.push(b'm');
        measure(
            &format!("long parameter / {size}"),
            &parameter,
            parameter.len(),
        );

        let mut string = b"\x1b]".to_vec();
        string.extend(std::iter::repeat_n(b'x', size));
        measure(&format!("open string / {size}"), &string, string.len());
    }
}
