use super::support;
use crate::{PaletteEntry, ProcessFailure, Processor, Rgb, Screen, StyleId};

const ROLES: [PaletteEntry; 3] = [
    PaletteEntry::Foreground,
    PaletteEntry::Background,
    PaletteEntry::Cursor,
];

fn text(_: &mut Screen, _: &str, _: StyleId) -> Result<(), ()> {
    Ok(())
}

fn feed(processor: &mut Processor, screen: &mut Screen, wire: &[u8], chunk: usize) -> Vec<u8> {
    let mut output = Vec::new();
    for mut input in wire.chunks(chunk) {
        loop {
            let result = processor.advance(input, screen, &mut text);
            while !processor.reply_bytes().is_empty() {
                let count = processor.reply_bytes().len().min(2);
                output.extend_from_slice(&processor.reply_bytes()[..count]);
                assert!(processor.consume_reply(count));
            }
            match result {
                Ok(count) => {
                    assert_eq!(count, input.len());
                    break;
                }
                Err(error) => {
                    assert!(matches!(error.error, ProcessFailure::ReplyPending));
                    input = &input[error.consumed..];
                }
            }
        }
    }
    output
}

fn reply(selector: usize, rgb: Rgb, terminator: &str) -> Vec<u8> {
    format!(
        "\x1b]{selector};rgb:{:04x}/{:04x}/{:04x}{terminator}",
        u16::from(rgb.red) * 257,
        u16::from(rgb.green) * 257,
        u16::from(rgb.blue) * 257
    )
    .into_bytes()
}

fn indexed_reply(index: u8, rgb: Rgb, terminator: &str) -> Vec<u8> {
    format!(
        "\x1b]4;{index};rgb:{:04x}/{:04x}/{:04x}{terminator}",
        u16::from(rgb.red) * 257,
        u16::from(rgb.green) * 257,
        u16::from(rgb.blue) * 257
    )
    .into_bytes()
}

#[test]
fn indexed_batches_cover_the_full_table_and_reset_only_indexed_overrides() {
    let values: [Rgb; 256] = std::array::from_fn(|index| Rgb {
        red: index as u8 ^ 128,
        green: 56,
        blue: 78,
    });
    for terminator in ["\x07", "\x1b\\"] {
        let mut set = String::from("\x1b]4");
        let mut query = String::from("\x1b]4");
        let mut reset = String::from("\x1b]104");
        let mut expected = Vec::new();
        for (index, value) in values.into_iter().enumerate() {
            use std::fmt::Write;
            write!(
                &mut set,
                ";{index};rgb:{:04x}/{:04x}/{:04x}",
                u16::from(value.red) * 257,
                u16::from(value.green) * 257,
                u16::from(value.blue) * 257
            )
            .unwrap();
            write!(&mut query, ";{index};?").unwrap();
            write!(&mut reset, ";{index}").unwrap();
            expected.extend(indexed_reply(index as u8, value, terminator));
        }
        assert!(set.len() > crate::MAX_TITLE_BYTES);
        for chunk in [1, 17, usize::MAX] {
            let mut processor = support::processor();
            let mut screen = Screen::new(2, 2).unwrap();
            feed(&mut processor, &mut screen, b"\x1b]10;#abcdef\x07", chunk);
            processor.take_palette_change();
            let old = std::array::from_fn::<_, 256, _>(|index| {
                processor
                    .palette()
                    .color(PaletteEntry::Indexed(index as u8))
            });
            feed(&mut processor, &mut screen, set.as_bytes(), chunk);
            assert_eq!(
                std::array::from_fn::<_, 256, _>(|index| processor
                    .palette()
                    .color(PaletteEntry::Indexed(index as u8))),
                old
            );
            feed(&mut processor, &mut screen, terminator.as_bytes(), chunk);
            assert_eq!(
                std::array::from_fn::<_, 256, _>(|index| processor
                    .palette()
                    .color(PaletteEntry::Indexed(index as u8))),
                values
            );
            assert!(processor.take_palette_change());
            let wire = format!("{query}{terminator}");
            assert_eq!(
                feed(&mut processor, &mut screen, wire.as_bytes(), chunk),
                expected
            );
            assert!(!processor.take_palette_change());
            let wire = format!("{reset}{terminator}");
            feed(&mut processor, &mut screen, wire.as_bytes(), chunk);
            assert_eq!(
                std::array::from_fn::<_, 256, _>(|index| processor
                    .palette()
                    .color(PaletteEntry::Indexed(index as u8))),
                old
            );
            assert!(processor.take_palette_change());
            assert!(processor.palette().is_overridden(PaletteEntry::Foreground));
            feed(
                &mut processor,
                &mut screen,
                b"\x1b]4;1;#123456;255;#abcdef\x07\x1b]104\x07",
                chunk,
            );
            assert_eq!(
                std::array::from_fn::<_, 256, _>(|index| processor
                    .palette()
                    .color(PaletteEntry::Indexed(index as u8))),
                old
            );
            assert!(processor.palette().is_overridden(PaletteEntry::Foreground));
            assert_eq!(
                (processor.parse_errors(), processor.ignored_sequences()),
                (0, 0)
            );
        }
    }
}

#[test]
fn malformed_oversized_and_cancelled_batches_have_no_partial_effects() {
    let mut invalid: Vec<Vec<u8>> = [
        "4;1;#112233;2;invalid",
        "4;1;?;256;#123456",
        "4;1;#123456;2",
        "4;;#123",
        "4;0001;#fff",
        "4;1;",
        "4;1;?;",
        "104;1;256",
        "104;",
        "104;1;",
    ]
    .into_iter()
    .map(|value| format!("\x1b]{value}\x07").into_bytes())
    .collect();
    invalid.push(
        format!(
            "\x1b]4{}\x07",
            ";1;?".repeat(crate::MAX_OSC_PALETTE_ENTRIES + 1)
        )
        .into_bytes(),
    );
    invalid.push(
        format!(
            "\x1b]4{}\x07",
            ";1;#123456".repeat(crate::MAX_OSC_PALETTE_ENTRIES + 1)
        )
        .into_bytes(),
    );
    invalid.push(
        format!(
            "\x1b]104{}\x07",
            ";1".repeat(crate::MAX_OSC_PALETTE_ENTRIES + 1)
        )
        .into_bytes(),
    );
    for chunk in [1, 17, usize::MAX] {
        let mut processor = support::processor();
        let mut screen = Screen::new(2, 2).unwrap();
        feed(
            &mut processor,
            &mut screen,
            b"\x1b]2;retained\x07\x1b]4;1;#abcdef\x07",
            chunk,
        );
        processor.take_palette_change();
        let old = processor.palette().color(PaletteEntry::Indexed(1));
        for (index, wire) in invalid.iter().enumerate() {
            assert!(feed(&mut processor, &mut screen, wire, chunk).is_empty());
            assert_eq!(processor.palette().color(PaletteEntry::Indexed(1)), old);
            assert!(!processor.take_palette_change());
            assert_eq!(processor.ignored_sequences(), index as u64 + 1);
        }
        for ending in ["\x18", "\x1a", "\x1b[0m"] {
            let wire = format!("\x1b]4;1;#123456;1;?{ending}");
            assert!(feed(&mut processor, &mut screen, wire.as_bytes(), chunk).is_empty());
            assert_eq!(processor.palette().color(PaletteEntry::Indexed(1)), old);
        }
        feed(
            &mut processor,
            &mut screen,
            b"\x1b]4;1;#123456;1;?\x1b",
            chunk,
        );
        processor.finish(&mut screen, &mut text).unwrap();
        assert_eq!(processor.parse_errors(), 1);
        assert_eq!(processor.palette().color(PaletteEntry::Indexed(1)), old);
        assert!(processor.reply_bytes().is_empty());
        assert_eq!(processor.window_title(), "retained");
        feed(&mut processor, &mut screen, b"\x1b]104;1\x07", chunk);
        assert_eq!(
            processor.palette().color(PaletteEntry::Indexed(1)),
            processor.palette().base_color(PaletteEntry::Indexed(1))
        );
    }
}

#[test]
fn batch_replies_capture_duplicate_indices_in_order_and_block_following_input() {
    let mut processor = support::processor();
    let mut screen = Screen::new(2, 2).unwrap();
    let old1 = processor.palette().color(PaletteEntry::Indexed(1));
    let old255 = processor.palette().color(PaletteEntry::Indexed(255));
    let first = Rgb {
        red: 0x12,
        green: 0x34,
        blue: 0x56,
    };
    let last = Rgb {
        red: 0xab,
        green: 0xcd,
        blue: 0xef,
    };
    let wire = b"\x1b[5n\x1b]4;1;?;1;#123456;1;?;255;?;1;#abcdef;1;?\x1b\\TAIL";
    let calls = std::cell::Cell::new(0);
    let mut sink = |_: &mut Screen, value: &str, _: StyleId| {
        calls.set(calls.get() + value.len());
        Ok::<_, ()>(())
    };
    let error = processor.advance(wire, &mut screen, &mut sink).unwrap_err();
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    assert_eq!(error.consumed, wire.len() - 4);
    assert_eq!(calls.get(), 0);
    assert_eq!(processor.palette().color(PaletteEntry::Indexed(1)), last);
    let mut base = std::array::from_fn(|index| {
        processor
            .palette()
            .base_color(PaletteEntry::Indexed(index as u8))
    });
    base[1] = Rgb {
        red: 8,
        green: 9,
        blue: 10,
    };
    base[255] = base[1];
    let roles = ROLES.map(|entry| processor.palette().base_color(entry));
    processor.set_palette_base(base, roles[0], roles[1], roles[2]);
    assert_eq!(processor.reply_bytes(), b"\x1b[0n");
    let again = processor
        .advance(&wire[error.consumed..], &mut screen, &mut sink)
        .unwrap_err();
    assert_eq!(again.consumed, 0);
    assert!(processor.consume_reply(4));
    let expected = [(1, old1), (1, first), (255, old255), (1, last)];
    for (position, (index, color)) in expected.into_iter().enumerate() {
        let result = processor.advance(&wire[error.consumed..], &mut screen, &mut sink);
        if position < 3 {
            let error = result.unwrap_err();
            assert!(matches!(error.error, ProcessFailure::ReplyPending));
            assert_eq!(error.consumed, 0);
            assert_eq!(calls.get(), 0);
        } else {
            assert_eq!(result.unwrap(), 4);
            assert_eq!(calls.get(), 4);
        }
        let expected = indexed_reply(index, color, "\x1b\\");
        assert_eq!(processor.reply_bytes(), expected);
        assert!(processor.consume_reply(1));
        if position == 3 {
            processor.advance(b"\x1bc", &mut screen, &mut sink).unwrap();
            assert_eq!(processor.reply_bytes(), &expected[1..]);
        }
        assert!(processor.consume_reply(processor.reply_bytes().len()));
    }
    assert_eq!(processor.palette().color(PaletteEntry::Indexed(1)), base[1]);
    let error = processor
        .advance(b"\x1b]4;1;?;255;?\x07", &mut screen, &mut sink)
        .unwrap_err();
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    processor.reset(&mut screen, &mut sink);
    assert_eq!(processor.advance(&[], &mut screen, &mut sink).unwrap(), 0);
    assert!(processor.reply_bytes().is_empty());
    assert_eq!(
        feed(&mut processor, &mut screen, b"\x1b]4;1;?\x07", 1),
        indexed_reply(1, base[1], "\x07")
    );
}

#[test]
fn numeric_color_forms_commit_atomically_and_queries_preserve_wire_terminators() {
    let cases = [
        (
            "#123",
            Rgb {
                red: 0x10,
                green: 0x20,
                blue: 0x30,
            },
        ),
        (
            "#123456",
            Rgb {
                red: 0x12,
                green: 0x34,
                blue: 0x56,
            },
        ),
        (
            "#123456789",
            Rgb {
                red: 0x12,
                green: 0x45,
                blue: 0x78,
            },
        ),
        (
            "#123456789abc",
            Rgb {
                red: 0x12,
                green: 0x56,
                blue: 0x9a,
            },
        ),
        (
            "RGB:f/80/1234",
            Rgb {
                red: 255,
                green: 128,
                blue: 0x12,
            },
        ),
        (
            "rgb:1/2/3",
            Rgb {
                red: 0x11,
                green: 0x22,
                blue: 0x33,
            },
        ),
        (
            "rgb:ffff/0000/8080",
            Rgb {
                red: 255,
                green: 0,
                blue: 128,
            },
        ),
    ];
    for chunk in [1, 2, 17, usize::MAX] {
        for terminator in ["\x07", "\x1b\\"] {
            let mut processor = support::processor();
            let mut screen = Screen::new(2, 2).unwrap();
            for (spec, rgb) in cases {
                for (index, role) in ROLES.into_iter().enumerate() {
                    let selector = index + 10;
                    let old = processor.palette().color(role);
                    let wire = format!("\x1b]{selector};{spec}");
                    feed(&mut processor, &mut screen, wire.as_bytes(), chunk);
                    assert_eq!(processor.palette().color(role), old);
                    feed(&mut processor, &mut screen, terminator.as_bytes(), chunk);
                    assert_eq!(processor.palette().color(role), rgb);
                    assert!(processor.take_palette_change());
                    assert!(!processor.take_palette_change());
                    let wire = format!("\x1b]{selector};?{terminator}");
                    assert_eq!(
                        feed(&mut processor, &mut screen, wire.as_bytes(), chunk),
                        reply(selector, rgb, terminator)
                    );
                    assert!(!processor.take_palette_change());
                    let wire = format!("\x1b]{}{terminator}", selector + 100);
                    feed(&mut processor, &mut screen, wire.as_bytes(), chunk);
                    assert_eq!(
                        processor.palette().color(role),
                        processor.palette().base_color(role)
                    );
                    assert!(processor.take_palette_change());
                }
            }
            assert_eq!(
                (processor.parse_errors(), processor.ignored_sequences()),
                (0, 0)
            );
        }
    }
}

#[test]
fn dynamic_chains_commit_supported_suffixes_and_query_each_role_across_chunks() {
    let specs = ["#112233", "rgb:4444/5555/6666", "#778899"];
    let values = [
        Rgb {
            red: 0x11,
            green: 0x22,
            blue: 0x33,
        },
        Rgb {
            red: 0x44,
            green: 0x55,
            blue: 0x66,
        },
        Rgb {
            red: 0x77,
            green: 0x88,
            blue: 0x99,
        },
    ];
    for start in 0..3 {
        for terminator in ["\x07", "\x1b\\"] {
            for chunk in [1, 17, usize::MAX] {
                let mut processor = support::processor();
                let mut screen = Screen::new(2, 2).unwrap();
                let base = ROLES.map(|role| processor.palette().color(role));
                let body = format!("\x1b]{};{}", start + 10, specs[start..].join(";"));
                assert!(feed(&mut processor, &mut screen, body.as_bytes(), chunk).is_empty());
                assert_eq!(ROLES.map(|role| processor.palette().color(role)), base);
                assert!(!processor.take_palette_change());
                feed(&mut processor, &mut screen, terminator.as_bytes(), chunk);
                let mut expected = base;
                expected[start..].copy_from_slice(&values[start..]);
                assert_eq!(ROLES.map(|role| processor.palette().color(role)), expected);
                assert!(processor.take_palette_change());
                let queries = vec!["?"; 3 - start].join(";");
                let wire = format!("\x1b]{};{queries}{terminator}", start + 10);
                let expected: Vec<u8> = (start..3)
                    .flat_map(|role| reply(role + 10, values[role], terminator))
                    .collect();
                assert_eq!(
                    feed(&mut processor, &mut screen, wire.as_bytes(), chunk),
                    expected
                );
                assert!(!processor.take_palette_change());
                let mixed = format!("\x1b]10;?;#abcdef;?{terminator}");
                let expected: Vec<u8> = [0, 2]
                    .into_iter()
                    .flat_map(|role| {
                        reply(
                            role + 10,
                            processor.palette().color(ROLES[role]),
                            terminator,
                        )
                    })
                    .collect();
                assert_eq!(
                    feed(&mut processor, &mut screen, mixed.as_bytes(), chunk),
                    expected
                );
                assert_eq!(
                    processor.palette().color(PaletteEntry::Background),
                    Rgb {
                        red: 0xab,
                        green: 0xcd,
                        blue: 0xef
                    }
                );
                assert!(processor.take_palette_change());
                assert_eq!(
                    (processor.parse_errors(), processor.ignored_sequences()),
                    (0, 0)
                );
            }
        }
    }
}

#[test]
fn rejected_cancelled_and_truncated_colors_preserve_titles_and_the_palette() {
    let invalid = [
        "10;",
        "10;rgb:/0/0",
        "11;#1234",
        "12;rgb:10000/0/0",
        "10;#ffffff;invalid",
        "10;?;;?",
        "10;#112233;?;#445566;?",
        "11;?;#abcdef;?",
        "12;#ffffff;?",
        "10;?;",
        "110;",
        "110;bad",
        "113",
        "10;#ff00gg",
        "10;rgb:ffff/ffff/ffffx",
        "10;rgb:1/2/3/4",
    ];
    for chunk in [1, 17, usize::MAX] {
        let mut processor = support::processor();
        let mut screen = Screen::new(2, 2).unwrap();
        feed(&mut processor, &mut screen, b"\x1b]2;retained\x07", chunk);
        processor.take_window_title_change();
        let base = ROLES.map(|role| processor.palette().color(role));
        for (index, body) in invalid.into_iter().enumerate() {
            let wire = format!("\x1b]{body}\x1b\\");
            assert!(feed(&mut processor, &mut screen, wire.as_bytes(), chunk).is_empty());
            assert_eq!(ROLES.map(|role| processor.palette().color(role)), base);
            assert!(!processor.take_palette_change());
            assert_eq!(processor.ignored_sequences(), index as u64 + 1);
        }
        for terminator in ["\x18", "\x1a", "\x1b[0m"] {
            let wire = format!("\x1b]10;?;#112233;?{terminator}");
            assert!(feed(&mut processor, &mut screen, wire.as_bytes(), chunk).is_empty());
            assert_eq!(ROLES.map(|role| processor.palette().color(role)), base);
        }
        feed(
            &mut processor,
            &mut screen,
            b"\x1b]10;?;#abcdef;rgb:ffff/ffff/ffff\x1b",
            chunk,
        );
        processor.finish(&mut screen, &mut text).unwrap();
        processor.finish(&mut screen, &mut text).unwrap();
        assert_eq!(processor.parse_errors(), 1);
        assert_eq!(ROLES.map(|role| processor.palette().color(role)), base);
        assert_eq!(processor.window_title(), "retained");
        assert_eq!(processor.take_window_title_change(), None);
        assert!(!processor.take_palette_change());
    }
}

#[test]
fn captured_color_queries_survive_backpressure_theme_changes_and_resets() {
    let mut processor = support::processor();
    let mut screen = Screen::new(2, 2).unwrap();
    let originals = ROLES.map(|role| processor.palette().color(role));
    let original = originals[0];
    let wire = b"\x1b[5n\x1b]10;?;?;?\x1b\\";
    let error = processor.advance(wire, &mut screen, &mut text).unwrap_err();
    assert_eq!(error.consumed, wire.len());
    assert!(matches!(error.error, ProcessFailure::ReplyPending));
    let next = Rgb {
        red: 200,
        green: 201,
        blue: 202,
    };
    let indexed = std::array::from_fn(|index| {
        processor
            .palette()
            .base_color(PaletteEntry::Indexed(index as u8))
    });
    let background = Rgb {
        red: 210,
        green: 211,
        blue: 212,
    };
    let cursor = Rgb {
        red: 220,
        green: 221,
        blue: 222,
    };
    assert!(processor.set_palette_base(indexed, next, background, cursor));
    assert!(processor.take_palette_change());
    assert_eq!(processor.reply_bytes(), b"\x1b[0n");
    assert!(processor.consume_reply(4));
    for (index, rgb) in originals.into_iter().enumerate() {
        let result = processor.advance(&[], &mut screen, &mut text);
        if index < 2 {
            assert!(matches!(
                result.unwrap_err().error,
                ProcessFailure::ReplyPending
            ));
        } else {
            assert_eq!(result.unwrap(), 0);
        }
        let expected = reply(index + 10, rgb, "\x1b\\");
        assert_eq!(processor.reply_bytes(), expected);
        assert!(processor.consume_reply(3));
        if index == 2 {
            processor.advance(b"\x1bc", &mut screen, &mut text).unwrap();
            assert_eq!(processor.reply_bytes(), &expected[3..]);
        }
        assert!(processor.consume_reply(processor.reply_bytes().len()));
    }
    assert_eq!(processor.palette().color(PaletteEntry::Foreground), next);
    feed(&mut processor, &mut screen, b"\x1b]10;#c8c9ca\x07", 1);
    assert!(!processor.take_palette_change());
    assert!(processor.palette().is_overridden(PaletteEntry::Foreground));
    assert!(!processor.set_palette_base(indexed, original, background, cursor));
    assert!(!processor.take_palette_change());
    feed(&mut processor, &mut screen, b"\x1b]110\x07", 1);
    assert_eq!(
        processor.palette().color(PaletteEntry::Foreground),
        original
    );
    assert!(processor.take_palette_change());
    feed(
        &mut processor,
        &mut screen,
        b"\x1b]11;#123456;#abcdef\x07",
        17,
    );
    processor.take_palette_change();
    assert!(matches!(
        processor
            .advance(b"\x1b]10;?;?;?\x07", &mut screen, &mut text)
            .unwrap_err()
            .error,
        ProcessFailure::ReplyPending
    ));
    processor.reset(&mut screen, &mut text);
    assert_eq!(
        processor.palette().color(PaletteEntry::Background),
        background
    );
    assert_eq!(processor.palette().color(PaletteEntry::Cursor), cursor);
    assert!(processor.take_palette_change());
    assert!(processor.reply_bytes().is_empty());
    assert_eq!(processor.advance(&[], &mut screen, &mut text).unwrap(), 0);
    assert!(processor.reply_bytes().is_empty());
    assert_eq!(processor.ignored_sequences(), 0);
}
