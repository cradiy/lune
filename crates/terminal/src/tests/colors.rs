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
fn rejected_cancelled_and_truncated_colors_preserve_titles_and_the_palette() {
    let invalid = [
        "10;",
        "10;rgb:/0/0",
        "11;#1234",
        "12;rgb:10000/0/0",
        "10;#ffffff;#000000",
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
            let wire = format!("\x1b]10;#112233{terminator}");
            assert!(feed(&mut processor, &mut screen, wire.as_bytes(), chunk).is_empty());
            assert_eq!(ROLES.map(|role| processor.palette().color(role)), base);
        }
        feed(
            &mut processor,
            &mut screen,
            b"\x1b]12;rgb:ffff/ffff/ffff\x1b",
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
    let original = processor.palette().color(PaletteEntry::Foreground);
    let wire = b"\x1b[5n\x1b]10;?\x1b\\";
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
    let background = processor.palette().base_color(PaletteEntry::Background);
    let cursor = processor.palette().base_color(PaletteEntry::Cursor);
    assert!(processor.set_palette_base(indexed, next, background, cursor));
    assert!(processor.take_palette_change());
    assert_eq!(processor.reply_bytes(), b"\x1b[0n");
    assert!(processor.consume_reply(4));
    processor.advance(&[], &mut screen, &mut text).unwrap();
    let expected = reply(10, original, "\x1b\\");
    assert_eq!(processor.reply_bytes(), expected);
    assert!(processor.consume_reply(3));
    processor.advance(b"\x1bc", &mut screen, &mut text).unwrap();
    assert_eq!(processor.reply_bytes(), &expected[3..]);
    assert_eq!(processor.palette().color(PaletteEntry::Foreground), next);
    assert!(processor.consume_reply(processor.reply_bytes().len()));
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
        b"\x1b]11;#123456\x07\x1b]12;#abcdef\x07",
        17,
    );
    processor.take_palette_change();
    processor.reset(&mut screen, &mut text);
    assert_eq!(
        processor.palette().color(PaletteEntry::Background),
        background
    );
    assert_eq!(processor.palette().color(PaletteEntry::Cursor), cursor);
    assert!(processor.take_palette_change());
    assert!(processor.reply_bytes().is_empty());
    assert_eq!(processor.ignored_sequences(), 0);
}
