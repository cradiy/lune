#![no_main]

use std::collections::HashMap;

use libfuzzer_sys::fuzz_target;
use lune_terminal::{Color, Style, StyleError, StyleId, StyleTable, Underline};

fn color(selector: u8, red: u8, green: u8, blue: u8) -> Color {
    match selector % 3 {
        0 => Color::Default,
        1 => Color::Indexed(red),
        _ => Color::Rgb { red, green, blue },
    }
}

fuzz_target!(|data: &[u8]| {
    let limit = data.first().copied().unwrap_or(0) as usize % 16 + 1;
    let mut table = StyleTable::new(limit).unwrap();
    let mut live = HashMap::from([(StyleId(0), Style::default())]);
    let mut issued = Vec::new();
    for bytes in data.as_chunks::<4>().0.iter().take(128) {
        match bytes[0] % 3 {
            0 => {
                let style = Style {
                    foreground: color(bytes[2], bytes[1], bytes[2], bytes[3]),
                    background: color(bytes[3], bytes[3], bytes[2], bytes[1]),
                    bold: bytes[1] & 1 != 0,
                    faint: bytes[1] & 2 != 0,
                    italic: bytes[1] & 4 != 0,
                    blink: bytes[1] & 8 != 0,
                    inverse: bytes[1] & 16 != 0,
                    hidden: bytes[1] & 32 != 0,
                    strikethrough: bytes[1] & 64 != 0,
                    underline: match bytes[1] % 3 {
                        0 => Underline::None,
                        1 => Underline::Single,
                        _ => Underline::Double,
                    },
                };
                let existing = live.iter().find_map(|(&id, &s)| (s == style).then_some(id));
                match table.intern(style) {
                    Ok(id) => {
                        if let Some(expected) = existing {
                            assert_eq!(id, expected);
                        } else {
                            assert!(live.len() < limit);
                            assert!(!issued.contains(&id));
                            issued.push(id);
                            live.insert(id, style);
                        }
                    }
                    Err(StyleError::CapacityExceeded) => {
                        assert_eq!(existing, None);
                        assert_eq!(live.len(), limit);
                    }
                    Err(error) => panic!("Unexpected error: {error}"),
                }
            }
            1 => {
                let mut roots = vec![StyleId(0)];
                for (index, &id) in issued.iter().enumerate() {
                    if live.contains_key(&id) && index % 4 == bytes[1] as usize % 4 {
                        roots.extend([id, id]);
                    }
                }
                if bytes[2] & 1 != 0 {
                    roots.push(StyleId(1 << 16));
                }
                let invalid = roots.iter().copied().find(|id| !live.contains_key(id));
                let result = table.collect(roots.iter().copied());
                if let Some(invalid) = invalid {
                    assert!(matches!(result, Err(StyleError::InvalidId(id)) if id == invalid));
                } else {
                    let before = live.len();
                    live.retain(|id, _| roots.contains(id));
                    assert_eq!(result.unwrap(), before - live.len());
                }
            }
            _ => {
                let id = issued
                    .get(bytes[1] as usize % issued.len().max(1))
                    .copied()
                    .unwrap_or(StyleId(u32::from_le_bytes(*bytes)));
                assert_eq!(table.get(id), live.get(&id));
            }
        }
        assert_eq!(table.style_count(), live.len());
        assert_eq!(table.get(StyleId(0)), live.get(&StyleId(0)));
        for &id in &issued {
            assert_eq!(table.get(id), live.get(&id));
        }
    }
});
