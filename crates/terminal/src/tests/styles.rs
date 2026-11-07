use crate::{Color, MAX_STYLES, Screen, Style, StyleError, StyleId, StyleTable};

#[test]
fn style_identity_distinguishes_attribute_combinations_and_color_channels() {
    let mut table = StyleTable::new(4096).unwrap();
    let mut values = std::collections::HashMap::new();
    let mut ids = std::collections::HashSet::new();
    let mut record = |style| {
        let id = table.intern(style).unwrap();
        if let Some(expected) = values.insert(style, id) {
            assert_eq!(id, expected);
        } else {
            assert!(ids.insert(id));
        }
        assert_eq!(table.get(id), Some(&style));
    };
    for flags in 0..128 {
        for underline in [
            crate::Underline::None,
            crate::Underline::Single,
            crate::Underline::Double,
        ] {
            record(Style {
                bold: flags & 1 != 0,
                faint: flags & 2 != 0,
                italic: flags & 4 != 0,
                blink: flags & 8 != 0,
                inverse: flags & 16 != 0,
                hidden: flags & 32 != 0,
                strikethrough: flags & 64 != 0,
                underline,
                ..Style::default()
            });
        }
    }
    for component in 0..=255 {
        for color in [
            Color::Indexed(component),
            Color::Rgb {
                red: component,
                green: 0,
                blue: 0,
            },
            Color::Rgb {
                red: 0,
                green: component,
                blue: 0,
            },
            Color::Rgb {
                red: 0,
                green: 0,
                blue: component,
            },
        ] {
            record(Style {
                foreground: color,
                ..Style::default()
            });
            record(Style {
                background: color,
                ..Style::default()
            });
        }
    }
    assert_eq!(table.style_count(), values.len());
}

const BOLD: Style = Style {
    foreground: Color::Default,
    background: Color::Default,
    bold: true,
    faint: false,
    italic: false,
    underline: crate::Underline::None,
    blink: false,
    inverse: false,
    hidden: false,
    strikethrough: false,
};

#[test]
fn bounded_interning_and_collection_preserve_live_ids_and_reject_invalid_roots() {
    for limit in [0, MAX_STYLES + 1, usize::MAX] {
        assert!(matches!(
            StyleTable::new(limit),
            Err(StyleError::InvalidLimit)
        ));
    }
    let mut table = StyleTable::new(3).unwrap();
    let default = table.intern(Style::default()).unwrap();
    assert_eq!(default, StyleId(0));
    let bold = table.intern(BOLD).unwrap();
    let red_value = Style {
        foreground: Color::Indexed(1),
        ..Style::default()
    };
    let red = table.intern(red_value).unwrap();
    assert_eq!(table.intern(BOLD).unwrap(), bold);
    assert_eq!(table.style_count(), 3);
    assert!(matches!(
        table.intern(Style {
            italic: true,
            ..Style::default()
        }),
        Err(StyleError::CapacityExceeded)
    ));
    assert_eq!(table.collect([default, bold, bold]).unwrap(), 1);
    assert_eq!(table.get(bold), Some(&BOLD));
    assert_eq!(table.get(red), None);
    let new_red = table.intern(red_value).unwrap();
    assert_ne!(new_red, red);
    assert_eq!(table.get(new_red), Some(&red_value));
    assert_eq!(table.get(red), None);
    assert!(matches!(table.collect([bold, red]), Err(StyleError::InvalidId(id)) if id == red));
    assert_eq!(table.style_count(), 3);
    assert_eq!(table.get(bold), Some(&BOLD));
    assert_eq!(table.get(new_red), Some(&red_value));
    assert_eq!(table.collect([]).unwrap(), 2);
    assert_eq!(table.style_count(), 1);
    assert_eq!(table.get(default), Some(&Style::default()));
    assert_eq!(table.get(StyleId(0x10000)), None);
    assert_eq!(table.get(StyleId(u32::MAX)), None);
}

#[test]
fn tracing_cells_and_external_owners_preserves_only_reachable_styles() {
    let mut table = StyleTable::new(8).unwrap();
    let red_value = Style {
        foreground: Color::Indexed(1),
        ..Style::default()
    };
    let blue_value = Style {
        background: Color::Indexed(4),
        ..Style::default()
    };
    let red = table.intern(red_value).unwrap();
    let blue = table.intern(blue_value).unwrap();
    let external = table.intern(BOLD).unwrap();
    let orphan = table
        .intern(Style {
            hidden: true,
            ..Style::default()
        })
        .unwrap();
    let mut screen = Screen::new(8, 4).unwrap();
    screen.write_ascii(b"red", red).unwrap();
    screen.line_feed(StyleId(0)).unwrap();
    screen.write_ascii(b"blue", blue).unwrap();
    let cells = || {
        screen
            .grid()
            .iter_rows()
            .flat_map(|row| row.cells().iter().map(|c| c.style()))
    };
    assert_eq!(table.collect(cells().chain([external])).unwrap(), 1);
    assert_eq!(table.get(orphan), None);
    assert_eq!(table.get(red), Some(&red_value));
    assert_eq!(table.get(blue), Some(&blue_value));
    assert_eq!(table.get(external), Some(&BOLD));
    screen.reset(StyleId(0));
    assert_eq!(table.collect([external]).unwrap(), 2);
    assert_eq!(table.get(red), None);
    assert_eq!(table.get(blue), None);
    assert_eq!(table.get(external), Some(&BOLD));
    assert_eq!(table.collect([]).unwrap(), 1);
    assert_eq!(table.style_count(), 1);
}

#[test]
fn generation_exhaustion_retires_a_slot_without_reviving_any_old_id() {
    let mut table = StyleTable::new(2).unwrap();
    let mut previous = StyleId(0);
    for _ in 0..=u16::MAX {
        let id = table.intern(BOLD).unwrap();
        assert!(id.0 > previous.0);
        assert_eq!(table.get(id), Some(&BOLD));
        assert_eq!(table.intern(BOLD).unwrap(), id);
        assert_eq!(table.collect([]).unwrap(), 1);
        assert_eq!(table.get(id), None);
        previous = id;
    }
    assert!(matches!(
        table.intern(BOLD),
        Err(StyleError::CapacityExceeded)
    ));
    assert_eq!(table.get(StyleId(1)), None);
    assert_eq!(table.style_count(), 1);
    assert_eq!(table.intern(Style::default()).unwrap(), StyleId(0));
}
