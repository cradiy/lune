use super::snapshot;
use crate::{CellPosition, CellWidth, Screen, StorageError, StyleId};

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
