//! Synthetic processor configuration for tests, fuzzing, and benchmarks.

use lune_terminal::{MAX_STYLES, Palette, Processor, Rgb};

pub fn palette() -> Palette {
    let colors = std::array::from_fn(|index| Rgb {
        red: index as u8,
        green: 12,
        blue: 34,
    });
    Palette::new(colors, colors[7], colors[0], colors[15])
}

pub fn processor() -> Processor {
    Processor::new(MAX_STYLES, palette()).unwrap()
}
