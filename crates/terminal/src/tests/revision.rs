use super::{assert_invariants, snapshot};
use crate::{
    CellPosition, CellText, CellWidth, Cursor, Screen, ScreenBuffer, StorageError, StyleId,
};

#[test]
fn revisions_change_only_the_required_columns_and_preserve_equal_width_joins() {
    for insert in [false, true] {
        let mut screen = Screen::new(8, 3).unwrap();
        screen.write_ascii(b"abcdefgh", StyleId(0)).unwrap();
        screen.set_cursor(0, 1);
        screen.set_insert_mode(insert);
        screen.write("❤", CellWidth::Single, StyleId(1)).unwrap();
        let position = CellPosition { row: 0, column: 1 };
        assert_eq!(
            screen
                .revise_unit(position, "❤\u{fe0f}", CellWidth::Double, StyleId(2))
                .unwrap(),
            position
        );
        let row = screen.grid().row(0).unwrap();
        assert_eq!(row.text(1), Some(CellText::Extended("❤\u{fe0f}")));
        assert!(row.cells()[2].is_continuation());
        assert_eq!(
            row.text(3),
            Some(CellText::Scalar(if insert { 'b' } else { 'd' }))
        );
        assert_eq!(
            screen.cursor(),
            Cursor {
                row: 0,
                column: 3,
                wrap_pending: false
            }
        );
        screen
            .revise_unit(position, "❤\u{fe0e}", CellWidth::Single, StyleId(3))
            .unwrap();
        let row = screen.grid().row(0).unwrap();
        if insert {
            assert_eq!(row.text(2), Some(CellText::Scalar('b')));
            assert_eq!(row.text(6), Some(CellText::Scalar('f')));
            assert!(row.cells()[7].is_erased());
        } else {
            assert!(row.cells()[2].is_erased());
            assert_eq!(row.text(3), Some(CellText::Scalar('d')));
        }
        assert_eq!(
            screen.cursor(),
            Cursor {
                row: 0,
                column: 2,
                wrap_pending: false
            }
        );
        assert_invariants(screen.grid());
    }

    let mut screen = Screen::new(4, 2).unwrap();
    screen.write_ascii(b"ABCDE", StyleId(1)).unwrap();
    let position = CellPosition { row: 0, column: 3 };
    screen
        .revise_unit(position, "e\u{301}", CellWidth::Single, StyleId(2))
        .unwrap();
    assert_eq!(screen.grid().row(0).unwrap().wrap_end(), Some(4));
    assert!(screen.cursor().wrap_pending);
    let position = screen
        .revise_unit(position, "❤\u{fe0f}", CellWidth::Double, StyleId(3))
        .unwrap();
    assert_eq!(position, CellPosition { row: 1, column: 0 });
    assert_eq!(screen.grid().row(0).unwrap().wrap_end(), Some(3));
    assert!(screen.grid().row(0).unwrap().cells()[3].is_erased());
    screen
        .revise_unit(position, "❤\u{fe0f}\u{301}", CellWidth::Double, StyleId(3))
        .unwrap();
    assert_eq!(screen.grid().row(0).unwrap().wrap_end(), Some(3));
    assert_invariants(screen.grid());
}

#[test]
fn edge_growth_relocates_whole_units_through_regions_and_both_buffers() {
    for (columns, rows, region, source, origin) in [
        (4, 5, 1..4, 3, true),
        (4, 5, 1..4, 2, true),
        (2, 1, 0..1, 0, false),
        (4, 3, 1..2, 1, true),
        (4, 4, 0..2, 3, false),
    ] {
        for insert in [false, true] {
            for buffer in [ScreenBuffer::Primary, ScreenBuffer::Alternate] {
                let mut screen = Screen::new(columns, rows).unwrap();
                screen.select_buffer(buffer);
                for row in 0..rows {
                    screen.set_cursor(row, 0);
                    screen
                        .write_ascii(&vec![b'A' + row as u8; columns], StyleId(1))
                        .unwrap();
                }
                screen.set_scroll_region(region.clone()).unwrap();
                screen.set_origin_mode(origin);
                screen.set_cursor(
                    if origin {
                        source - region.start
                    } else {
                        source
                    },
                    columns - 1,
                );
                screen.set_insert_mode(insert);
                screen.write("❤", CellWidth::Single, StyleId(2)).unwrap();
                screen.save_cursor();
                let saved = screen.cursor();
                let before = snapshot(screen.grid());
                let inactive = if buffer == ScreenBuffer::Primary {
                    ScreenBuffer::Alternate
                } else {
                    ScreenBuffer::Primary
                };
                let other = snapshot(screen.buffer(inactive));
                let result = screen
                    .revise_unit(
                        CellPosition {
                            row: source,
                            column: columns - 1,
                        },
                        "❤\u{fe0f}",
                        CellWidth::Double,
                        StyleId(7),
                    )
                    .unwrap();
                let scrolling = source == region.end - 1;
                let target = if scrolling {
                    source
                } else {
                    (source + 1).min(rows - 1)
                };
                assert_eq!(
                    result,
                    CellPosition {
                        row: target,
                        column: 0
                    }
                );
                assert_eq!(
                    screen.grid().row(target).unwrap().text(0),
                    Some(CellText::Extended("❤\u{fe0f}"))
                );
                assert_eq!(screen.grid().row(target).unwrap().cells()[0].width(), 2);
                assert_eq!(
                    screen.cursor(),
                    Cursor {
                        row: target,
                        column: 2.min(columns - 1),
                        wrap_pending: columns == 2
                    }
                );
                if scrolling && region.len() > 1 {
                    assert_eq!(
                        screen.grid().row(target - 1).unwrap().wrap_end(),
                        Some(columns - 1)
                    );
                    assert!(
                        screen.grid().row(target - 1).unwrap().cells()[columns - 1].is_erased()
                    );
                } else if source != target {
                    assert_eq!(
                        screen.grid().row(source).unwrap().wrap_end(),
                        Some(columns - 1)
                    );
                    assert!(screen.grid().row(source).unwrap().cells()[columns - 1].is_erased());
                }
                let after = snapshot(screen.grid());
                for row in 0..rows {
                    if (scrolling && !region.contains(&row))
                        || (!scrolling && row != source && row != target)
                    {
                        assert_eq!(after.rows[row], before.rows[row]);
                    }
                }
                assert_eq!(snapshot(screen.buffer(inactive)), other);
                assert_eq!(screen.scroll_region(), region);
                assert_eq!(screen.origin_mode(), origin);
                assert_eq!(screen.insert_mode(), insert);
                assert_invariants(screen.grid());
                screen.restore_cursor();
                assert_eq!(screen.cursor(), saved);
            }
        }
    }
}

#[test]
fn rejected_revisions_preserve_cursor_both_buffers_and_connections() {
    let mut screen = Screen::new(4, 4).unwrap();
    screen.write_ascii(b"ABCDE", StyleId(1)).unwrap();
    screen.set_cursor(2, 1);
    screen.write("水", CellWidth::Double, StyleId(2)).unwrap();
    let long = "x".repeat(crate::MAX_CELL_TEXT_BYTES + 1);
    for (position, text, width) in [
        (CellPosition { row: 4, column: 0 }, "x", CellWidth::Single),
        (
            CellPosition {
                row: 0,
                column: usize::MAX,
            },
            "x",
            CellWidth::Double,
        ),
        (CellPosition { row: 2, column: 2 }, "x", CellWidth::Single),
        (CellPosition { row: 0, column: 3 }, "", CellWidth::Double),
        (
            CellPosition { row: 0, column: 3 },
            long.as_str(),
            CellWidth::Double,
        ),
    ] {
        let before = snapshot(screen.grid());
        let cursor = screen.cursor();
        assert!(
            screen
                .revise_unit(position, text, width, StyleId(9))
                .is_err()
        );
        assert_eq!(snapshot(screen.grid()), before);
        assert_eq!(screen.cursor(), cursor);
    }
    screen.set_auto_wrap(false);
    let before = snapshot(screen.grid());
    let cursor = screen.cursor();
    assert!(matches!(
        screen.revise_unit(
            CellPosition { row: 0, column: 3 },
            "❤\u{fe0f}",
            CellWidth::Double,
            StyleId(9)
        ),
        Err(StorageError::OutOfBounds)
    ));
    assert_eq!(snapshot(screen.grid()), before);
    assert_eq!(screen.cursor(), cursor);
    screen.set_scroll_region(1..3).unwrap();
    screen.set_origin_mode(true);
    assert!(
        screen
            .revise_unit(
                CellPosition { row: 0, column: 0 },
                "x",
                CellWidth::Single,
                StyleId(0)
            )
            .is_err()
    );
    let mut narrow = Screen::new(1, 1).unwrap();
    narrow.write("x", CellWidth::Single, StyleId(1)).unwrap();
    let before = snapshot(narrow.grid());
    let cursor = narrow.cursor();
    assert!(
        narrow
            .revise_unit(
                CellPosition { row: 0, column: 0 },
                "水",
                CellWidth::Double,
                StyleId(0)
            )
            .is_err()
    );
    assert_eq!(snapshot(narrow.grid()), before);
    assert_eq!(narrow.cursor(), cursor);
}
