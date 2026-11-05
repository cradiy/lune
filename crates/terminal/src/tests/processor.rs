use super::{Snapshot, snapshot};
use crate::{
    CellPosition, CellWidth, Color, Cursor, ProcessFailure, Processor, Screen, StorageError, Style,
    StyleError, StyleId,
};

fn write_text(screen: &mut Screen, text: &str, style: StyleId) -> Result<(), StorageError> {
    if text.is_ascii() {
        screen.write_ascii(text.as_bytes(), style)
    } else {
        screen.write(
            text,
            if text == "水" {
                CellWidth::Double
            } else {
                CellWidth::Single
            },
            style,
        )
    }
}

fn run(input: &[u8], cuts: &[usize]) -> (Screen, Processor) {
    let mut screen = Screen::new(8, 4).unwrap();
    let mut processor = Processor::default();
    let mut start = 0;
    for end in cuts.iter().copied().chain([input.len()]) {
        assert_eq!(
            processor
                .advance(&input[start..end], &mut screen, write_text)
                .unwrap(),
            end - start
        );
        start = end;
    }
    processor.finish(&mut screen, write_text).unwrap();
    (screen, processor)
}

fn row_text(screen: &Screen, row: usize) -> String {
    screen
        .grid()
        .text(
            CellPosition { row, column: 0 }..CellPosition {
                row,
                column: screen.grid().columns(),
            },
        )
        .unwrap()
        .to_string()
}

#[test]
fn byte_stream_controls_compose_with_origin_modes_and_split_input() {
    let input = "\x1b[2;4r\x1b[?6habc\x1b[2;3H水\x1b7\x1b[4h\x1b[1;1HXY\x1b[?7l\x1b8!\x1b[0A\x1b[G\x1b[X\x1b[2;8Hyz\x1b[?999h\x1b[?2J\x1b[1:2A\x1b[65536A\x1b]0;synthetic title\x07".as_bytes();
    let (screen, mut processor) = run(input, &[]);
    assert_eq!(row_text(&screen, 0), "        ");
    assert_eq!(row_text(&screen, 1), " Yabc   ");
    assert_eq!(row_text(&screen, 2), "  水!  y");
    assert_eq!(row_text(&screen, 3), "z       ");
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 3,
            column: 1,
            wrap_pending: false
        }
    );
    assert_eq!(screen.scroll_region(), 1..4);
    assert!(screen.origin_mode() && screen.auto_wrap() && screen.insert_mode());
    assert_eq!(screen.grid().row(2).unwrap().wrap_end(), Some(8));
    assert_eq!(processor.parse_errors(), 1);
    assert_eq!(processor.ignored_sequences(), 3);
    let expected = snapshot(screen.grid());
    for split in 0..=input.len() {
        let (mut split_screen, mut split_processor) = run(input, &[split]);
        assert_eq!(snapshot(split_screen.grid()), expected);
        assert_eq!(split_screen.cursor(), screen.cursor());
        assert_eq!(split_screen.scroll_region(), screen.scroll_region());
        assert_eq!(
            (
                split_processor.parse_errors(),
                split_processor.ignored_sequences()
            ),
            (1, 3)
        );
        split_processor
            .advance(b"\x1b8", &mut split_screen, write_text)
            .unwrap();
        assert_eq!(
            split_screen.cursor(),
            Cursor {
                row: 2,
                column: 4,
                wrap_pending: false
            }
        );
        assert!(
            split_screen.origin_mode() && split_screen.auto_wrap() && split_screen.insert_mode()
        );
        assert_eq!(snapshot(split_screen.grid()), expected);
    }
    let cuts: Vec<_> = (0..input.len()).collect();
    let (split_screen, split_processor) = run(input, &cuts);
    assert_eq!(snapshot(split_screen.grid()), expected);
    assert_eq!(
        (
            split_processor.parse_errors(),
            split_processor.ignored_sequences()
        ),
        (1, 3)
    );
    let mut reset_screen = screen;
    processor.reset(&mut reset_screen);
    assert_eq!(reset_screen.cursor(), Cursor::default());
    assert_eq!(
        (processor.parse_errors(), processor.ignored_sequences()),
        (0, 0)
    );
}

#[test]
fn failures_preserve_ascii_runs_and_both_events_of_a_reprocessed_byte() {
    let mut screen = Screen::new(8, 4).unwrap();
    let mut processor = Processor::default();
    let before: Snapshot = snapshot(screen.grid());
    let error = processor
        .advance(b"abc", &mut screen, |_, _, _| Err("blocked"))
        .unwrap_err();
    assert_eq!(error.consumed, 0);
    assert!(matches!(error.error, ProcessFailure::Text("blocked")));
    assert_eq!(snapshot(screen.grid()), before);
    processor.advance(b"abc", &mut screen, write_text).unwrap();
    processor.reset(&mut screen);
    processor
        .advance(b"\xf0\x9f", &mut screen, write_text)
        .unwrap();
    let error = processor
        .advance(b"XA", &mut screen, |s, text, style| {
            if text == "X" {
                return Err("blocked");
            }
            write_text(s, text, style).unwrap();
            Ok(())
        })
        .unwrap_err();
    assert_eq!(error.consumed, 1);
    assert_eq!(row_text(&screen, 0), "�       ");
    let before = snapshot(screen.grid());
    let error = processor
        .advance(b"A", &mut screen, |_, _, _| Err("blocked"))
        .unwrap_err();
    assert_eq!(error.consumed, 0);
    assert_eq!(snapshot(screen.grid()), before);
    processor.advance(b"A", &mut screen, write_text).unwrap();
    assert_eq!(row_text(&screen, 0), "�XA     ");

    processor.reset(&mut screen);
    processor
        .advance(b"\xf0\x9f", &mut screen, write_text)
        .unwrap();
    let error = processor
        .advance(b"\nA", &mut screen, |_, _, _| Err("blocked"))
        .unwrap_err();
    assert_eq!(error.consumed, 1);
    assert_eq!(screen.cursor(), Cursor::default());
    processor.advance(b"A", &mut screen, write_text).unwrap();
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 1,
            column: 2,
            wrap_pending: false
        }
    );
    assert_eq!(row_text(&screen, 0), "�       ");
    assert_eq!(row_text(&screen, 1), " A      ");
}

#[test]
fn finishing_and_resetting_own_pending_input_without_duplicate_effects() {
    let mut screen = Screen::new(8, 4).unwrap();
    let mut processor = Processor::default();
    processor
        .advance(b"\xe6\xb0", &mut screen, write_text)
        .unwrap();
    assert!(
        processor
            .finish(&mut screen, |_, _, _| Err("blocked"))
            .is_err()
    );
    assert_eq!(screen.cursor(), Cursor::default());
    assert!(
        processor
            .finish(&mut screen, |_, _, _| Err("blocked"))
            .is_err()
    );
    processor.finish(&mut screen, write_text).unwrap();
    let before = snapshot(screen.grid());
    processor.finish(&mut screen, write_text).unwrap();
    assert_eq!(snapshot(screen.grid()), before);
    assert_eq!(screen.cursor().column, 1);
    processor
        .advance(b"\x1b[", &mut screen, write_text)
        .unwrap();
    processor.finish(&mut screen, write_text).unwrap();
    assert_eq!(processor.parse_errors(), 1);
    processor.finish(&mut screen, write_text).unwrap();
    assert_eq!(processor.parse_errors(), 1);
    processor
        .advance(b"\xe6\xb0", &mut screen, write_text)
        .unwrap();
    processor
        .finish(&mut screen, |_, _, _| Err("blocked"))
        .unwrap_err();
    processor.reset(&mut screen);
    processor.finish(&mut screen, write_text).unwrap();
    assert_eq!(screen.cursor(), Cursor::default());
    assert_eq!(processor.parse_errors(), 0);
    assert_eq!(row_text(&screen, 0), "        ");
}

#[test]
fn sgr_styles_follow_saved_cursors_wide_text_erasure_and_input_boundaries() {
    let input = "\x1b[31mR\x1b7\x1b[1;48;5;17m\x1b[1;5HB\x1b8S\x1b[0mN\x1b[48:2::1:2:3m\x1b[3;1H\x1b[K\x1b[4;1H\x1b[38;2;0;255;1m水\x1b[38;5;256m!\x1b[?31m\x1b[1 m".as_bytes();
    let (screen, processor) = run(input, &[]);
    let red = Style {
        foreground: Color::Indexed(1),
        ..Style::default()
    };
    let background = Color::Rgb {
        red: 1,
        green: 2,
        blue: 3,
    };
    let green = Style {
        foreground: Color::Rgb {
            red: 0,
            green: 255,
            blue: 1,
        },
        background,
        ..Style::default()
    };
    let style_at = |row: usize, column: usize| {
        let id = screen.grid().row(row).unwrap().cells()[column].style();
        *processor.styles().get(id).unwrap()
    };
    assert_eq!(row_text(&screen, 0), "RSN B   ");
    assert_eq!(style_at(0, 0), red);
    assert_eq!(style_at(0, 1), red);
    assert_eq!(style_at(0, 2), Style::default());
    assert_eq!(
        style_at(0, 4),
        Style {
            bold: true,
            background: Color::Indexed(17),
            ..red
        }
    );
    for column in 0..8 {
        assert_eq!(
            style_at(2, column),
            Style {
                background,
                ..Style::default()
            }
        );
    }
    for column in 0..3 {
        assert_eq!(style_at(3, column), green);
    }
    assert_eq!(processor.styles().get(processor.style_id()), Some(&green));
    assert_eq!(processor.ignored_sequences(), 3);
    let expected = snapshot(screen.grid());
    for split in 0..=input.len() {
        let (split_screen, split_processor) = run(input, &[split]);
        assert_eq!(snapshot(split_screen.grid()), expected);
        assert_eq!(split_screen.cursor(), screen.cursor());
        assert_eq!(split_processor.ignored_sequences(), 3);
        assert_eq!(
            split_processor.styles().get(split_processor.style_id()),
            Some(&green)
        );
    }
}

#[test]
fn full_style_storage_retries_after_tracing_current_saved_and_external_owners() {
    let mut screen = Screen::new(8, 4).unwrap();
    let mut processor = Processor::new(4).unwrap();
    processor
        .advance(b"\x1b[31m\x1b7\x1b[34m", &mut screen, write_text)
        .unwrap();
    let blue = processor.style_id();
    processor
        .advance(b"\x1b[32m", &mut screen, write_text)
        .unwrap();
    let green = processor.style_id();
    let input = b"\x1b[35mX";
    let error = processor
        .advance(input, &mut screen, write_text)
        .unwrap_err();
    assert_eq!(error.consumed, 5);
    assert!(matches!(
        error.error,
        ProcessFailure::Style(StyleError::CapacityExceeded)
    ));
    assert_eq!(processor.style_id(), green);
    assert_eq!(screen.cursor(), Cursor::default());
    assert_eq!(processor.collect_styles(&screen, [blue]).unwrap(), 0);
    let error = processor
        .advance(&input[5..], &mut screen, write_text)
        .unwrap_err();
    assert_eq!(error.consumed, 0);
    assert!(matches!(
        processor.finish(&mut screen, write_text),
        Err(ProcessFailure::Style(StyleError::CapacityExceeded))
    ));
    assert_eq!(processor.collect_styles(&screen, []).unwrap(), 1);
    processor
        .advance(&input[5..], &mut screen, write_text)
        .unwrap();
    let magenta = processor.style_id();
    assert_ne!(magenta, blue);
    assert_eq!(processor.styles().get(blue), None);
    assert_eq!(row_text(&screen, 0), "X       ");
    processor
        .advance(b"\x1b8R", &mut screen, write_text)
        .unwrap();
    let red = processor.style_id();
    assert_eq!(
        processor.styles().get(red),
        Some(&Style {
            foreground: Color::Indexed(1),
            ..Style::default()
        })
    );
    assert_eq!(row_text(&screen, 0), "R       ");
    processor.reset(&mut screen);
    assert_eq!(processor.style_id(), StyleId(0));
    assert_eq!(processor.collect_styles(&screen, [red]).unwrap(), 2);
    assert!(processor.styles().get(red).is_some());
    assert_eq!(processor.collect_styles(&screen, []).unwrap(), 1);
    assert_eq!(processor.styles().get(red), None);
    assert_eq!(processor.styles().style_count(), 1);
    let mut limited = Processor::new(1).unwrap();
    assert!(
        limited
            .advance(b"\x1b[31mR", &mut screen, write_text)
            .is_err()
    );
    limited.reset(&mut screen);
    limited.finish(&mut screen, write_text).unwrap();
    limited.advance(b"D", &mut screen, write_text).unwrap();
    assert_eq!(row_text(&screen, 0), "D       ");
    assert_eq!(limited.style_id(), StyleId(0));
}

#[test]
fn designated_charsets_shift_save_restore_and_preserve_unicode_across_chunks() {
    let input = "A^\x1b(0_`abcdefghijklmnopqrstuvwxyz{|}~\r\n\x1b(B\x1b)0\x0elqqk\x0fq\x0e\x1b[31m\x1b7\x1b)B\x1b(0\x0f\x1b[3;1Hq水\x1b[34m\x1b8x\x0fx\x0e\x1b)Aq".as_bytes();
    let expected = "A^\u{a0}◆▒␉␌␍␊°±␤␋┘┐┌└┼⎺⎻─⎼⎽├┤┴┬│≤≥π≠£·";
    for split in 0..=input.len() {
        let mut screen = Screen::new(64, 4).unwrap();
        let mut processor = Processor::default();
        processor
            .advance(&input[..split], &mut screen, write_text)
            .unwrap();
        processor
            .advance(&input[split..], &mut screen, write_text)
            .unwrap();
        processor.finish(&mut screen, write_text).unwrap();
        assert_eq!(row_text(&screen, 0).trim_end(), expected);
        assert_eq!(row_text(&screen, 1).trim_end(), "┌──┐q│x─");
        assert_eq!(row_text(&screen, 2).trim_end(), "─水");
        assert_eq!(processor.ignored_sequences(), 1);
        assert_eq!(
            processor.styles().get(processor.style_id()),
            Some(&Style {
                foreground: Color::Indexed(1),
                ..Style::default()
            })
        );
        processor
            .advance(b"\x1bcq\x1b8x", &mut screen, write_text)
            .unwrap();
        assert_eq!(row_text(&screen, 0).trim_end(), "x");
        assert_eq!(processor.style_id(), StyleId(0));
        processor.reset(&mut screen);
        processor
            .advance(b"q\x0ex", &mut screen, write_text)
            .unwrap();
        assert_eq!(row_text(&screen, 0).trim_end(), "qx");
    }
}

#[test]
fn mapped_text_retries_once_after_ground_and_reprocessed_utf8_input() {
    let mut screen = Screen::new(8, 4).unwrap();
    let mut processor = Processor::default();
    let input = b"\x1b(0q\x1b(BX";
    let error = processor
        .advance(input, &mut screen, |_, _, _| Err("blocked"))
        .unwrap_err();
    assert_eq!(error.consumed, 4);
    assert_eq!(screen.cursor(), Cursor::default());
    let again = processor
        .advance(&input[4..], &mut screen, |_, _, _| Err("blocked"))
        .unwrap_err();
    assert_eq!(again.consumed, 0);
    processor
        .advance(&input[4..], &mut screen, write_text)
        .unwrap();
    assert_eq!(row_text(&screen, 0), "─X      ");
    processor.reset(&mut screen);
    processor
        .advance(b"\x1b)0\x0e\xf0\x9f", &mut screen, write_text)
        .unwrap();
    let error = processor
        .advance(b"q\x0fq", &mut screen, |s, value, style| {
            if value == "─" {
                return Err("blocked");
            }
            write_text(s, value, style).unwrap();
            Ok(())
        })
        .unwrap_err();
    assert_eq!(error.consumed, 1);
    assert_eq!(row_text(&screen, 0), "�       ");
    processor
        .advance(b"\x0fq", &mut screen, write_text)
        .unwrap();
    assert_eq!(row_text(&screen, 0), "�─q     ");
}
