use crate::{Color, Palette, PaletteEntry, Rgb, Screen, Style, StyleTable};

fn entries() -> impl Iterator<Item = PaletteEntry> {
    (0..=u8::MAX).map(PaletteEntry::Indexed).chain([
        PaletteEntry::Foreground,
        PaletteEntry::Background,
        PaletteEntry::Cursor,
    ])
}

fn colors(salt: u8) -> [Rgb; 259] {
    std::array::from_fn(|index| Rgb {
        red: index as u8 ^ salt,
        green: (index >> 8) as u8 ^ salt.rotate_left(1),
        blue: (index as u8).rotate_left(2) ^ salt,
    })
}

fn palette(base: &[Rgb; 259]) -> Palette {
    Palette::new(
        base[..256].try_into().unwrap(),
        base[256],
        base[257],
        base[258],
    )
}

fn rebase(palette: &mut Palette, base: &[Rgb; 259]) -> bool {
    palette.set_base(
        base[..256].try_into().unwrap(),
        base[256],
        base[257],
        base[258],
    )
}

#[test]
fn equal_overrides_survive_theme_changes_and_resets_restore_the_latest_base() {
    let old = colors(1);
    let next = colors(2);
    let mut palette = palette(&old);
    for (index, entry) in entries().enumerate() {
        assert!(!palette.set_override(entry, old[index]));
        assert!(palette.is_overridden(entry));
    }
    assert!(!rebase(&mut palette, &next));
    for (index, entry) in entries().enumerate() {
        assert_eq!(palette.color(entry), old[index]);
        assert_eq!(palette.base_color(entry), next[index]);
    }
    assert!(palette.reset_indexed());
    for (index, entry) in entries().enumerate() {
        assert_eq!(
            palette.color(entry),
            if index < 256 { next[index] } else { old[index] }
        );
        assert_eq!(palette.is_overridden(entry), index >= 256);
    }
    assert!(palette.reset(PaletteEntry::Foreground));
    assert!(palette.reset_all());
    assert!(!palette.reset_all());
    for (index, entry) in entries().enumerate() {
        assert_eq!(palette.color(entry), next[index]);
        assert!(!palette.is_overridden(entry));
    }
    assert!(!rebase(&mut palette, &next));
    assert!(rebase(&mut palette, &old));
}

#[test]
fn palette_mutations_match_an_independent_optional_override_model() {
    let mut base = colors(0);
    let mut overrides = [None; 259];
    let mut palette = palette(&base);
    let mut seed = 1_u64;
    let entries: Vec<_> = entries().collect();
    for _ in 0..3000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let index = (seed >> 32) as usize % entries.len();
        let salt = (seed >> 24) as u8;
        let before: [Rgb; 259] = std::array::from_fn(|i| overrides[i].unwrap_or(base[i]));
        let changed = match seed % 6 {
            0 | 1 => {
                let color = if seed.is_multiple_of(6) {
                    base[index]
                } else {
                    colors(salt)[index]
                };
                overrides[index] = Some(color);
                palette.set_override(entries[index], color)
            }
            2 => {
                overrides[index] = None;
                palette.reset(entries[index])
            }
            3 => {
                overrides[..256].fill(None);
                palette.reset_indexed()
            }
            4 => {
                overrides.fill(None);
                palette.reset_all()
            }
            _ => {
                base = colors(salt);
                rebase(&mut palette, &base)
            }
        };
        let after: [Rgb; 259] = std::array::from_fn(|i| overrides[i].unwrap_or(base[i]));
        assert_eq!(changed, before != after);
        for (index, &entry) in entries.iter().enumerate() {
            assert_eq!(palette.color(entry), after[index]);
            assert_eq!(palette.base_color(entry), base[index]);
            assert_eq!(palette.is_overridden(entry), overrides[index].is_some());
        }
    }
}

#[test]
fn late_color_resolution_updates_existing_styles_without_mutating_cells() {
    let mut palette = palette(&colors(1));
    let mut styles = StyleTable::new(4).unwrap();
    let style = Style {
        foreground: Color::Indexed(255),
        background: Color::Default,
        bold: true,
        inverse: true,
        ..Style::default()
    };
    let id = styles.intern(style).unwrap();
    let mut screen = Screen::new(4, 2).unwrap();
    screen.write_ascii(b"text", id).unwrap();
    let before = super::snapshot(screen.grid());
    let rgb = Rgb {
        red: 12,
        green: 34,
        blue: 56,
    };
    assert!(palette.set_override(PaletteEntry::Indexed(255), rgb));
    assert!(rebase(&mut palette, &colors(3)));
    assert_eq!(
        palette.resolve(style.foreground, PaletteEntry::Foreground),
        rgb
    );
    assert_eq!(
        palette.resolve(style.background, PaletteEntry::Background),
        colors(3)[257]
    );
    for entry in entries() {
        assert_eq!(palette.resolve(Color::Default, entry), palette.color(entry));
        assert_eq!(
            palette.resolve(
                Color::Rgb {
                    red: 12,
                    green: 34,
                    blue: 56
                },
                entry
            ),
            rgb
        );
    }
    assert_eq!(styles.intern(style).unwrap(), id);
    assert_eq!(styles.get(id), Some(&style));
    assert_eq!(super::snapshot(screen.grid()), before);
}
