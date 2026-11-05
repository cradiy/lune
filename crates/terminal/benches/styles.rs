use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::{Color, Style, StyleTable};

fn value(n: usize) -> Style {
    Style {
        foreground: Color::Rgb {
            red: n as u8,
            green: (n >> 8) as u8,
            blue: 17,
        },
        ..Style::default()
    }
}

fn measure(name: &str, units: usize, mut operation: impl FnMut()) {
    let mut samples = [0.0; 5];
    for sample in &mut samples {
        let start = Instant::now();
        let mut iterations = 0_u64;
        while start.elapsed() < Duration::from_millis(100) {
            for _ in 0..128 {
                operation();
            }
            iterations += 128;
        }
        *sample = start.elapsed().as_nanos() as f64 / (iterations as f64 * units as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!("{name:<26} {:>8.3} ns/style", samples[2]);
}

fn main() {
    for count in [64, 4096] {
        let mut table = StyleTable::new(count + 1).unwrap();
        let values: Vec<_> = (0..count).map(value).collect();
        let ids: Vec<_> = values.iter().map(|&s| table.intern(s).unwrap()).collect();
        let mut position = 0;
        measure(&format!("resolve / {count}"), 1, || {
            black_box(table.get(black_box(ids[position])).unwrap());
            position = (position + 1) % count;
        });
        measure(&format!("intern hit / {count}"), 1, || {
            black_box(table.intern(black_box(values[position])).unwrap());
            position = (position + 1) % count;
        });
        measure(&format!("trace all / {count}"), count, || {
            black_box(table.collect(black_box(ids.iter().copied())).unwrap());
        });
    }
    let mut table = StyleTable::new(1025).unwrap();
    let values: Vec<_> = (0..1024).map(value).collect();
    measure("intern + reclaim / 1024", values.len(), || {
        for &style in &values {
            black_box(table.intern(black_box(style)).unwrap());
        }
        black_box(table.collect([]).unwrap());
    });
}
