use super::{assert_invariants, snapshot};
use crate::{
    CellText, CellWidth, HistoryLimits, MAX_HISTORY_BYTES, MAX_HISTORY_ROWS, Processor, RowHistory,
    Screen, ScreenBuffer, StorageError, StyleId,
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
    let mut history = RowHistory::new(limits(2, 4096)).unwrap();
    assert!(history.scroll_up(&mut screen, StyleId(9)).unwrap());
    let row = history.row(0).unwrap();
    assert_eq!(row.cells().as_ptr(), cells);
    assert_eq!(row.text(0), Some(CellText::Extended("e\u{301}")));
    let Some(CellText::Extended(text)) = row.text(0) else {
        unreachable!()
    };
    assert_eq!(text.as_ptr(), payload);
    assert_eq!(row.wrap_end(), Some(4));
    assert_eq!(row.cells()[0].style(), StyleId(1));
    assert_eq!(screen.cursor(), cursor);
    assert!(history.scroll_up(&mut screen, StyleId(9)).unwrap());
    assert_eq!(
        history.row(1).unwrap().text(0),
        Some(CellText::Extended("水\u{301}"))
    );
    assert_eq!(history.row(1).unwrap().cells()[0].width(), 2);
    assert!(history.scroll_up(&mut screen, StyleId(9)).unwrap());
    assert_eq!(history.len(), 2);
    assert_eq!(
        history.row(0).unwrap().text(0),
        Some(CellText::Extended("水\u{301}"))
    );
    assert_eq!(history.row(1).unwrap().text(0), Some(CellText::Scalar('Z')));
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
    let mut history = RowHistory::new(limits(4, 64)).unwrap();
    assert!(history.scroll_up(&mut screen, StyleId(0)).unwrap());
    assert_eq!(history.row(0).unwrap().wrap_end(), Some(4));
    screen.set_cursor(0, 0);
    screen
        .write(
            &(String::from("e") + &"\u{301}".repeat(200)),
            CellWidth::Single,
            StyleId(2),
        )
        .unwrap();
    assert!(!history.scroll_up(&mut screen, StyleId(0)).unwrap());
    assert_eq!(history.len(), 1);
    assert_eq!(history.row(0).unwrap().wrap_end(), None);
    assert_eq!(history.row(0).unwrap().text(0), Some(CellText::Scalar('a')));
    assert!(history.retained_bytes() <= 64);
    let previous_limits = history.limits();
    let previous_bytes = history.retained_bytes();
    for invalid in [
        limits(MAX_HISTORY_ROWS + 1, 64),
        limits(4, MAX_HISTORY_BYTES + 1),
    ] {
        assert!(matches!(
            history.set_limits(invalid),
            Err(StorageError::CapacityExceeded)
        ));
        assert_eq!(history.limits(), previous_limits);
        assert_eq!(history.retained_bytes(), previous_bytes);
        assert_eq!(history.len(), 1);
    }
    history.set_limits(limits(4, 1)).unwrap();
    assert!(history.is_empty());
    assert_eq!(history.retained_bytes(), 0);

    let mut screen = Screen::new(4, 2).unwrap();
    let mut history = RowHistory::new(limits(8, 4096)).unwrap();
    for index in 0..8 {
        screen.set_cursor(0, 0);
        screen.write_ascii(&[b'a' + index; 4], StyleId(0)).unwrap();
        history.scroll_up(&mut screen, StyleId(0)).unwrap();
    }
    screen.set_cursor(0, 0);
    let owned = String::from("e") + &"\u{301}".repeat(256);
    screen.write(&owned, CellWidth::Single, StyleId(1)).unwrap();
    let budget = screen.grid().row(0).unwrap().retained_bytes() + 4 * size_of::<crate::Cell>();
    history.set_limits(limits(8, budget)).unwrap();
    assert_eq!(history.len(), 8);
    history.scroll_up(&mut screen, StyleId(0)).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history.row(0).unwrap().text(0), Some(CellText::Scalar('h')));
    assert_eq!(
        history.row(1).unwrap().text(0),
        Some(CellText::Extended(&owned))
    );
    assert_eq!(history.retained_bytes(), budget);

    let mut screen = Screen::new(4, 2).unwrap();
    let mut history = RowHistory::new(limits(3, 4096)).unwrap();
    history.scroll_up(&mut screen, StyleId(0)).unwrap();
    screen.resize(8, 2, StyleId(0)).unwrap();
    history.scroll_up(&mut screen, StyleId(0)).unwrap();
    let reusable = history.row(1).unwrap().cells().as_ptr();
    screen.resize(6, 2, StyleId(0)).unwrap();
    screen.set_cursor(0, 0);
    screen.write(&owned, CellWidth::Single, StyleId(1)).unwrap();
    let budget = screen.grid().row(0).unwrap().retained_bytes();
    history.set_limits(limits(3, budget)).unwrap();
    assert_eq!(history.len(), 2);
    history.scroll_up(&mut screen, StyleId(0)).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(screen.grid().row(1).unwrap().cells().as_ptr(), reusable);

    for budget in [4096, 72] {
        let mut screen = Screen::new(5, 2).unwrap();
        screen.write_ascii(b"abcdefghij", StyleId(1)).unwrap();
        let mut history = RowHistory::new(limits(3, budget)).unwrap();
        history.scroll_up(&mut screen, StyleId(0)).unwrap();
        screen.resize(3, 2, StyleId(0)).unwrap();
        history.scroll_up(&mut screen, StyleId(0)).unwrap();
        if budget == 72 {
            assert_eq!(history.len(), 1);
            assert_eq!(history.row(0).unwrap().cells().len(), 3);
        } else {
            assert_eq!(history.len(), 2);
            assert_eq!(history.row(0).unwrap().cells().len(), 5);
            assert_eq!(history.row(0).unwrap().text(4), Some(CellText::Scalar('e')));
            assert_eq!(history.row(0).unwrap().wrap_end(), None);
            assert_eq!(history.row(1).unwrap().cells().len(), 3);
        }
        assert!(history.retained_bytes() <= budget);
    }
}

#[test]
fn alternate_and_partial_scrolling_preserve_history_and_screen_state_rules() {
    let mut screen = Screen::new(4, 4).unwrap();
    screen.write_ascii(b"abcdefghijklZ", StyleId(1)).unwrap();
    let mut history = RowHistory::new(limits(4, 4096)).unwrap();
    history.scroll_up(&mut screen, StyleId(0)).unwrap();
    assert_eq!(history.row(0).unwrap().wrap_end(), Some(4));
    screen.select_buffer(ScreenBuffer::Alternate);
    screen.write_ascii(b"ALT!", StyleId(2)).unwrap();
    let primary = snapshot(screen.buffer(ScreenBuffer::Primary));
    let cursor = screen.cursor();
    assert!(!history.scroll_up(&mut screen, StyleId(3)).unwrap());
    assert_eq!(history.len(), 1);
    assert_eq!(history.row(0).unwrap().wrap_end(), Some(4));
    assert_eq!(snapshot(screen.buffer(ScreenBuffer::Primary)), primary);
    assert_eq!(screen.cursor(), cursor);
    screen.select_buffer(ScreenBuffer::Primary);
    screen.set_scroll_region(1..4).unwrap();
    screen.set_origin_mode(true);
    screen.set_insert_mode(true);
    let top = screen.grid().row(0).unwrap().cells().to_vec();
    let cursor = screen.cursor();
    assert!(!history.scroll_up(&mut screen, StyleId(4)).unwrap());
    assert_eq!(history.row(0).unwrap().wrap_end(), Some(4));
    assert_eq!(screen.grid().row(0).unwrap().cells(), top);
    assert_eq!(screen.cursor(), cursor);
    assert_eq!(screen.scroll_region(), 1..4);
    assert!(screen.origin_mode() && screen.insert_mode());
    screen.set_scroll_region(0..2).unwrap();
    assert!(!history.scroll_up(&mut screen, StyleId(4)).unwrap());
    assert_eq!(history.row(0).unwrap().wrap_end(), None);
    history.clear();
    assert_eq!(history.limits(), limits(4, 4096));
    assert!(history.is_empty());
    assert_invariants(screen.grid());
}

#[test]
fn style_collection_retains_history_owners_until_eviction() {
    fn ascii(screen: &mut Screen, text: &str, style: StyleId) -> Result<(), StorageError> {
        screen.write_ascii(text.as_bytes(), style)
    }
    let mut screen = Screen::new(4, 2).unwrap();
    let mut processor = Processor::new(4).unwrap();
    processor
        .advance(b"\x1b[31mRED", &mut screen, &mut ascii)
        .unwrap();
    let red = processor.style_id();
    let mut history = RowHistory::new(limits(2, 4096)).unwrap();
    history.scroll_up(&mut screen, StyleId(0)).unwrap();
    processor
        .advance(b"\x1b[34m\x1b[0m", &mut screen, &mut ascii)
        .unwrap();
    assert_eq!(
        processor
            .collect_styles(&screen, history.style_ids())
            .unwrap(),
        1
    );
    assert!(processor.styles().get(red).is_some());
    history.clear();
    assert_eq!(
        processor
            .collect_styles(&screen, history.style_ids())
            .unwrap(),
        1
    );
    assert_eq!(processor.styles().get(red), None);
}
