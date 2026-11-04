use super::{Snapshot, snapshot};
use crate::{CellPosition, CellWidth, Cursor, Processor, Screen, StorageError, StyleId};

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
    let mut processor = Processor::new(StyleId(3));
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
    assert_eq!((error.consumed, error.error), (0, "blocked"));
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
