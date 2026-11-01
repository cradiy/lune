use super::{assert_invariants, snapshot};
use crate::{CellText, CellWidth, Cursor, EraseMode, Screen, StorageError, StyleId};

#[test]
fn ascii_batches_match_unit_writes_across_wraps_and_scrolls() {
    let input = b"abcdefghijklmnopqrstuvwxyz0123456789";
    for columns in 1..=12 {
        for rows in 1..=3 {
            for region in (0..rows).flat_map(|start| (start + 1..=rows).map(move |end| start..end))
            {
                for row in 0..rows {
                    for (column, auto_wrap) in (0..=columns)
                        .flat_map(|column| [false, true].map(move |mode| (column, mode)))
                    {
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
                            screen.set_auto_wrap(auto_wrap);
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
                                    screen.set_cursor(rows - 1, 0).unwrap();
                                    for _ in 0..offset {
                                        screen.line_feed(StyleId(0));
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
                                            screen.set_cursor(row, column).unwrap();
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
                                    screen
                                        .set_cursor(cursor_row, cursor_column.min(columns - 1))
                                        .unwrap();
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
                                screen.set_cursor(3, 0).unwrap();
                                for _ in 0..offset {
                                    screen.line_feed(StyleId(0));
                                }
                                for target in 0..4 {
                                    screen.set_cursor(target, 0).unwrap();
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
                                    screen.set_cursor(row, source).unwrap();
                                    screen
                                        .write(&text, width, StyleId(source as u32 + 1))
                                        .unwrap();
                                    source += width.columns();
                                }
                                screen.set_scroll_region(1..3).unwrap();
                                screen.set_cursor(row, column.min(columns - 1)).unwrap();
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
        (Screen::scroll_up, false, false),
    ];
    for rows in 1..=6 {
        for offset in 0..rows {
            for region in (0..rows).flat_map(|start| (start + 1..=rows).map(move |end| start..end))
            {
                for cursor_row in 0..rows {
                    for requested in (0..=rows + 1).chain([usize::MAX]) {
                        for (edit, cursor_relative, insert) in operations {
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
            screen.set_cursor(row, 0).unwrap();
            screen.write_ascii(b"abcde", StyleId(row as u32)).unwrap();
            screen.set_cursor(row, 1).unwrap();
            screen
                .write("水\u{301}", CellWidth::Double, StyleId(8))
                .unwrap();
        }
        screen.set_scroll_region(region.clone()).unwrap();
        for (move_cursor, (row, column)) in moves.into_iter().zip(destinations) {
            screen.set_cursor(start.0, start.1).unwrap();
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
    screen.set_cursor(0, 64).unwrap();
    screen
        .write("水\u{301}", CellWidth::Double, StyleId(1))
        .unwrap();
    screen.set_cursor(0, 129).unwrap();
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
        screen.set_cursor(0, column).unwrap();
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
        screen.set_cursor(0, start).unwrap();
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
    screen.set_cursor(0, 129).unwrap();
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
        screen.set_cursor(0, column).unwrap();
        screen.set_tab_stop();
    }
    screen.resize(65, 2, StyleId(3)).unwrap();
    screen.resize(130, 2, StyleId(3)).unwrap();
    for column in 0_usize..130 {
        let expected = [3, 63, 64].contains(&column) || (column >= 65 && column.is_multiple_of(8));
        assert_eq!(screen.is_tab_stop(column), Some(expected));
    }
    screen.set_cursor(0, 64).unwrap();
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
    wide.set_cursor(0, crate::MAX_DIMENSION - 2).unwrap();
    wide.set_tab_stop();
    wide.set_cursor(0, 0).unwrap();
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
                screen.set_cursor(row, columns.saturating_sub(2)).unwrap();
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
