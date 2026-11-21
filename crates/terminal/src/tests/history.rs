use super::support;

use super::{assert_invariants, snapshot};
use crate::{
    CellText, CellWidth, HistoryLimits, MAX_HISTORY_BYTES, MAX_HISTORY_ROWS, Processor, Screen,
    ScreenBuffer, StorageError, StyleId, UnicodeWriter,
};

fn limits(rows: usize, bytes: usize) -> HistoryLimits {
    HistoryLimits {
        max_rows: rows,
        max_bytes: bytes,
    }
}

#[test]
fn capture_transfers_owned_rows_preserves_joins_and_recycles_the_oldest_buffer() {
    let mut screen = Screen::new(4, 3).unwrap();
    screen
        .write("e\u{301}", CellWidth::Single, StyleId(1))
        .unwrap();
    screen.write_ascii(b"abc", StyleId(2)).unwrap();
    screen
        .write("水\u{301}", CellWidth::Double, StyleId(3))
        .unwrap();
    screen.write_ascii(b"deZ", StyleId(4)).unwrap();
    screen.save_cursor();
    let cursor = screen.cursor();
    let cells = screen.grid().row(0).unwrap().cells().as_ptr();
    let Some(CellText::Extended(text)) = screen.grid().row(0).unwrap().text(0) else {
        panic!("Owned text is present")
    };
    let payload = text.as_ptr();
    screen.set_history_limits(limits(2, 4096)).unwrap();
    screen.scroll_up(1, StyleId(9)).unwrap();
    let row = screen.history().row(0).unwrap();
    assert_eq!(row.cells().as_ptr(), cells);
    assert_eq!(row.text(0), Some(CellText::Extended("e\u{301}")));
    let Some(CellText::Extended(text)) = row.text(0) else {
        unreachable!()
    };
    assert_eq!(text.as_ptr(), payload);
    assert_eq!(row.wrap_end(), Some(4));
    assert_eq!(row.cells()[0].style(), StyleId(1));
    assert_eq!(screen.cursor(), cursor);
    screen.scroll_up(1, StyleId(9)).unwrap();
    assert_eq!(
        screen.history().row(1).unwrap().text(0),
        Some(CellText::Extended("水\u{301}"))
    );
    assert_eq!(screen.history().row(1).unwrap().cells()[0].width(), 2);
    screen.scroll_up(1, StyleId(9)).unwrap();
    assert_eq!(screen.history().len(), 2);
    assert_eq!(
        screen.history().row(0).unwrap().text(0),
        Some(CellText::Extended("水\u{301}"))
    );
    assert_eq!(
        screen.history().row(1).unwrap().text(0),
        Some(CellText::Scalar('Z'))
    );
    assert_eq!(screen.grid().row(2).unwrap().cells().as_ptr(), cells);
    assert!(
        screen
            .grid()
            .row(2)
            .unwrap()
            .cells()
            .iter()
            .all(|cell| cell.is_erased() && cell.style() == StyleId(9))
    );
    assert_eq!(screen.cursor(), cursor);
    screen.carriage_return();
    screen.restore_cursor();
    assert_eq!(screen.cursor(), cursor);
    assert_invariants(screen.grid());
}

#[test]
fn byte_limits_account_for_capacity_and_oversized_rows_break_adjacency() {
    let mut screen = Screen::new(4, 3).unwrap();
    screen.write_ascii(b"abcdefghZ", StyleId(1)).unwrap();
    screen.set_history_limits(limits(4, 64)).unwrap();
    screen.scroll_up(1, StyleId(0)).unwrap();
    assert_eq!(screen.history().row(0).unwrap().wrap_end(), Some(4));
    screen.set_cursor(0, 0);
    screen
        .write(
            &(String::from("e") + &"\u{301}".repeat(200)),
            CellWidth::Single,
            StyleId(2),
        )
        .unwrap();
    screen.scroll_up(1, StyleId(0)).unwrap();
    assert_eq!(screen.history().len(), 1);
    assert_eq!(screen.history().row(0).unwrap().wrap_end(), None);
    assert_eq!(
        screen.history().row(0).unwrap().text(0),
        Some(CellText::Scalar('a'))
    );
    assert!(screen.history().retained_bytes() <= 64);
    let previous_limits = screen.history().limits();
    let previous_bytes = screen.history().retained_bytes();
    for invalid in [
        limits(MAX_HISTORY_ROWS + 1, 64),
        limits(4, MAX_HISTORY_BYTES + 1),
    ] {
        assert!(matches!(
            screen.set_history_limits(invalid),
            Err(StorageError::CapacityExceeded)
        ));
        assert_eq!(screen.history().limits(), previous_limits);
        assert_eq!(screen.history().retained_bytes(), previous_bytes);
        assert_eq!(screen.history().len(), 1);
    }
    screen.set_history_limits(limits(4, 1)).unwrap();
    assert!(screen.history().is_empty());
    assert_eq!(screen.history().retained_bytes(), 0);

    let mut screen = Screen::new(4, 2).unwrap();
    screen.set_history_limits(limits(8, 4096)).unwrap();
    for index in 0..8 {
        screen.set_cursor(0, 0);
        screen.write_ascii(&[b'a' + index; 4], StyleId(0)).unwrap();
        screen.scroll_up(1, StyleId(0)).unwrap();
    }
    screen.set_cursor(0, 0);
    let owned = String::from("e") + &"\u{301}".repeat(256);
    screen.write(&owned, CellWidth::Single, StyleId(1)).unwrap();
    let budget = screen.grid().row(0).unwrap().retained_bytes() + 4 * size_of::<crate::Cell>();
    screen.set_history_limits(limits(8, budget)).unwrap();
    assert_eq!(screen.history().len(), 8);
    screen.scroll_up(1, StyleId(0)).unwrap();
    assert_eq!(screen.history().len(), 2);
    assert_eq!(
        screen.history().row(0).unwrap().text(0),
        Some(CellText::Scalar('h'))
    );
    assert_eq!(
        screen.history().row(1).unwrap().text(0),
        Some(CellText::Extended(&owned))
    );
    assert_eq!(screen.history().retained_bytes(), budget);

    let mut screen = Screen::new(4, 2).unwrap();
    screen.set_history_limits(limits(3, 4096)).unwrap();
    screen.scroll_up(1, StyleId(0)).unwrap();
    screen.resize(8, 2, StyleId(0)).unwrap();
    screen.scroll_up(1, StyleId(0)).unwrap();
    let reusable = screen.history().row(1).unwrap().cells().as_ptr();
    screen.resize(6, 2, StyleId(0)).unwrap();
    screen.set_cursor(0, 0);
    screen.write(&owned, CellWidth::Single, StyleId(1)).unwrap();
    let budget = screen.grid().row(0).unwrap().retained_bytes();
    screen.set_history_limits(limits(3, budget)).unwrap();
    assert_eq!(screen.history().len(), 2);
    screen.scroll_up(1, StyleId(0)).unwrap();
    assert_eq!(screen.history().len(), 1);
    assert_eq!(screen.grid().row(1).unwrap().cells().as_ptr(), reusable);

    for budget in [4096, 72] {
        let mut screen = Screen::new(5, 2).unwrap();
        screen.write_ascii(b"abcdefghij", StyleId(1)).unwrap();
        screen.set_history_limits(limits(3, budget)).unwrap();
        screen.scroll_up(1, StyleId(0)).unwrap();
        screen.resize(3, 2, StyleId(0)).unwrap();
        screen.scroll_up(1, StyleId(0)).unwrap();
        if budget == 72 {
            assert_eq!(screen.history().len(), 1);
            assert_eq!(screen.history().row(0).unwrap().cells().len(), 3);
        } else {
            assert_eq!(screen.history().len(), 2);
            assert_eq!(screen.history().row(0).unwrap().cells().len(), 5);
            assert_eq!(
                screen.history().row(0).unwrap().text(4),
                Some(CellText::Scalar('e'))
            );
            assert_eq!(screen.history().row(0).unwrap().wrap_end(), None);
            assert_eq!(screen.history().row(1).unwrap().cells().len(), 3);
        }
        assert!(screen.history().retained_bytes() <= budget);
    }
}

#[test]
fn capture_follows_the_primary_top_region() {
    let mut screen = Screen::new(4, 4).unwrap();
    screen.write_ascii(b"abcdefghijklZ", StyleId(1)).unwrap();
    screen.set_history_limits(limits(4, 4096)).unwrap();
    screen.scroll_up(1, StyleId(0)).unwrap();
    assert_eq!(screen.history().row(0).unwrap().wrap_end(), Some(4));
    screen.select_buffer(ScreenBuffer::Alternate);
    screen.write_ascii(b"ALT!", StyleId(2)).unwrap();
    let primary = snapshot(screen.buffer(ScreenBuffer::Primary));
    let cursor = screen.cursor();
    screen.scroll_up(1, StyleId(3)).unwrap();
    assert_eq!(screen.history().len(), 1);
    assert_eq!(screen.history().row(0).unwrap().wrap_end(), Some(4));
    assert_eq!(snapshot(screen.buffer(ScreenBuffer::Primary)), primary);
    assert_eq!(screen.cursor(), cursor);
    screen.select_buffer(ScreenBuffer::Primary);
    screen.set_scroll_region(1..4).unwrap();
    screen.set_origin_mode(true);
    screen.set_insert_mode(true);
    let top = screen.grid().row(0).unwrap().cells().to_vec();
    let cursor = screen.cursor();
    screen.scroll_up(1, StyleId(4)).unwrap();
    assert_eq!(screen.history().row(0).unwrap().wrap_end(), Some(4));
    assert_eq!(screen.grid().row(0).unwrap().cells(), top);
    assert_eq!(screen.cursor(), cursor);
    assert_eq!(screen.scroll_region(), 1..4);
    assert!(screen.origin_mode() && screen.insert_mode());
    screen.set_scroll_region(0..2).unwrap();
    screen.scroll_up(1, StyleId(4)).unwrap();
    assert_eq!(screen.history().len(), 2);
    assert_eq!(screen.history().row(0).unwrap().wrap_end(), Some(4));
    let captured = screen.history().row(1).unwrap();
    assert_eq!(captured.text(0), Some(CellText::Scalar('e')));
    assert_eq!(captured.wrap_end(), None);
    screen.clear_history();
    assert_eq!(screen.history().limits(), limits(4, 4096));
    assert!(screen.history().is_empty());
    assert_invariants(screen.grid());
}

#[test]
fn style_collection_retains_history_owners_until_eviction() {
    fn ascii(screen: &mut Screen, text: &str, style: StyleId) -> Result<(), StorageError> {
        screen.write_ascii(text.as_bytes(), style)
    }
    let mut screen = Screen::new(4, 2).unwrap();
    let mut processor = Processor::new(4, support::palette()).unwrap();
    processor
        .advance(b"\x1b[31mRED", &mut screen, &mut ascii)
        .unwrap();
    let red = processor.style_id();
    screen.set_history_limits(limits(2, 4096)).unwrap();
    screen.scroll_up(1, StyleId(0)).unwrap();
    processor
        .advance(b"\x1b[34m\x1b[0m", &mut screen, &mut ascii)
        .unwrap();
    assert_eq!(processor.collect_styles(&screen, []).unwrap(), 1);
    assert!(processor.styles().get(red).is_some());
    screen.clear_history();
    assert_eq!(processor.collect_styles(&screen, []).unwrap(), 1);
    assert_eq!(processor.styles().get(red), None);
}

#[test]
fn automatic_capture_preserves_control_boundaries_and_single_row_early_wraps() {
    for (control, carriage) in [
        (b"\n".as_slice(), false),
        (b"\x0b", false),
        (b"\x0c", false),
        (b"\x1bD", false),
        (b"\x1bE", true),
        (b"\x1b[20h\n", true),
    ] {
        let mut screen = Screen::new(4, 2).unwrap();
        screen.set_history_limits(limits(4, 4096)).unwrap();
        screen.write_ascii(b"abcdefgh", StyleId(0)).unwrap();
        let mut processor = support::processor();
        processor
            .advance(control, &mut screen, &mut UnicodeWriter::default())
            .unwrap();
        assert_eq!(screen.history().len(), 1);
        assert_eq!(
            screen.history().row(0).unwrap().text(0),
            Some(CellText::Scalar('a'))
        );
        assert_eq!(screen.history().row(0).unwrap().wrap_end(), Some(4));
        assert_eq!(screen.cursor().column, if carriage { 0 } else { 3 });
        assert!(!screen.cursor().wrap_pending);
        screen.line_feed(StyleId(0)).unwrap();
        assert_eq!(
            screen.history().row(1).unwrap().text(0),
            Some(CellText::Scalar('e'))
        );
        assert_eq!(screen.history().row(1).unwrap().wrap_end(), None);
    }
    let mut screen = Screen::new(4, 1).unwrap();
    screen.set_history_limits(limits(4, 4096)).unwrap();
    let mut processor = support::processor();
    let mut writer = UnicodeWriter::default();
    processor
        .advance("abc❤\u{fe0f}X".as_bytes(), &mut screen, &mut writer)
        .unwrap();
    let row = screen.history().row(0).unwrap();
    assert_eq!(row.wrap_end(), Some(3));
    assert_eq!(row.text(3), Some(CellText::Scalar(' ')));
    assert!(row.cells()[3].is_erased());
    assert_eq!(
        screen.grid().row(0).unwrap().text(0),
        Some(CellText::Extended("❤\u{fe0f}"))
    );
    assert_eq!(
        screen.grid().row(0).unwrap().text(2),
        Some(CellText::Scalar('X'))
    );
    assert_invariants(screen.grid());
    let configured = screen.history().limits();
    processor
        .advance(b"\x1bc", &mut screen, &mut writer)
        .unwrap();
    assert!(screen.history().is_empty());
    assert_eq!(screen.history().limits(), configured);
}

#[test]
fn automatic_history_matches_fragmented_unicode_input_and_edits_end_top_adjacency() {
    let wire = "abcdefghijk\r\nabc❤\u{fe0f}X\nabc\u{600}abcdefghijkl\x1b[2S水\u{301}👩\u{200d}💻";
    for columns in [1, 2, 4, 9] {
        for (rows, end) in [(1, 1), (3, 3), (2, 1), (4, 3)] {
            let run = |chunk| {
                let mut screen = Screen::new(columns, rows).unwrap();
                screen.set_history_limits(limits(7, 65536)).unwrap();
                if end < rows {
                    screen.set_cursor(end, 0);
                    screen.write_ascii(b"S", StyleId(0)).unwrap();
                    screen.set_scroll_region(0..end).unwrap();
                }
                let mut processor = support::processor();
                let mut writer = UnicodeWriter::default();
                for part in wire.as_bytes().chunks(chunk) {
                    processor.advance(part, &mut screen, &mut writer).unwrap();
                }
                processor.finish(&mut screen, &mut writer).unwrap();
                assert_invariants(screen.grid());
                if end < rows {
                    assert_eq!(
                        screen.grid().row(end).unwrap().text(0),
                        Some(CellText::Scalar('S'))
                    );
                }
                screen
            };
            let bulk = run(wire.len());
            for chunk in [1, 2, 3, 7] {
                let fragmented = run(chunk);
                assert_eq!(snapshot(bulk.grid()), snapshot(fragmented.grid()));
                assert_eq!(bulk.cursor(), fragmented.cursor());
                assert_eq!(
                    format!("{:?}", bulk.history().iter_rows().collect::<Vec<_>>()),
                    format!("{:?}", fragmented.history().iter_rows().collect::<Vec<_>>())
                );
            }
        }
    }
    for edit in 0..7 {
        let mut screen = Screen::new(4, 2).unwrap();
        screen.set_history_limits(limits(4, 4096)).unwrap();
        screen.write_ascii(b"abcdefghi", StyleId(0)).unwrap();
        assert_eq!(screen.history().row(0).unwrap().wrap_end(), Some(4));
        screen.set_cursor(0, 0);
        match edit {
            0 => screen.scroll_down(1, StyleId(0)),
            1 => screen.reverse_index(StyleId(0)),
            2 => screen.insert_lines(1, StyleId(0)),
            3 => screen.delete_lines(1, StyleId(0)),
            4 => screen.erase_line(crate::EraseMode::All, StyleId(0)),
            5 => screen.erase_display(crate::EraseMode::All, StyleId(0)),
            _ => screen.resize(3, 2, StyleId(0)).unwrap(),
        }
        assert_eq!(screen.history().len(), 1);
        assert_eq!(screen.history().row(0).unwrap().wrap_end(), None);
    }
}
