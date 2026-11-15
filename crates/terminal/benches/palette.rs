use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use lune_terminal::{Color, Palette, PaletteEntry, Rgb};

fn measure(name: &str, units: usize, mut operation: impl FnMut()) {
    let mut samples = [0.0_f64; 5];
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
    println!("{name:<28} {:>8.3} ns/operation", samples[2]);
}

fn main() {
    let indexed = std::array::from_fn(|index| Rgb {
        red: index as u8,
        green: 12,
        blue: 34,
    });
    let mut palette = Palette::new(indexed, indexed[7], indexed[0], indexed[15]);
    let mixed: [Color; 1024] = std::array::from_fn(|index| match index % 4 {
        0 => Color::Default,
        1 | 2 => Color::Indexed(index as u8),
        _ => Color::Rgb {
            red: index as u8,
            green: 56,
            blue: 78,
        },
    });
    let mut index = 0_u8;
    measure("resolve indexed", 1, || {
        black_box(palette.resolve(Color::Indexed(black_box(index)), PaletteEntry::Foreground));
        index = index.wrapping_add(1);
    });
    for overridden in [false, true] {
        if overridden {
            for index in (0..=u8::MAX).step_by(2) {
                palette.set_override(PaletteEntry::Indexed(index), indexed[255 - index as usize]);
            }
        }
        let mut position = 0;
        measure(
            if overridden {
                "resolve mixed / overrides"
            } else {
                "resolve mixed / base"
            },
            1,
            || {
                let default = if position & 1 == 0 {
                    PaletteEntry::Foreground
                } else {
                    PaletteEntry::Background
                };
                black_box(palette.resolve(black_box(mixed[position]), default));
                position = (position + 1) % mixed.len();
            },
        );
    }
    palette.reset_all();
    for count in [8, 256] {
        measure(&format!("override/reset / {count}"), count + 1, || {
            for index in 0..count {
                black_box(palette.set_override(
                    PaletteEntry::Indexed(index as u8),
                    indexed[(index + 1) % 256],
                ));
            }
            black_box(palette.reset_all());
        });
    }
    let mut updated = indexed;
    measure("replace theme / 259 colors", 1, || {
        updated[0].red ^= 1;
        black_box(palette.set_base(black_box(updated), updated[7], updated[0], updated[15]));
    });
}
