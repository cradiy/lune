use super::snapshot;
use crate::{CellPosition, CellWidth, HistoryLimits, Screen, ScreenBuffer, StorageError, StyleId};

fn point(row: usize, column: usize) -> CellPosition {
    CellPosition { row, column }
}

#[test]
fn text_ranges_join_rows_preserve_units_and_validate_physical_boundaries() {
    let mut screen = Screen::new(4, 4).unwrap();
    screen.write_ascii(b"abc", StyleId(1)).unwrap();
    screen
        .write("水\u{301}", CellWidth::Double, StyleId(2))
        .unwrap();
    screen.write_ascii(b"xy", StyleId(3)).unwrap();
    screen.write("é", CellWidth::Single, StyleId(4)).unwrap();
    screen.line_feed(StyleId(0)).unwrap();
    screen.carriage_return();
    screen
        .write("👩\u{200d}💻", CellWidth::Double, StyleId(5))
        .unwrap();
    screen.write_ascii(b" z", StyleId(6)).unwrap();
    let before = snapshot(screen.grid());
    let cursor = screen.cursor();
    for (start, end, expected) in [
        ((0, 0), (3, 4), "abc水\u{301}xyé   \n👩\u{200d}💻 z"),
        ((1, 1), (1, 2), "水\u{301}"),
        ((1, 0), (1, 1), "水\u{301}"),
        ((1, 1), (1, 1), ""),
        ((0, 3), (1, 1), "水\u{301}"),
        ((0, 3), (1, 0), ""),
        ((0, 3), (0, 4), ""),
        ((1, 4), (2, 1), "é"),
        ((2, 4), (3, 0), "\n"),
        ((2, 1), (2, 4), "   "),
        ((3, 1), (3, 2), "👩\u{200d}💻"),
        ((3, 4), (3, 4), ""),
    ] {
        let view = screen
            .grid()
            .text(point(start.0, start.1)..point(end.0, end.1))
            .unwrap();
        assert_eq!(view.to_string(), expected);
    }
    for range in [
        point(0, 2)..point(0, 1),
        point(2, 0)..point(1, 4),
        point(0, 0)..point(4, 0),
        point(4, 0)..point(4, 0),
        point(0, 0)..point(0, 5),
        point(0, usize::MAX)..point(0, usize::MAX),
        point(usize::MAX, 0)..point(usize::MAX, 0),
    ] {
        assert!(matches!(
            screen.grid().text(range),
            Err(StorageError::OutOfBounds)
        ));
    }
    assert_eq!(snapshot(screen.grid()), before);
    assert_eq!(screen.cursor(), cursor);
}

#[test]
fn screen_text_joins_history_to_visible_rows_and_borrows_complete_units() {
    let mut screen = Screen::new(4, 2).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 4,
            max_bytes: 4096,
        })
        .unwrap();
    screen.write_ascii(b"abc", StyleId(1)).unwrap();
    screen
        .write("水\u{301}", CellWidth::Double, StyleId(2))
        .unwrap();
    screen.write_ascii(b"xyZ", StyleId(3)).unwrap();
    let before = snapshot(screen.grid());
    let cursor = screen.cursor();
    let view = screen.view();
    assert_eq!((view.history_rows(), view.rows()), (1, 3));
    let mut rows = view.iter_rows();
    assert!(std::ptr::eq(
        rows.next().unwrap(),
        screen.history().row(0).unwrap()
    ));
    assert!(std::ptr::eq(
        rows.next_back().unwrap(),
        screen.grid().row(1).unwrap()
    ));
    assert_eq!(rows.len(), 1);
    assert!(std::ptr::eq(
        rows.next().unwrap(),
        screen.grid().row(0).unwrap()
    ));
    assert!(rows.next_back().is_none());
    for (start, end, expected) in [
        ((0, 0), (2, 4), "abc水\u{301}xyZ   "),
        ((0, 0), (1, 4), "abc水\u{301}xy"),
        ((0, 3), (0, 4), ""),
        ((0, 3), (1, 1), "水\u{301}"),
        ((0, 4), (1, 0), ""),
        ((1, 1), (1, 2), "水\u{301}"),
        ((1, 1), (1, 1), ""),
        ((1, 4), (2, 4), "Z   "),
    ] {
        assert_eq!(
            view.text(point(start.0, start.1)..point(end.0, end.1))
                .unwrap()
                .to_string(),
            expected
        );
    }
    assert_eq!(snapshot(screen.grid()), before);
    assert_eq!(screen.cursor(), cursor);
}

#[test]
fn screen_text_uses_each_historical_width_and_excludes_primary_rows_on_alternate() {
    let mut screen = Screen::new(4, 2).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 4,
            max_bytes: 4096,
        })
        .unwrap();
    screen.write_ascii(b"abcdefghi", StyleId(1)).unwrap();
    screen.resize(6, 2, StyleId(0)).unwrap();
    screen.scroll_up(1, StyleId(0)).unwrap();
    screen.resize(3, 2, StyleId(0)).unwrap();
    let view = screen.view();
    assert_eq!(
        view.iter_rows()
            .map(|row| row.cells().len())
            .collect::<Vec<_>>(),
        [4, 6, 3, 3]
    );
    assert_eq!(
        view.text(point(0, 0)..point(2, 1)).unwrap().to_string(),
        "abcd\nefgh  \ni"
    );
    assert_eq!(
        view.text(point(0, 4)..point(1, 6)).unwrap().to_string(),
        "\nefgh  "
    );
    for range in [
        point(1, 6)..point(1, 7),
        point(2, 4)..point(2, 4),
        point(1, 0)..point(0, 4),
        point(0, 0)..point(4, 0),
        point(usize::MAX, 0)..point(usize::MAX, 0),
        point(0, usize::MAX)..point(0, usize::MAX),
    ] {
        assert!(matches!(view.text(range), Err(StorageError::OutOfBounds)));
    }
    let primary = view.text(point(0, 0)..point(3, 3)).unwrap().to_string();
    screen.select_buffer(ScreenBuffer::Alternate);
    screen.set_cursor(0, 0);
    screen.write_ascii(b"ALT", StyleId(2)).unwrap();
    assert_eq!((screen.view().history_rows(), screen.view().rows()), (0, 2));
    assert_eq!(
        screen
            .view()
            .text(point(0, 0)..point(0, 3))
            .unwrap()
            .to_string(),
        "ALT"
    );
    assert!(screen.view().row(2).is_none());
    screen.select_buffer(ScreenBuffer::Primary);
    assert_eq!(
        screen
            .view()
            .text(point(0, 0)..point(3, 3))
            .unwrap()
            .to_string(),
        primary
    );
    screen.clear_history();
    assert_eq!((screen.view().history_rows(), screen.view().rows()), (0, 2));
}
