use super::{assert_invariants, snapshot};
use crate::{
    CellText, CellWidth, Cursor, CursorAppearance, CursorShape, EraseMode, Screen, StorageError,
    StyleId,
};

#[test]
fn cursor_appearance_preserves_placement_saved_state_and_resizing_until_reset() {
    let mut screen = Screen::new(4, 4).unwrap();
    let initial = CursorAppearance {
        visible: true,
        blinking: true,
        shape: CursorShape::Block,
    };
    assert_eq!(screen.cursor_appearance(), initial);
    screen.write_ascii(b"abcd", StyleId(1)).unwrap();
    screen.save_cursor();
    let cursor = screen.cursor();
    let contents = snapshot(screen.grid());
    let hidden = CursorAppearance {
        visible: false,
        blinking: false,
        shape: CursorShape::Bar,
    };
    screen.set_cursor_appearance(hidden);
    assert_eq!(screen.cursor(), cursor);
    assert_eq!(snapshot(screen.grid()), contents);
    screen.carriage_return();
    screen.restore_cursor();
    assert_eq!(screen.cursor(), cursor);
    assert_eq!(screen.cursor_appearance(), hidden);
    screen.resize(3, 2, StyleId(0)).unwrap();
    screen.restore_cursor();
    assert_eq!(screen.cursor_appearance(), hidden);
    let checkpoint = snapshot(screen.grid());
    assert!(screen.resize(0, 2, StyleId(0)).is_err());
    assert_eq!(snapshot(screen.grid()), checkpoint);
    assert_eq!(screen.cursor_appearance(), hidden);
    screen.reset(StyleId(0));
    assert_eq!(screen.cursor_appearance(), initial);
}

#[test]
fn soft_wraps_record_actual_joins_and_invalidate_edited_boundaries() {
    let mut screen = Screen::new(4, 4).unwrap();
    screen.write_ascii(b"abcd", StyleId(1)).unwrap();
    assert_eq!(screen.grid().row(0).unwrap().wrap_end(), None);
    let edge = screen.grid().row(0).unwrap().cells()[3];
    let pending = screen.cursor();
    screen.write_ascii(b"", StyleId(1)).unwrap();
    assert_eq!(screen.cursor(), pending);
    assert!(screen.write("", CellWidth::Single, StyleId(1)).is_err());
    assert_eq!(screen.grid().row(0).unwrap().wrap_end(), None);
    screen.write("x", CellWidth::Single, StyleId(1)).unwrap();
    assert_eq!(screen.grid().row(0).unwrap().wrap_end(), Some(4));
    assert_eq!(screen.grid().row(0).unwrap().cells()[3], edge);
    screen.set_cursor(0, 1);
    screen.write("y", CellWidth::Single, StyleId(2)).unwrap();
    assert_eq!(screen.grid().row(0).unwrap().wrap_end(), Some(4));
    screen.set_cursor(0, 3);
    screen.write("z", CellWidth::Single, StyleId(2)).unwrap();
    assert_eq!(screen.grid().row(0).unwrap().wrap_end(), None);
    screen.write("x", CellWidth::Single, StyleId(2)).unwrap();
    screen.set_cursor(0, 0);
    screen.line_feed(StyleId(0)).unwrap();
    assert_eq!(screen.grid().row(0).unwrap().wrap_end(), None);

    let mut grid = crate::Grid::new(4, 2).unwrap();
    grid.write(0, 2, "水", CellWidth::Double, StyleId(1))
        .unwrap();
    grid.set_wrap_end(0, Some(4));
    grid.write(0, 2, "x", CellWidth::Single, StyleId(1))
        .unwrap();
    assert_eq!(grid.row(0).unwrap().wrap_end(), None);
    grid.set_wrap_end(0, Some(3));
    grid.append_text(0, 0, "\u{301}").unwrap();
    assert_eq!(grid.row(0).unwrap().wrap_end(), Some(3));
    grid.append_text(0, 3, "\u{301}").unwrap();
    assert_eq!(grid.row(0).unwrap().wrap_end(), None);

    for insert in [false, true] {
        screen.reset(StyleId(0));
        screen.set_insert_mode(insert);
        screen.write_ascii(b"abc", StyleId(1)).unwrap();
        screen.write("水", CellWidth::Double, StyleId(2)).unwrap();
        assert_eq!(screen.grid().row(0).unwrap().wrap_end(), Some(3));
        assert_eq!(
            screen.grid().row(0).unwrap().text(3),
            Some(CellText::Scalar(' '))
        );
        assert_invariants(screen.grid());
        let before = snapshot(screen.grid());
        assert!(screen.resize(0, 4, StyleId(0)).is_err());
        assert!(screen.write_ascii(b"invalid\n", StyleId(0)).is_err());
        assert_eq!(snapshot(screen.grid()), before);
        screen.resize(4, 4, StyleId(0)).unwrap();
        assert_eq!(snapshot(screen.grid()), before);
        screen.resize(4, 2, StyleId(0)).unwrap();
        assert_eq!(screen.grid().row(0).unwrap().wrap_end(), Some(3));
        screen.resize(4, 1, StyleId(0)).unwrap();
        screen.resize(4, 4, StyleId(0)).unwrap();
        assert!(
            screen
                .grid()
                .iter_rows()
                .all(|row| row.wrap_end().is_none())
        );
    }

    screen.reset(StyleId(0));
    screen
        .write_ascii(b"abcdefghijklmnopq", StyleId(1))
        .unwrap();
    screen.set_cursor(1, 0);
    screen.erase_line(EraseMode::All, StyleId(0));
    assert_eq!(screen.grid().row(0).unwrap().wrap_end(), None);
    assert_eq!(screen.grid().row(1).unwrap().wrap_end(), None);
    assert_eq!(screen.grid().row(2).unwrap().wrap_end(), Some(4));
    screen.set_cursor(2, 0);
    screen.insert_cells(0, StyleId(0));
    assert_eq!(screen.grid().row(2).unwrap().wrap_end(), Some(4));
    screen.delete_cells(1, StyleId(0));
    assert_eq!(screen.grid().row(2).unwrap().wrap_end(), None);
    screen.set_cursor(0, 3);
    screen.write_ascii(b"xy", StyleId(1)).unwrap();
    screen.resize(5, 4, StyleId(0)).unwrap();
    assert!(
        screen
            .grid()
            .iter_rows()
            .all(|row| row.wrap_end().is_none())
    );
    screen
        .write_ascii(b"abcdefghijklmnopqrst", StyleId(1))
        .unwrap();
    screen.reset(StyleId(0));
    assert!(
        screen
            .grid()
            .iter_rows()
            .all(|row| row.wrap_end().is_none())
    );
}

#[test]
fn scrolling_preserves_only_connections_between_retained_adjacent_rows() {
    for rows in 1..=8 {
        for start in 0..=rows {
            for end in start..=rows {
                for count in [0, 1, 2, rows, usize::MAX] {
                    for up in [false, true] {
                        let mut grid = crate::Grid::new(4, rows).unwrap();
                        for row in 0..rows - 1 {
                            grid.set_wrap_end(row, Some(4 - row % 2));
                        }
                        let original: Vec<_> = grid.iter_rows().map(|row| row.wrap_end()).collect();
                        if up {
                            grid.scroll_up(start..end, count, StyleId(0)).unwrap();
                        } else {
                            grid.scroll_down(start..end, count, StyleId(0)).unwrap();
                        }
                        let count = count.min(end - start);
                        let source = |row: usize| {
                            if count == 0 || row < start || row >= end {
                                Some(row)
                            } else if up {
                                (row + count < end).then_some(row + count)
                            } else {
                                row.checked_sub(count).filter(|&row| row >= start)
                            }
                        };
                        for row in 0..rows {
                            let expected = if row + 1 < rows {
                                match (source(row), source(row + 1)) {
                                    (Some(a), Some(b)) if a + 1 == b => original[a],
                                    _ => None,
                                }
                            } else {
                                None
                            };
                            assert_eq!(
                                grid.row(row).unwrap().wrap_end(),
                                expected,
                                "rows={rows}, region={start}..{end}, count={count}, up={up}, row={row}"
                            );
                        }
                        assert_invariants(&grid);
                    }
                }
            }
        }
    }
}

#[test]
fn insertion_writes_shift_complete_units_and_preserve_mode_boundaries() {
    let mut screen = Screen::new(8, 3).unwrap();
    screen.write_ascii(b"abcdefgh", StyleId(1)).unwrap();
    let pending = screen.cursor();
    screen.set_insert_mode(true);
    assert_eq!(screen.cursor(), pending);
    screen.set_cursor(0, 2);
    screen.write_ascii(b"XY", StyleId(2)).unwrap();
    for (column, byte) in b"abXYcdef".iter().enumerate() {
        assert_eq!(
            screen.grid().row(0).unwrap().text(column),
            Some(CellText::Scalar(char::from(*byte)))
        );
    }
    screen.set_cursor(0, 3);
    screen
        .write("水\u{301}", CellWidth::Double, StyleId(3))
        .unwrap();
    let row = screen.grid().row(0).unwrap();
    assert_eq!(row.text(3), Some(CellText::Extended("水\u{301}")));
    assert_eq!(row.text(4), Some(CellText::Continuation));
    assert_eq!(row.text(5), Some(CellText::Scalar('Y')));
    assert_eq!(row.text(7), Some(CellText::Scalar('d')));
    let before = snapshot(screen.grid());
    let cursor = screen.cursor();
    assert!(screen.write_ascii(b"valid\n", StyleId(4)).is_err());
    assert!(screen.write("", CellWidth::Single, StyleId(4)).is_err());
    assert_eq!(snapshot(screen.grid()), before);
    assert_eq!(screen.cursor(), cursor);
    screen.save_cursor();
    screen.set_insert_mode(false);
    screen.restore_cursor();
    assert!(!screen.insert_mode());
    screen.set_insert_mode(true);
    screen.resize(9, 3, StyleId(0)).unwrap();
    assert!(screen.insert_mode());
    screen.reset(StyleId(0));
    assert!(!screen.insert_mode());

    let mut bottom = Screen::new(4, 3).unwrap();
    bottom.set_scroll_region(0..1).unwrap();
    bottom.set_cursor(2, 0);
    bottom.write_ascii(b"abcd", StyleId(1)).unwrap();
    bottom.set_cursor(2, 3);
    bottom.set_insert_mode(true);
    bottom.write("水", CellWidth::Double, StyleId(2)).unwrap();
    assert_eq!(
        bottom.grid().row(2).unwrap().text(2),
        Some(CellText::Scalar('a'))
    );
    assert_eq!(
        bottom.grid().row(2).unwrap().text(3),
        Some(CellText::Scalar('b'))
    );
    assert_invariants(bottom.grid());
    assert_invariants(screen.grid());
}

#[test]
fn reset_clears_owned_units_restores_state_and_reuses_wrapped_row_buffers() {
    for columns in [1, 8, 63, 64, 65, 129] {
        for rows in [1, 4] {
            let mut screen = Screen::new(columns + 8, rows).unwrap();
            screen.resize(columns, rows, StyleId(0)).unwrap();
            screen.set_cursor(rows - 1, 0);
            screen.line_feed(StyleId(1)).unwrap();
            for row in 0..rows {
                screen.set_cursor(row, 0);
                screen
                    .write("e\u{301}", CellWidth::Single, StyleId(2))
                    .unwrap();
                if columns > 1 {
                    screen.set_cursor(row, columns - 2);
                    screen
                        .write("水\u{301}", CellWidth::Double, StyleId(3))
                        .unwrap();
                }
            }
            screen.set_scroll_region(rows / 2..rows).unwrap();
            screen.set_origin_mode(true);
            screen.set_cursor(usize::MAX, usize::MAX);
            screen.write("!", CellWidth::Single, StyleId(4)).unwrap();
            screen.save_cursor();
            screen.clear_tab_stops();
            screen.set_tab_stop();
            screen.set_auto_wrap(false);
            let buffers: Vec<_> = screen
                .grid()
                .iter_rows()
                .map(|row| row.cells().as_ptr())
                .collect();
            for style in [StyleId(7), StyleId(9)] {
                screen.reset(style);
                assert_eq!(
                    (screen.grid().columns(), screen.grid().rows()),
                    (columns, rows)
                );
                assert_eq!(screen.cursor(), Cursor::default());
                assert_eq!(screen.scroll_region(), 0..rows);
                assert_eq!((screen.auto_wrap(), screen.origin_mode()), (true, false));
                for (row, buffer) in screen.grid().iter_rows().zip(&buffers) {
                    assert_eq!(row.cells().as_ptr(), *buffer);
                    for column in 0..columns {
                        assert_eq!(row.text(column), Some(CellText::Scalar(' ')));
                        assert_eq!(
                            (row.cells()[column].width(), row.cells()[column].style()),
                            (1, style)
                        );
                    }
                }
                for column in 0..columns {
                    assert_eq!(screen.is_tab_stop(column), Some(column % 8 == 0));
                }
                screen.tab_forward(columns.div_ceil(8));
                assert_eq!(screen.cursor().column, columns - 1);
                screen.tab_backward(usize::MAX);
                screen.tab_forward(usize::MAX);
                assert_eq!(screen.cursor().column, columns - 1);
                screen.tab_backward(usize::MAX);
                assert_eq!(screen.cursor().column, 0);
                screen.set_origin_mode(true);
                screen.set_auto_wrap(false);
                screen.set_cursor(usize::MAX, usize::MAX);
                screen.restore_cursor();
                assert_eq!(screen.cursor(), Cursor::default());
                assert_eq!((screen.auto_wrap(), screen.origin_mode()), (true, false));
                assert_invariants(screen.grid());
            }
            screen.resize(columns + 8, rows, StyleId(0)).unwrap();
            screen.set_cursor(0, columns + 7);
            screen.write("!", CellWidth::Single, StyleId(1)).unwrap();
            screen.write("X", CellWidth::Single, StyleId(2)).unwrap();
            assert_eq!(screen.cursor().row, usize::from(rows > 1));
            assert_invariants(screen.grid());
        }
    }
}

#[test]
fn saved_cursor_restores_modes_relative_rows_and_pending_writes_without_consuming_the_slot() {
    let mut initial = Screen::new(4, 3).unwrap();
    initial.set_scroll_region(1..3).unwrap();
    initial.set_origin_mode(true);
    initial.set_auto_wrap(false);
    initial.set_cursor(usize::MAX, usize::MAX);
    initial.restore_cursor();
    assert_eq!(initial.cursor(), Cursor::default());
    assert!(initial.auto_wrap());
    assert!(!initial.origin_mode());
    assert_eq!(initial.scroll_region(), 1..3);

    for columns in [1, 4] {
        for origin in [false, true] {
            for (auto, pending) in [(false, false), (true, false), (true, true)] {
                let mut screen = Screen::new(columns, 6).unwrap();
                if columns > 1 {
                    screen.set_cursor(2, 0);
                    screen
                        .write("水\u{301}", CellWidth::Double, StyleId(1))
                        .unwrap();
                }
                screen.set_scroll_region(1..5).unwrap();
                screen.set_origin_mode(origin);
                screen.set_auto_wrap(auto);
                screen.set_cursor(
                    if origin { 1 } else { 2 },
                    if pending { columns - 1 } else { 1 },
                );
                if pending {
                    screen.write("!", CellWidth::Single, StyleId(2)).unwrap();
                }
                let saved = screen.cursor();
                let contents = snapshot(screen.grid());
                screen.save_cursor();
                assert_eq!(screen.cursor(), saved);
                screen.set_origin_mode(!origin);
                screen.set_auto_wrap(!auto);
                screen.set_scroll_region(2..6).unwrap();
                screen.clear_tab_stops();
                screen.set_tab_stop();
                let tabs: Vec<_> = (0..columns)
                    .map(|column| screen.is_tab_stop(column))
                    .collect();
                let expected = Cursor {
                    row: if origin { 3 } else { 2 },
                    ..saved
                };
                for _ in 0..2 {
                    screen.restore_cursor();
                    assert_eq!(screen.cursor(), expected);
                    assert_eq!((screen.origin_mode(), screen.auto_wrap()), (origin, auto));
                    assert_eq!(screen.scroll_region(), 2..6);
                    assert_eq!(snapshot(screen.grid()), contents);
                    assert_eq!(
                        (0..columns)
                            .map(|column| screen.is_tab_stop(column))
                            .collect::<Vec<_>>(),
                        tabs
                    );
                    screen.carriage_return();
                    screen.set_origin_mode(!origin);
                    screen.set_auto_wrap(!auto);
                }
                screen.restore_cursor();
                screen.write("X", CellWidth::Single, StyleId(3)).unwrap();
                assert_eq!(screen.cursor().row, expected.row + usize::from(pending));
                let written = snapshot(screen.grid());
                screen.restore_cursor();
                assert_eq!(screen.cursor(), expected);
                assert_eq!(snapshot(screen.grid()), written);

                screen.set_cursor(0, 0);
                screen.set_auto_wrap(false);
                let replacement = screen.cursor();
                screen.save_cursor();
                screen.set_cursor(usize::MAX, usize::MAX);
                screen.set_auto_wrap(true);
                screen.restore_cursor();
                assert_eq!(screen.cursor(), replacement);
                assert!(!screen.auto_wrap());
                assert_invariants(screen.grid());
            }
        }
    }
}

#[test]
fn saved_cursor_survives_resizing_and_cancels_pending_wrapping_when_clamped() {
    for origin in [false, true] {
        let mut screen = Screen::new(4, 6).unwrap();
        screen.set_scroll_region(1..5).unwrap();
        screen.set_origin_mode(origin);
        screen.set_cursor(2, 3);
        screen.write("!", CellWidth::Single, StyleId(1)).unwrap();
        screen.save_cursor();
        for (columns, rows, region) in [
            (8, 6, 2..6),
            (2, 6, 2..6),
            (4, 2, 0..2),
            (4, 6, 2..6),
            (4, 6, 4..5),
            (4, 6, 1..5),
        ] {
            screen.set_origin_mode(!origin);
            screen.set_auto_wrap(false);
            screen.resize(columns, rows, StyleId(2)).unwrap();
            screen.set_scroll_region(region.clone()).unwrap();
            let contents = snapshot(screen.grid());
            screen.restore_cursor();
            let height = if origin { region.len() } else { rows };
            assert_eq!(
                screen.cursor(),
                Cursor {
                    row: if origin { region.start } else { 0 } + 2.min(height - 1),
                    column: 3.min(columns - 1),
                    wrap_pending: columns == 4 && height > 2,
                }
            );
            assert_eq!((screen.origin_mode(), screen.auto_wrap()), (origin, true));
            assert_eq!(screen.scroll_region(), region);
            assert_eq!(snapshot(screen.grid()), contents);
            assert_invariants(screen.grid());
        }
    }
}

#[test]
fn origin_addressing_clamps_and_homes_without_changing_storage_or_modes() {
    for columns in [1, 4] {
        for rows in 1..=5 {
            for region in (0..rows).flat_map(|start| (start + 1..=rows).map(move |end| start..end))
            {
                for enabled in [false, true] {
                    let mut screen = Screen::new(columns, rows).unwrap();
                    screen
                        .write("e\u{301}", CellWidth::Single, StyleId(1))
                        .unwrap();
                    if columns > 1 {
                        screen.set_cursor(rows - 1, columns - 2);
                        screen.write("水", CellWidth::Double, StyleId(2)).unwrap();
                    }
                    screen.clear_tab_stops();
                    screen.set_tab_stop();
                    let tabs: Vec<_> = (0..columns)
                        .map(|column| screen.is_tab_stop(column))
                        .collect();
                    let before = snapshot(screen.grid());
                    screen.set_scroll_region(region.clone()).unwrap();
                    screen.set_origin_mode(enabled);
                    let top = if enabled { region.start } else { 0 };
                    let bottom = if enabled { region.end - 1 } else { rows - 1 };
                    for row in [0, 1, bottom - top, rows, usize::MAX] {
                        for column in [0, columns - 1, columns, usize::MAX] {
                            screen.set_cursor(row, column);
                            assert_eq!(
                                screen.cursor(),
                                Cursor {
                                    row: top + row.min(bottom - top),
                                    column: column.min(columns - 1),
                                    wrap_pending: false,
                                }
                            );
                        }
                    }
                    assert_eq!(snapshot(screen.grid()), before);
                    screen.write("!", CellWidth::Single, StyleId(3)).unwrap();
                    assert!(screen.cursor().wrap_pending);
                    let written = snapshot(screen.grid());
                    screen.set_origin_mode(enabled);
                    assert_eq!(
                        screen.cursor(),
                        Cursor {
                            row: top,
                            ..Cursor::default()
                        }
                    );
                    assert_eq!(snapshot(screen.grid()), written);
                    screen.set_cursor(usize::MAX, usize::MAX);
                    screen.write("!", CellWidth::Single, StyleId(3)).unwrap();
                    screen.set_scroll_region(region.clone()).unwrap();
                    assert_eq!(
                        screen.cursor(),
                        Cursor {
                            row: top,
                            ..Cursor::default()
                        }
                    );
                    screen.set_auto_wrap(false);
                    screen.set_origin_mode(!enabled);
                    assert_eq!(
                        screen.cursor(),
                        Cursor {
                            row: if enabled { 0 } else { region.start },
                            ..Cursor::default()
                        }
                    );
                    assert!(!screen.auto_wrap());
                    assert_eq!(screen.scroll_region(), region);
                    assert_eq!(
                        (0..columns)
                            .map(|column| screen.is_tab_stop(column))
                            .collect::<Vec<_>>(),
                        tabs
                    );
                    assert_invariants(screen.grid());
                }
            }
        }
    }
}

#[test]
fn origin_writes_movement_and_resize_preserve_region_bounds_and_error_state() {
    let mut screen = Screen::new(4, 5).unwrap();
    for row in 0..5 {
        screen.set_cursor(row, 0);
        screen
            .write_ascii(&[b'a' + row as u8; 4], StyleId(row as u32))
            .unwrap();
    }
    let populated = snapshot(screen.grid());
    screen.set_scroll_region(1..4).unwrap();
    screen.set_origin_mode(true);
    for _ in 0..8 {
        screen.set_cursor(usize::MAX, usize::MAX);
        screen
            .write("水\u{301}", CellWidth::Double, StyleId(9))
            .unwrap();
        screen.write_ascii(b"abcdefghi", StyleId(8)).unwrap();
        screen.move_cursor_up(usize::MAX);
        assert_eq!(screen.cursor().row, 1);
        screen.reverse_index(StyleId(7));
        screen.move_cursor_down(usize::MAX);
        assert_eq!(screen.cursor().row, 3);
        screen.line_feed(StyleId(7)).unwrap();
    }
    let scrolled = snapshot(screen.grid());
    for row in [0, 4] {
        assert_eq!(scrolled.rows[row], populated.rows[row]);
    }
    screen.set_cursor(usize::MAX, usize::MAX);
    screen.write("!", CellWidth::Single, StyleId(3)).unwrap();
    let cursor = screen.cursor();
    screen.resize(4, 5, StyleId(0)).unwrap();
    assert_eq!(screen.cursor(), cursor);
    let before = snapshot(screen.grid());
    for region in [0..0, 0..6, 0..usize::MAX] {
        assert!(screen.set_scroll_region(region).is_err());
        assert_eq!(screen.cursor(), cursor);
        assert_eq!(screen.scroll_region(), 1..4);
    }
    assert!(screen.resize(0, 5, StyleId(0)).is_err());
    assert_eq!(screen.cursor(), cursor);
    assert_eq!(snapshot(screen.grid()), before);
    screen.resize(6, 5, StyleId(0)).unwrap();
    assert_eq!(screen.scroll_region(), 1..4);
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 3,
            column: 3,
            wrap_pending: false
        }
    );
    screen.set_cursor(usize::MAX, usize::MAX);
    screen.write("!", CellWidth::Single, StyleId(3)).unwrap();
    screen.resize(6, 2, StyleId(0)).unwrap();
    assert_eq!(screen.scroll_region(), 0..2);
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 1,
            column: 5,
            wrap_pending: false
        }
    );
    assert!(screen.origin_mode());
    screen.set_scroll_region(1..2).unwrap();
    screen.set_cursor(usize::MAX, usize::MAX);
    screen.move_cursor_up(usize::MAX);
    screen.reverse_index(StyleId(0));
    screen.write_ascii(b"single row wraps", StyleId(1)).unwrap();
    assert_eq!(screen.cursor().row, 1);
    assert_invariants(screen.grid());
}

#[test]
fn ascii_batches_match_unit_writes_across_wraps_and_scrolls() {
    let input = b"abcdefghijklmnopqrstuvwxyz0123456789";
    for columns in 1..=12 {
        for rows in 1..=3 {
            for region in (0..rows).flat_map(|start| (start + 1..=rows).map(move |end| start..end))
            {
                for row in 0..rows {
                    for (column, auto_wrap, insert_mode) in (0..=columns).flat_map(|column| {
                        [false, true].into_iter().flat_map(move |auto| {
                            [false, true].map(move |insert| (column, auto, insert))
                        })
                    }) {
                        let mut bulk = Screen::new(columns, rows).unwrap();
                        let mut scalar = Screen::new(columns, rows).unwrap();
                        for screen in [&mut bulk, &mut scalar] {
                            screen
                                .write("e\u{301}", CellWidth::Single, StyleId(1))
                                .unwrap();
                            if columns > 1 {
                                screen.set_cursor(rows - 1, columns - 2);
                                screen.write("界", CellWidth::Double, StyleId(2)).unwrap();
                            }
                            screen.set_scroll_region(region.clone()).unwrap();
                            screen.set_cursor(row, column.min(columns - 1));
                            if column == columns {
                                screen.write("!", CellWidth::Single, StyleId(3)).unwrap();
                            }
                            screen.set_auto_wrap(auto_wrap);
                            screen.set_insert_mode(insert_mode);
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
            screen.set_cursor(row, 2);
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

            screen.set_cursor(target, 2);
            screen.write("旧", CellWidth::Double, StyleId(1)).unwrap();
            screen.set_cursor(target, 3);
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
    screen.line_feed(StyleId(3)).unwrap();
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 1,
            column: 2,
            wrap_pending: false
        }
    );
    screen.write("Y", CellWidth::Single, StyleId(4)).unwrap();
    screen.line_feed(StyleId(5)).unwrap();
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
    screen.set_scroll_region(0..1).unwrap();
    assert_eq!(screen.cursor(), Cursor::default());
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
                        screen.set_cursor(rows - 1, 0);
                        for _ in 0..offset {
                            screen.line_feed(StyleId(0)).unwrap();
                        }
                        for row in 0..rows {
                            screen.set_cursor(row, 0);
                            screen
                                .write_ascii(&[b'a' + row as u8; 4], StyleId(row as u32))
                                .unwrap();
                            screen.set_cursor(row, 1);
                            screen
                                .write("水\u{301}", CellWidth::Double, StyleId(row as u32))
                                .unwrap();
                        }
                        screen.set_scroll_region(region.clone()).unwrap();
                        screen.set_cursor(cursor_row, 3);
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
                            screen.line_feed(StyleId(9)).unwrap();
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
    screen.set_cursor(3, 0);
    screen.line_feed(StyleId(0)).unwrap();
    screen.line_feed(StyleId(0)).unwrap();
    screen.set_cursor(0, 0);
    screen.write_ascii(b"abcdefgh", StyleId(1)).unwrap();
    screen.set_scroll_region(1..3).unwrap();
    screen.set_cursor(2, 1);
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
    screen.set_cursor(3, 1);
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
    screen.set_cursor(1, 1);
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

#[test]
fn erasure_clears_intersecting_units_and_preserves_screen_state_and_buffers() {
    for columns in 1..=9 {
        for rows in 1..=4 {
            for offset in 0..rows {
                for phase in 0..2 {
                    for cursor_row in 0..rows {
                        for cursor_column in 0..=columns {
                            for mode in [EraseMode::FromCursor, EraseMode::ToCursor, EraseMode::All]
                            {
                                for display in [false, true] {
                                    let mut screen = Screen::new(columns, rows).unwrap();
                                    screen.set_cursor(rows - 1, 0);
                                    for _ in 0..offset {
                                        screen.line_feed(StyleId(0)).unwrap();
                                    }
                                    for row in 0..rows {
                                        let mut column = 0;
                                        while column < columns {
                                            let width = if (column + phase) % 3 == 0
                                                && column + 1 < columns
                                            {
                                                CellWidth::Double
                                            } else {
                                                CellWidth::Single
                                            };
                                            let mut text =
                                                char::from(b'a' + column as u8).to_string();
                                            if (row + column + phase) % 2 == 0 {
                                                text.push('\u{301}');
                                            }
                                            screen.set_cursor(row, column);
                                            screen
                                                .write(
                                                    &text,
                                                    width,
                                                    StyleId((row * columns + column + 1) as u32),
                                                )
                                                .unwrap();
                                            column += width.columns();
                                        }
                                    }
                                    let region = if rows > 2 { 1..rows - 1 } else { 0..rows };
                                    screen.set_scroll_region(region.clone()).unwrap();
                                    screen.set_cursor(cursor_row, cursor_column.min(columns - 1));
                                    if cursor_column == columns {
                                        screen.write("!", CellWidth::Single, StyleId(42)).unwrap();
                                    }
                                    let cursor = screen.cursor();
                                    let before = snapshot(screen.grid());
                                    let buffers: Vec<_> = screen
                                        .grid()
                                        .iter_rows()
                                        .map(|row| row.cells().as_ptr())
                                        .collect();
                                    let text_buffers: Vec<Vec<_>> = screen
                                        .grid()
                                        .iter_rows()
                                        .map(|row| {
                                            (0..columns)
                                                .map(|column| match row.text(column) {
                                                    Some(CellText::Extended(text)) => {
                                                        Some(text.as_ptr())
                                                    }
                                                    _ => None,
                                                })
                                                .collect()
                                        })
                                        .collect();
                                    let style = StyleId(u32::MAX);
                                    let mut expected = before.rows.clone();
                                    let cursor_offset = cursor.row * columns + cursor.column;
                                    for (row, cells) in before.rows.iter().enumerate() {
                                        for (column, cell) in cells
                                            .iter()
                                            .enumerate()
                                            .filter(|(_, cell)| cell.width > 0)
                                        {
                                            let start = row * columns + column;
                                            let erase = (display || row == cursor.row)
                                                && match mode {
                                                    EraseMode::FromCursor => {
                                                        start + cell.width > cursor_offset
                                                    }
                                                    EraseMode::ToCursor => start <= cursor_offset,
                                                    EraseMode::All => true,
                                                };
                                            if erase {
                                                for blank in
                                                    &mut expected[row][column..column + cell.width]
                                                {
                                                    blank.text = " ".into();
                                                    blank.width = 1;
                                                    blank.style = style;
                                                    blank.erased = true;
                                                }
                                            }
                                        }
                                    }
                                    if display {
                                        screen.erase_display(mode, style);
                                    } else {
                                        screen.erase_line(mode, style);
                                    }
                                    assert_eq!(
                                        snapshot(screen.grid()).rows,
                                        expected,
                                        "{columns}x{rows}, cursor={cursor:?}, mode={mode:?}, display={display}"
                                    );
                                    assert_eq!(
                                        screen.cursor(),
                                        Cursor {
                                            wrap_pending: false,
                                            ..cursor
                                        }
                                    );
                                    assert_eq!(screen.scroll_region(), region);
                                    for (row, actual) in screen.grid().iter_rows().enumerate() {
                                        assert_eq!(actual.cells().as_ptr(), buffers[row]);
                                        for column in 0..columns {
                                            if expected[row][column].text
                                                == before.rows[row][column].text
                                                && let Some(buffer) = text_buffers[row][column]
                                            {
                                                let Some(CellText::Extended(text)) =
                                                    actual.text(column)
                                                else {
                                                    panic!("Retained text is present");
                                                };
                                                assert_eq!(text.as_ptr(), buffer);
                                            }
                                        }
                                    }
                                    assert_invariants(screen.grid());
                                    if cursor.wrap_pending {
                                        screen.write("Z", CellWidth::Single, StyleId(7)).unwrap();
                                        assert_eq!(screen.cursor().row, cursor.row);
                                        assert_eq!(
                                            screen
                                                .grid()
                                                .row(cursor.row)
                                                .unwrap()
                                                .text(cursor.column),
                                            Some(CellText::Scalar('Z'))
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn cursor_cell_edits_preserve_units_buffers_and_zero_count_state() {
    let edits: [fn(&mut Screen, usize, StyleId); 3] = [
        Screen::insert_cells,
        Screen::delete_cells,
        Screen::erase_cells,
    ];
    for columns in 1..=9 {
        for offset in [0, 2] {
            for phase in 0..2 {
                for row in 0..4 {
                    for column in 0..=columns {
                        for requested in (0..=columns + 1).chain([usize::MAX]) {
                            for (kind, edit) in edits.iter().enumerate() {
                                let mut screen = Screen::new(columns, 4).unwrap();
                                screen.set_cursor(3, 0);
                                for _ in 0..offset {
                                    screen.line_feed(StyleId(0)).unwrap();
                                }
                                for target in 0..4 {
                                    screen.set_cursor(target, 0);
                                    screen
                                        .write_ascii(
                                            &vec![b'A' + target as u8; columns],
                                            StyleId(100 + target as u32),
                                        )
                                        .unwrap();
                                }
                                let mut source = 0;
                                while source < columns {
                                    let width = if (source + phase) % 3 == 1 && source + 1 < columns
                                    {
                                        CellWidth::Double
                                    } else {
                                        CellWidth::Single
                                    };
                                    let mut text = char::from(b'a' + source as u8).to_string();
                                    if source % 2 == 1 {
                                        text.push('\u{301}');
                                    }
                                    screen.set_cursor(row, source);
                                    screen
                                        .write(&text, width, StyleId(source as u32 + 1))
                                        .unwrap();
                                    source += width.columns();
                                }
                                screen.set_scroll_region(1..3).unwrap();
                                screen.set_cursor(row, column.min(columns - 1));
                                if column == columns {
                                    screen.write("!", CellWidth::Single, StyleId(42)).unwrap();
                                }
                                let cursor = screen.cursor();
                                let before = snapshot(screen.grid());
                                let buffers: Vec<_> = screen
                                    .grid()
                                    .iter_rows()
                                    .map(|row| row.cells().as_ptr())
                                    .collect();
                                let text_buffers: Vec<_> = (0..columns)
                                    .map(|column| {
                                        match screen.grid().row(row).unwrap().text(column) {
                                            Some(CellText::Extended(text)) => Some(text.as_ptr()),
                                            _ => None,
                                        }
                                    })
                                    .collect();
                                let style = StyleId(u32::MAX);
                                let count = requested.min(columns - cursor.column);
                                let mut expected = before.rows.clone();
                                for cell in &mut expected[row] {
                                    cell.text = " ".into();
                                    cell.width = 1;
                                    cell.style = style;
                                    cell.erased = true;
                                }
                                let mut retained = Vec::new();
                                for (source, cell) in before.rows[row]
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, cell)| cell.width > 0)
                                {
                                    let end = source + cell.width;
                                    let target = if count == 0 || end <= cursor.column {
                                        Some(source)
                                    } else {
                                        match kind {
                                            0 if source >= cursor.column
                                                && end <= columns - count =>
                                            {
                                                Some(source + count)
                                            }
                                            1 if source >= cursor.column + count => {
                                                Some(source - count)
                                            }
                                            2 if source >= cursor.column + count => Some(source),
                                            _ => None,
                                        }
                                    };
                                    if let Some(target) = target {
                                        expected[row][target..target + cell.width]
                                            .clone_from_slice(&before.rows[row][source..end]);
                                        if let Some(buffer) = text_buffers[source] {
                                            retained.push((target, buffer));
                                        }
                                    }
                                }
                                edit(&mut screen, requested, style);
                                assert_eq!(
                                    snapshot(screen.grid()).rows,
                                    expected,
                                    "columns={columns}, cursor={cursor:?}, count={requested}, kind={kind}"
                                );
                                assert_eq!(
                                    screen.cursor(),
                                    Cursor {
                                        wrap_pending: requested == 0 && cursor.wrap_pending,
                                        ..cursor
                                    }
                                );
                                assert_eq!(screen.scroll_region(), 1..3);
                                for (actual, buffer) in screen.grid().iter_rows().zip(buffers) {
                                    assert_eq!(actual.cells().as_ptr(), buffer);
                                }
                                for (column, buffer) in retained {
                                    let Some(CellText::Extended(text)) =
                                        screen.grid().row(row).unwrap().text(column)
                                    else {
                                        panic!("Retained text is present");
                                    };
                                    assert_eq!(text.as_ptr(), buffer);
                                }
                                assert_invariants(screen.grid());
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn line_edits_and_scrolls_preserve_state_and_recycle_rows_within_their_extent() {
    let operations = [
        (
            Screen::insert_lines as fn(&mut Screen, usize, StyleId),
            true,
            true,
        ),
        (Screen::delete_lines, true, false),
        (Screen::scroll_down, false, true),
        (
            |screen, count, style| screen.scroll_up(count, style).unwrap(),
            false,
            false,
        ),
    ];
    for rows in 1..=6 {
        for offset in 0..rows {
            for region in (0..rows).flat_map(|start| (start + 1..=rows).map(move |end| start..end))
            {
                for cursor_row in 0..rows {
                    for requested in (0..=rows + 1).chain([usize::MAX]) {
                        for (edit, cursor_relative, insert) in operations {
                            let mut screen = Screen::new(4, rows).unwrap();
                            screen.set_cursor(rows - 1, 0);
                            for _ in 0..offset {
                                screen.line_feed(StyleId(0)).unwrap();
                            }
                            for row in 0..rows {
                                screen.set_cursor(row, 0);
                                screen
                                    .write_ascii(&[b'a' + row as u8; 4], StyleId(row as u32))
                                    .unwrap();
                                screen.set_cursor(row, 1);
                                screen
                                    .write("水\u{301}", CellWidth::Double, StyleId(row as u32))
                                    .unwrap();
                            }
                            screen.set_scroll_region(region.clone()).unwrap();
                            screen.set_cursor(cursor_row, 3);
                            screen.write("!", CellWidth::Single, StyleId(8)).unwrap();
                            let cursor = screen.cursor();
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
                            let extent = if cursor_relative {
                                cursor_row..region.end
                            } else {
                                region.clone()
                            };
                            let active = requested != 0
                                && (!cursor_relative || region.contains(&cursor_row));
                            let height = extent.end.saturating_sub(extent.start);
                            let count = if active { requested.min(height) } else { 0 };
                            edit(&mut screen, requested, StyleId(9));
                            assert_eq!(
                                screen.cursor(),
                                Cursor {
                                    wrap_pending: !(active && cursor_relative)
                                        && cursor.wrap_pending,
                                    ..cursor
                                }
                            );
                            assert_eq!(screen.scroll_region(), region);
                            let after = snapshot(screen.grid());
                            for row in 0..rows {
                                let inside = active && extent.contains(&row);
                                let source = if !inside {
                                    Some(row)
                                } else if insert {
                                    row.checked_sub(count)
                                        .filter(|&source| source >= extent.start)
                                } else {
                                    (row + count < extent.end).then_some(row + count)
                                };
                                let buffer = if !inside {
                                    row
                                } else if insert {
                                    extent.start + (row - extent.start + height - count) % height
                                } else {
                                    extent.start + (row - extent.start + count) % height
                                };
                                assert_eq!(
                                    screen.grid().row(row).unwrap().cells().as_ptr(),
                                    buffers[buffer].0
                                );
                                if let Some(source) = source {
                                    assert_eq!(
                                        after.rows[row], before.rows[source],
                                        "rows={rows}, region={region:?}, cursor={cursor_row}, count={requested}, insert={insert}, cursor_relative={cursor_relative}"
                                    );
                                    let Some(CellText::Extended(text)) =
                                        screen.grid().row(row).unwrap().text(1)
                                    else {
                                        panic!("Moved text is present");
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
}

#[test]
fn relative_moves_stop_at_directional_boundaries_and_preserve_owned_contents() {
    let moves = [
        Screen::move_cursor_up,
        Screen::move_cursor_down,
        Screen::move_cursor_left,
        Screen::move_cursor_right,
    ];
    for (rows, region, start, count, pending, destinations) in [
        (7, 2..5, (0, 0), 0, false, [(0, 0); 4]),
        (7, 2..5, (0, 4), 1, true, [(0, 4), (1, 4), (0, 3), (0, 4)]),
        (
            7,
            2..5,
            (1, 2),
            usize::MAX,
            false,
            [(0, 2), (4, 2), (1, 0), (1, 4)],
        ),
        (
            7,
            2..5,
            (2, 2),
            usize::MAX,
            false,
            [(2, 2), (4, 2), (2, 0), (2, 4)],
        ),
        (7, 2..5, (3, 1), 1, false, [(2, 1), (4, 1), (3, 0), (3, 2)]),
        (
            7,
            2..5,
            (4, 2),
            usize::MAX - 1,
            false,
            [(2, 2), (4, 2), (4, 0), (4, 4)],
        ),
        (
            7,
            2..5,
            (5, 2),
            usize::MAX,
            false,
            [(2, 2), (6, 2), (5, 0), (5, 4)],
        ),
        (7, 2..5, (6, 2), 1, false, [(5, 2), (6, 2), (6, 1), (6, 3)]),
        (
            7,
            2..5,
            (6, 2),
            usize::MAX,
            false,
            [(2, 2), (6, 2), (6, 0), (6, 4)],
        ),
        (7, 2..5, (2, 4), 0, true, [(2, 4); 4]),
        (7, 2..5, (4, 4), 1, true, [(3, 4), (4, 4), (4, 3), (4, 4)]),
        (
            7,
            3..4,
            (3, 4),
            usize::MAX,
            true,
            [(3, 4), (3, 4), (3, 0), (3, 4)],
        ),
        (
            1,
            0..1,
            (0, 4),
            usize::MAX,
            true,
            [(0, 4), (0, 4), (0, 0), (0, 4)],
        ),
    ] {
        let mut screen = Screen::new(5, rows).unwrap();
        for row in 0..rows {
            screen.set_cursor(row, 0);
            screen.write_ascii(b"abcde", StyleId(row as u32)).unwrap();
            screen.set_cursor(row, 1);
            screen
                .write("水\u{301}", CellWidth::Double, StyleId(8))
                .unwrap();
        }
        screen.set_scroll_region(region.clone()).unwrap();
        for (move_cursor, (row, column)) in moves.into_iter().zip(destinations) {
            screen.set_cursor(start.0, start.1);
            if pending {
                screen.write("!", CellWidth::Single, StyleId(9)).unwrap();
            }
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
            move_cursor(&mut screen, count);
            assert_eq!(
                screen.cursor(),
                Cursor {
                    row,
                    column,
                    wrap_pending: pending && count == 0
                }
            );
            assert_eq!(screen.scroll_region(), region);
            assert_eq!(snapshot(screen.grid()), before);
            for (row, (cells, text)) in screen.grid().iter_rows().zip(buffers) {
                assert_eq!(row.cells().as_ptr(), cells);
                let Some(CellText::Extended(current)) = row.text(1) else {
                    unreachable!()
                };
                assert_eq!(current.as_ptr(), text);
            }
            assert_invariants(screen.grid());
        }
    }
    let mut single = Screen::new(1, 1).unwrap();
    single
        .write("e\u{301}", CellWidth::Single, StyleId(1))
        .unwrap();
    let before = snapshot(single.grid());
    for move_cursor in moves {
        move_cursor(&mut single, 0);
        assert!(single.cursor().wrap_pending);
    }
    for move_cursor in moves {
        single
            .write("e\u{301}", CellWidth::Single, StyleId(1))
            .unwrap();
        move_cursor(&mut single, usize::MAX);
        assert_eq!(single.cursor(), Cursor::default());
        assert_eq!(snapshot(single.grid()), before);
    }
}

#[test]
fn tabs_cross_word_boundaries_preserve_contents_and_follow_width_changes() {
    let mut screen = Screen::new(130, 2).unwrap();
    screen.set_scroll_region(1..2).unwrap();
    screen.set_cursor(0, 64);
    screen
        .write("水\u{301}", CellWidth::Double, StyleId(1))
        .unwrap();
    screen.set_cursor(0, 129);
    screen.write("!", CellWidth::Single, StyleId(2)).unwrap();
    let pending = screen.cursor();
    let contents = snapshot(screen.grid());
    for column in 0_usize..130 {
        assert_eq!(screen.is_tab_stop(column), Some(column.is_multiple_of(8)));
    }
    assert_eq!(screen.is_tab_stop(130), None);
    assert_eq!(screen.is_tab_stop(usize::MAX), None);
    screen.set_tab_stop();
    screen.clear_tab_stop();
    screen.clear_tab_stops();
    assert_eq!(screen.cursor(), pending);
    screen.tab_forward(0);
    screen.tab_backward(0);
    assert_eq!(screen.cursor(), pending);
    screen.tab_forward(1);
    assert_eq!(screen.cursor().column, 129);
    assert!(!screen.cursor().wrap_pending);
    for column in [0, 1, 63, 64, 65, 127, 129] {
        screen.set_cursor(0, column);
        screen.set_tab_stop();
    }
    for (backward, start, count, end) in [
        (false, 62, 1, 63),
        (false, 63, 1, 64),
        (false, 64, 1, 65),
        (false, 65, 1, 127),
        (false, 63, 2, 65),
        (false, 65, 2, 129),
        (false, 128, 1, 129),
        (false, 64, usize::MAX, 129),
        (true, 66, 1, 65),
        (true, 65, 1, 64),
        (true, 64, 1, 63),
        (true, 127, 2, 64),
        (true, 129, 1, 127),
        (true, 128, 3, 64),
        (true, 129, usize::MAX, 0),
        (true, 0, 1, 0),
    ] {
        screen.set_cursor(0, start);
        if backward {
            screen.tab_backward(count);
        } else {
            screen.tab_forward(count);
        }
        assert_eq!(
            screen.cursor(),
            Cursor {
                row: 0,
                column: end,
                wrap_pending: false
            }
        );
        assert_eq!(screen.scroll_region(), 1..2);
        assert_eq!(snapshot(screen.grid()), contents);
    }
    screen.set_cursor(0, 129);
    screen.write("!", CellWidth::Single, StyleId(2)).unwrap();
    screen.tab_backward(1);
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: 0,
            column: 127,
            wrap_pending: false
        }
    );
    screen.clear_tab_stops();
    for column in [3, 63, 64, 65, 127, 129] {
        screen.set_cursor(0, column);
        screen.set_tab_stop();
    }
    screen.resize(65, 2, StyleId(3)).unwrap();
    screen.resize(130, 2, StyleId(3)).unwrap();
    for column in 0_usize..130 {
        let expected = [3, 63, 64].contains(&column) || (column >= 65 && column.is_multiple_of(8));
        assert_eq!(screen.is_tab_stop(column), Some(expected));
    }
    screen.set_cursor(0, 64);
    screen.clear_tab_stop();
    screen.resize(130, 3, StyleId(3)).unwrap();
    assert_eq!(screen.is_tab_stop(64), Some(false));
    screen.resize(63, 3, StyleId(3)).unwrap();
    screen.resize(64, 3, StyleId(3)).unwrap();
    assert_eq!(screen.is_tab_stop(63), Some(false));
    screen.resize(65, 3, StyleId(3)).unwrap();
    assert_eq!(screen.is_tab_stop(64), Some(true));
    assert_eq!(screen.is_tab_stop(3), Some(true));
    assert_invariants(screen.grid());

    let mut wide = Screen::new(crate::MAX_DIMENSION, 1).unwrap();
    wide.tab_forward(8191);
    assert_eq!(wide.cursor().column, 65528);
    wide.tab_forward(1);
    assert_eq!(wide.cursor().column, crate::MAX_DIMENSION - 1);
    wide.tab_backward(8191);
    assert_eq!(wide.cursor().column, 8);
    wide.clear_tab_stops();
    wide.set_cursor(0, crate::MAX_DIMENSION - 2);
    wide.set_tab_stop();
    wide.set_cursor(0, 0);
    wide.tab_forward(1);
    assert_eq!(wide.cursor().column, crate::MAX_DIMENSION - 2);
}

#[test]
fn automatic_wrap_modes_preserve_wide_units_and_reject_invalid_ignored_writes() {
    let too_long = "x".repeat(crate::MAX_CELL_TEXT_BYTES + 1);
    for columns in 1..=4 {
        for rows in [1, 3] {
            for row in 0..rows {
                let mut screen = Screen::new(columns, rows).unwrap();
                screen.set_scroll_region(0..1).unwrap();
                screen.set_cursor(row, columns.saturating_sub(2));
                if columns > 1 {
                    screen
                        .write("水\u{301}", CellWidth::Double, StyleId(1))
                        .unwrap();
                } else {
                    screen.write("x", CellWidth::Single, StyleId(1)).unwrap();
                }
                let pending = screen.cursor();
                screen.set_auto_wrap(true);
                assert_eq!(screen.cursor(), pending);
                screen.set_auto_wrap(false);
                let cursor = screen.cursor();
                let before = snapshot(screen.grid());
                assert!(!cursor.wrap_pending);
                if columns > 1 {
                    screen
                        .write("界\u{301}", CellWidth::Double, StyleId(9))
                        .unwrap();
                    assert!(matches!(
                        screen.write("", CellWidth::Double, StyleId(9)),
                        Err(StorageError::EmptyText)
                    ));
                    assert!(matches!(
                        screen.write(&too_long, CellWidth::Double, StyleId(9)),
                        Err(StorageError::TextTooLong)
                    ));
                } else {
                    assert!(matches!(
                        screen.write("界", CellWidth::Double, StyleId(9)),
                        Err(StorageError::OutOfBounds)
                    ));
                }
                screen.write_ascii(b"", StyleId(9)).unwrap();
                assert!(matches!(
                    screen.write_ascii(b"ab\nc", StyleId(9)),
                    Err(StorageError::InvalidAscii)
                ));
                assert_eq!(screen.cursor(), cursor);
                assert_eq!(snapshot(screen.grid()), before);
                assert!(!screen.auto_wrap());
                assert_eq!(screen.scroll_region(), 0..1);
                screen.write("A", CellWidth::Single, StyleId(2)).unwrap();
                screen.write("B", CellWidth::Single, StyleId(3)).unwrap();
                assert_eq!(screen.cursor(), cursor);
                assert_eq!(
                    screen.grid().row(row).unwrap().text(columns - 1),
                    Some(CellText::Scalar('B'))
                );
                if columns > 1 {
                    assert_eq!(
                        screen.grid().row(row).unwrap().text(columns - 2),
                        Some(CellText::Scalar(' '))
                    );
                }
                assert_invariants(screen.grid());
                screen.set_auto_wrap(true);
                screen.write("C", CellWidth::Single, StyleId(4)).unwrap();
                assert_eq!(
                    screen.cursor(),
                    Cursor {
                        wrap_pending: true,
                        ..cursor
                    }
                );
                screen.set_auto_wrap(true);
                assert!(screen.cursor().wrap_pending);
                screen.set_auto_wrap(false);
                screen.set_auto_wrap(true);
                screen.write("D", CellWidth::Single, StyleId(4)).unwrap();
                assert_eq!(
                    screen.cursor(),
                    Cursor {
                        wrap_pending: true,
                        ..cursor
                    }
                );
                screen.set_auto_wrap(false);
                screen.resize(columns + 1, rows + 1, StyleId(4)).unwrap();
                assert!(!screen.auto_wrap());
                screen.write_ascii(b"abcdefghijklmnop", StyleId(5)).unwrap();
                assert_eq!(
                    screen.cursor(),
                    Cursor {
                        row,
                        column: columns,
                        wrap_pending: false
                    }
                );
                assert_invariants(screen.grid());
            }
        }
    }
}
