use super::{assert_invariants, snapshot};
use crate::{CellText, CellWidth, Cursor, Screen, StorageError, StyleId};

#[test]
fn ascii_batches_match_unit_writes_across_wraps_and_scrolls() {
    let input = b"abcdefghijklmnopqrstuvwxyz0123456789";
    for columns in 1..=12 {
        for rows in 1..=3 {
            for region in (0..rows).flat_map(|start| (start + 1..=rows).map(move |end| start..end))
            {
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
                            screen.set_scroll_region(region.clone()).unwrap();
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
    let position = screen.cursor();
    screen.set_scroll_region(0..1).unwrap();
    assert_eq!(
        (screen.cursor().row, screen.cursor().column),
        (position.row, position.column)
    );
    assert!(!screen.cursor().wrap_pending);
    screen.write("Z", CellWidth::Single, StyleId(6)).unwrap();
    let cursor = screen.cursor();
    let before = snapshot(screen.grid());
    for region in [
        0..0,
        2..2,
        std::ops::Range { start: 2, end: 1 },
        0..3,
        0..usize::MAX,
    ] {
        assert!(matches!(
            screen.set_scroll_region(region),
            Err(StorageError::OutOfBounds)
        ));
    }
    assert!(screen.set_cursor(usize::MAX, 0).is_err());
    assert!(screen.resize(0, 1, StyleId(7)).is_err());
    assert!(screen.write("", CellWidth::Single, StyleId(7)).is_err());
    assert_eq!(screen.cursor(), cursor);
    assert_eq!(snapshot(screen.grid()), before);
    assert_eq!(screen.scroll_region(), 0..1);
    screen.resize(3, 2, StyleId(7)).unwrap();
    assert_eq!(screen.cursor(), cursor);
    screen.resize(4, 2, StyleId(7)).unwrap();
    assert_eq!(screen.scroll_region(), 0..1);
    assert!(!screen.cursor().wrap_pending);
    screen.resize(4, 3, StyleId(7)).unwrap();
    assert_eq!(screen.scroll_region(), 0..3);
    screen.resize(2, 1, StyleId(7)).unwrap();
    assert_eq!(screen.scroll_region(), 0..1);
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

#[test]
fn regional_indices_preserve_outside_rows_and_recycle_owned_buffers() {
    for rows in 1..=6 {
        for offset in 0..rows {
            for region in (0..rows).flat_map(|start| (start + 1..=rows).map(move |end| start..end))
            {
                for cursor_row in 0..rows {
                    for up in [true, false] {
                        let mut screen = Screen::new(4, rows).unwrap();
                        screen.set_cursor(rows - 1, 0).unwrap();
                        for _ in 0..offset {
                            screen.line_feed(StyleId(0));
                        }
                        for row in 0..rows {
                            screen.set_cursor(row, 0).unwrap();
                            screen
                                .write_ascii(&[b'a' + row as u8; 4], StyleId(row as u32))
                                .unwrap();
                            screen.set_cursor(row, 1).unwrap();
                            screen
                                .write("水\u{301}", CellWidth::Double, StyleId(row as u32))
                                .unwrap();
                        }
                        screen.set_scroll_region(region.clone()).unwrap();
                        screen.set_cursor(cursor_row, 3).unwrap();
                        screen.write("!", CellWidth::Single, StyleId(8)).unwrap();
                        let before = snapshot(screen.grid());
                        let buffers: Vec<_> = screen
                            .grid()
                            .iter_rows()
                            .map(|row| {
                                let Some(CellText::Extended(text)) = row.text(1) else {
                                    unreachable!()
                                };
                                (row.cells().as_ptr(), text.as_ptr())
                            })
                            .collect();
                        let scroll = cursor_row == if up { region.end - 1 } else { region.start };
                        if up {
                            screen.line_feed(StyleId(9));
                        } else {
                            screen.reverse_index(StyleId(9));
                        }
                        let target_row = if scroll {
                            cursor_row
                        } else if up {
                            (cursor_row + 1).min(rows - 1)
                        } else {
                            cursor_row.saturating_sub(1)
                        };
                        assert_eq!(
                            screen.cursor(),
                            Cursor {
                                row: target_row,
                                column: 3,
                                wrap_pending: false
                            }
                        );
                        assert_eq!(screen.scroll_region(), region);
                        let after = snapshot(screen.grid());
                        for row in 0..rows {
                            let inside = scroll && region.contains(&row);
                            let source = if !inside {
                                Some(row)
                            } else if up {
                                (row + 1 < region.end).then_some(row + 1)
                            } else {
                                row.checked_sub(1).filter(|&source| source >= region.start)
                            };
                            let buffer = if !inside {
                                row
                            } else if up {
                                region.start + (row - region.start + 1) % region.len()
                            } else {
                                region.start
                                    + (row - region.start + region.len() - 1) % region.len()
                            };
                            assert_eq!(
                                screen.grid().row(row).unwrap().cells().as_ptr(),
                                buffers[buffer].0
                            );
                            if let Some(source) = source {
                                assert_eq!(after.rows[row], before.rows[source]);
                                let Some(CellText::Extended(text)) =
                                    screen.grid().row(row).unwrap().text(1)
                                else {
                                    panic!("Moved text is present")
                                };
                                assert_eq!(text.as_ptr(), buffers[source].1);
                            } else {
                                assert!(after.rows[row].iter().all(|cell| cell.text == " "
                                    && cell.width == 1
                                    && cell.style == StyleId(9)));
                            }
                        }
                        assert_invariants(screen.grid());
                    }
                }
            }
        }
    }
}

#[test]
fn regional_wide_wraps_preserve_units_and_handle_the_physical_bottom() {
    let mut screen = Screen::new(2, 4).unwrap();
    screen.set_cursor(3, 0).unwrap();
    screen.line_feed(StyleId(0));
    screen.line_feed(StyleId(0));
    screen.set_cursor(0, 0).unwrap();
    screen.write_ascii(b"abcdefgh", StyleId(1)).unwrap();
    screen.set_scroll_region(1..3).unwrap();
    screen.set_cursor(2, 1).unwrap();
    let before = snapshot(screen.grid());
    let outside = [0, 3].map(|row| screen.grid().row(row).unwrap().cells().as_ptr());
    screen
        .write("水\u{301}", CellWidth::Double, StyleId(2))
        .unwrap();
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 2,
            column: 1,
            wrap_pending: true
        }
    );
    assert_eq!(
        screen.grid().row(1).unwrap().text(0),
        Some(CellText::Scalar('e'))
    );
    assert_eq!(
        screen.grid().row(1).unwrap().text(1),
        Some(CellText::Scalar(' '))
    );
    assert_eq!(
        screen.grid().row(2).unwrap().text(0),
        Some(CellText::Extended("水\u{301}"))
    );
    screen.write("新", CellWidth::Double, StyleId(3)).unwrap();
    assert_eq!(
        screen.grid().row(1).unwrap().text(0),
        Some(CellText::Extended("水\u{301}"))
    );
    let after = snapshot(screen.grid());
    for (row, buffer) in [0, 3].into_iter().zip(outside) {
        assert_eq!(before.rows[row], after.rows[row]);
        assert_eq!(screen.grid().row(row).unwrap().cells().as_ptr(), buffer);
    }
    screen.set_cursor(3, 1).unwrap();
    let before = snapshot(screen.grid());
    screen.write("外", CellWidth::Double, StyleId(4)).unwrap();
    assert_eq!(
        screen.grid().row(3).unwrap().text(0),
        Some(CellText::Scalar('外'))
    );
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 3,
            column: 1,
            wrap_pending: true
        }
    );
    assert_eq!(snapshot(screen.grid()).rows[..3], before.rows[..3]);
    screen.set_scroll_region(1..2).unwrap();
    screen.set_cursor(1, 1).unwrap();
    screen.write("月", CellWidth::Double, StyleId(5)).unwrap();
    assert_eq!(
        screen.grid().row(1).unwrap().text(0),
        Some(CellText::Scalar('月'))
    );
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 1,
            column: 1,
            wrap_pending: true
        }
    );
    assert_invariants(screen.grid());
}
