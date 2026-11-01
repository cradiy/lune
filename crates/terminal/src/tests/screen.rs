use super::{assert_invariants, snapshot};
use crate::{CellText, CellWidth, Cursor, Screen, StorageError, StyleId};

#[test]
fn ascii_batches_match_unit_writes_across_wraps_and_scrolls() {
    let input = b"abcdefghijklmnopqrstuvwxyz0123456789";
    for columns in 1..=12 {
        for rows in 1..=3 {
            for row in 0..rows {
                for column in 0..=columns {
                    let mut bulk = Screen::new(columns, rows).unwrap();
                    let mut scalar = Screen::new(columns, rows).unwrap();
                    for screen in [&mut bulk, &mut scalar] {
                        screen
                            .write("e\u{301}", CellWidth::Single, StyleId(1))
                            .unwrap();
                        if columns > 1 {
                            screen.set_cursor(rows - 1, columns - 2).unwrap();
                            screen.write("界", CellWidth::Double, StyleId(2)).unwrap();
                        }
                        screen.set_cursor(row, column.min(columns - 1)).unwrap();
                        if column == columns {
                            screen.write("!", CellWidth::Single, StyleId(3)).unwrap();
                        }
                    }
                    for chunk in input.chunks(7) {
                        bulk.write_ascii(chunk, StyleId(4)).unwrap();
                        for &byte in chunk {
                            scalar
                                .write(
                                    char::from(byte).encode_utf8(&mut [0; 4]),
                                    CellWidth::Single,
                                    StyleId(4),
                                )
                                .unwrap();
                        }
                        assert_eq!(bulk.cursor(), scalar.cursor());
                        assert_eq!(snapshot(bulk.grid()), snapshot(scalar.grid()));
                        assert_invariants(bulk.grid());
                    }
                    let before = snapshot(bulk.grid());
                    let cursor = bulk.cursor();
                    assert!(matches!(
                        bulk.write_ascii(b"printable\n", StyleId(9)),
                        Err(StorageError::InvalidAscii)
                    ));
                    bulk.write_ascii(b"", StyleId(9)).unwrap();
                    assert_eq!(bulk.cursor(), cursor);
                    assert_eq!(snapshot(bulk.grid()), before);
                }
            }
        }
    }
}

#[test]
fn wide_units_wrap_whole_and_repair_the_old_edge() {
    for rows in [1, 2, 3] {
        for at_bottom in [false, true] {
            let mut screen = Screen::new(4, rows).unwrap();
            let row = if at_bottom { rows - 1 } else { 0 };
            screen.set_cursor(row, 2).unwrap();
            screen.write("旧", CellWidth::Double, StyleId(1)).unwrap();
            screen
                .write("水\u{301}", CellWidth::Double, StyleId(2))
                .unwrap();
            let target = (row + 1).min(rows - 1);
            assert_eq!(
                screen.grid().row(target).unwrap().text(0),
                Some(CellText::Extended("水\u{301}"))
            );
            if rows > 1 {
                let previous = if at_bottom { row - 1 } else { row };
                assert_eq!(
                    screen.grid().row(previous).unwrap().text(2),
                    Some(CellText::Scalar('旧'))
                );
            }
            assert_eq!(
                screen.cursor(),
                Cursor {
                    row: target,
                    column: 2,
                    wrap_pending: false
                }
            );

            screen.set_cursor(target, 2).unwrap();
            screen.write("旧", CellWidth::Double, StyleId(1)).unwrap();
            screen.set_cursor(target, 3).unwrap();
            screen.write("新", CellWidth::Double, StyleId(3)).unwrap();
            if rows > 1 {
                let previous = if target == rows - 1 {
                    target - 1
                } else {
                    target
                };
                let cells = screen.grid().row(previous).unwrap().cells();
                assert_eq!(cells[2].character(), Some(' '));
                assert_eq!(cells[2].style(), StyleId(3));
                assert_eq!(cells[3].character(), Some(' '));
                assert_eq!(cells[3].style(), StyleId(3));
            }
            assert_invariants(screen.grid());
        }
    }
    let mut narrow = Screen::new(1, 1).unwrap();
    narrow.write("x", CellWidth::Single, StyleId(1)).unwrap();
    let before = snapshot(narrow.grid());
    let cursor = narrow.cursor();
    assert!(matches!(
        narrow.write("界", CellWidth::Double, StyleId(2)),
        Err(StorageError::OutOfBounds)
    ));
    assert_eq!(snapshot(narrow.grid()), before);
    assert_eq!(narrow.cursor(), cursor);
}

#[test]
fn controls_and_resize_cancel_wrapping_without_losing_cursor_bounds() {
    let mut screen = Screen::new(3, 2).unwrap();
    screen.write_ascii(b"abc", StyleId(1)).unwrap();
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 0,
            column: 2,
            wrap_pending: true
        }
    );
    screen.resize(3, 2, StyleId(9)).unwrap();
    assert!(screen.cursor().wrap_pending);
    screen.backspace();
    screen.write("X", CellWidth::Single, StyleId(2)).unwrap();
    assert_eq!(
        screen.grid().row(0).unwrap().text(1),
        Some(CellText::Scalar('X'))
    );
    screen.line_feed(StyleId(3));
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 1,
            column: 2,
            wrap_pending: false
        }
    );
    screen.write("Y", CellWidth::Single, StyleId(4)).unwrap();
    screen.line_feed(StyleId(5));
    assert_eq!(
        screen.grid().row(0).unwrap().text(2),
        Some(CellText::Scalar('Y'))
    );
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 1,
            column: 2,
            wrap_pending: false
        }
    );
    screen.carriage_return();
    screen.backspace();
    screen.write_ascii(b"end", StyleId(6)).unwrap();
    let cursor = screen.cursor();
    let before = snapshot(screen.grid());
    assert!(screen.set_cursor(usize::MAX, 0).is_err());
    assert!(screen.resize(0, 1, StyleId(7)).is_err());
    assert!(screen.write("", CellWidth::Single, StyleId(7)).is_err());
    assert_eq!(screen.cursor(), cursor);
    assert_eq!(snapshot(screen.grid()), before);
    screen.resize(2, 1, StyleId(7)).unwrap();
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 0,
            column: 1,
            wrap_pending: false
        }
    );
    assert_invariants(screen.grid());
}
