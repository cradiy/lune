#![no_main]

use std::ops::Range;

use libfuzzer_sys::fuzz_target;
use lune_terminal::{
    Cell, CellPosition, CellText, CellWidth, EraseMode, HistoryLimits, LogicalLine,
    MAX_HISTORY_BYTES, MAX_HISTORY_ROWS, Row, Screen, ScreenBuffer, StorageError, StyleId,
    TextRange,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct SavedRow {
    cells: Vec<Cell>,
    text: Vec<String>,
    wrap: Option<usize>,
}

fn row(value: &Row) -> SavedRow {
    SavedRow {
        cells: value.cells().to_vec(),
        text: (0..value.cells().len())
            .map(|column| match value.text(column).unwrap() {
                CellText::Scalar(value) => value.to_string(),
                CellText::Extended(value) => value.to_owned(),
                CellText::Continuation => String::new(),
            })
            .collect(),
        wrap: value.wrap_end(),
    }
}

fn check(left: &Screen, right: &Screen) {
    assert_eq!(left.cursor(), right.cursor());
    assert_eq!(left.scroll_region(), right.scroll_region());
    assert_eq!(left.origin_mode(), right.origin_mode());
    assert_eq!(left.auto_wrap(), right.auto_wrap());
    assert_eq!(left.insert_mode(), right.insert_mode());
    assert_eq!(left.active_buffer(), right.active_buffer());
    for buffer in [ScreenBuffer::Primary, ScreenBuffer::Alternate] {
        assert_eq!(
            left.buffer(buffer).iter_rows().map(row).collect::<Vec<_>>(),
            right
                .buffer(buffer)
                .iter_rows()
                .map(row)
                .collect::<Vec<_>>()
        );
    }
}

fn check_text(screen: &Screen, bytes: [u8; 4]) {
    let rows = if screen.active_buffer() == ScreenBuffer::Primary {
        screen
            .history()
            .iter_rows()
            .chain(screen.grid().iter_rows())
            .map(row)
            .collect::<Vec<_>>()
    } else {
        screen.grid().iter_rows().map(row).collect::<Vec<_>>()
    };
    let view = screen.view();
    assert_eq!(view.rows(), rows.len());
    assert_eq!(view.iter_rows().map(row).collect::<Vec<_>>(), rows);
    let mut expected_lines = Vec::new();
    let mut start = 0;
    for (index, row) in rows.iter().enumerate() {
        if row.wrap.is_none() || index + 1 == rows.len() {
            expected_lines.push(start..index + 1);
            start = index + 1;
        }
    }
    assert_eq!(
        view.logical_lines()
            .map(|line| line.range())
            .collect::<Vec<_>>(),
        expected_lines
    );
    assert_eq!(
        view.logical_lines()
            .rev()
            .map(|line| line.range())
            .collect::<Vec<_>>(),
        expected_lines.iter().rev().cloned().collect::<Vec<_>>()
    );
    let index = usize::from(bytes[0]) % rows.len();
    let line = view.logical_line(index).unwrap();
    let range = expected_lines
        .iter()
        .find(|range| range.contains(&index))
        .unwrap();
    assert_eq!(line.range(), *range);
    let mut expected_text = String::new();
    for row in &rows[range.clone()] {
        for (column, cell) in row.cells.iter().enumerate() {
            if cell.width() != 0 && column < row.wrap.unwrap_or(row.cells.len()) {
                expected_text.push_str(&row.text[column]);
            }
        }
    }
    assert_eq!(line.text().to_string(), expected_text);
    check_reflow(line, &rows, usize::from(bytes[1] % 17) + 1);
    check_ranges(&rows, |range| view.text(range), bytes);
    let viewport = screen.viewport();
    let selected = &rows[viewport.range()];
    assert_eq!(viewport.iter_rows().map(row).collect::<Vec<_>>(), selected);
    check_ranges(selected, |range| viewport.text(range), bytes);
}

fn check_reflow(line: LogicalLine<'_>, rows: &[SavedRow], columns: usize) {
    let mut units = Vec::new();
    for index in line.range() {
        let row = &rows[index];
        for (column, cell) in row.cells.iter().enumerate() {
            if column < row.wrap.unwrap_or(row.cells.len()) && cell.width() != 0 {
                units.push((
                    CellPosition { row: index, column },
                    cell.width(),
                    row.text[column].as_str(),
                ));
            }
        }
    }
    if columns == 1 && units.iter().any(|unit| unit.1 == 2) {
        assert!(matches!(
            line.reflow(columns),
            Err(StorageError::OutOfBounds)
        ));
        return;
    }
    let mut groups = Vec::new();
    let mut first = 0;
    let mut used = 0;
    for (index, unit) in units.iter().enumerate() {
        if used + unit.1 > columns {
            groups.push((first, index, used));
            first = index;
            used = 0;
        }
        used += unit.1;
    }
    groups.push((first, units.len(), used));
    let last = line.range().end - 1;
    let end = CellPosition {
        row: last,
        column: rows[last].cells.len(),
    };
    let actual = line.reflow(columns).unwrap().collect::<Vec<_>>();
    assert_eq!(actual.len(), groups.len());
    for (index, (row, &(start, finish, used))) in actual.iter().zip(&groups).enumerate() {
        assert_eq!(
            row.source_range(),
            units[start].0..units.get(finish).map_or(end, |unit| unit.0)
        );
        assert_eq!(row.used_columns(), used);
        assert_eq!(row.wrap_end(), (index + 1 < groups.len()).then_some(used));
        let expected: String = units[start..finish].iter().map(|unit| unit.2).collect();
        assert_eq!(row.text().to_string(), expected);
    }
}

fn check_ranges<'a>(
    rows: &[SavedRow],
    mut text: impl FnMut(Range<CellPosition>) -> Result<TextRange<'a>, StorageError>,
    bytes: [u8; 4],
) {
    for range in [
        CellPosition { row: 0, column: 0 }..CellPosition {
            row: rows.len() - 1,
            column: rows.last().unwrap().cells.len(),
        },
        CellPosition {
            row: usize::from(bytes[1]) % (rows.len() + 1),
            column: usize::from(bytes[2] % 14),
        }..CellPosition {
            row: usize::from(bytes[3]) % (rows.len() + 1),
            column: usize::from(bytes[0] % 14),
        },
    ] {
        let valid = range.start <= range.end
            && [&range.start, &range.end].into_iter().all(|p| {
                rows.get(p.row)
                    .is_some_and(|row| p.column <= row.cells.len())
            });
        if !valid {
            assert!(matches!(text(range), Err(StorageError::OutOfBounds)));
            continue;
        }
        let mut expected = String::new();
        for (index, row) in rows
            .iter()
            .enumerate()
            .take(range.end.row + 1)
            .skip(range.start.row)
        {
            let start = if index == range.start.row {
                range.start.column
            } else {
                0
            };
            let end = if index == range.end.row {
                range.end.column
            } else {
                row.cells.len()
            }
            .min(row.wrap.unwrap_or(row.cells.len()));
            if start < end {
                for (column, cell) in row.cells.iter().enumerate() {
                    if cell.width() != 0 && column < end && column + cell.width() > start {
                        expected.push_str(&row.text[column]);
                    }
                }
            }
            if index < range.end.row && row.wrap.is_none() {
                expected.push('\n');
            }
        }
        assert_eq!(text(range).unwrap().to_string(), expected);
    }
}

fn anchor(screen: &Screen, offset: usize) -> Option<*const Cell> {
    (offset != 0).then(|| {
        screen
            .history()
            .row(screen.history().len() - offset)
            .unwrap()
            .cells()
            .as_ptr()
    })
}

fn reconcile(screen: &Screen, offset: &mut usize, anchor: Option<*const Cell>) {
    // Each tracked step captures at most one row. An evicted cell buffer can
    // become visible, but cannot reenter history before this identity check.
    *offset = anchor.map_or(0, |pointer| {
        let index = screen
            .history()
            .iter_rows()
            .position(|row| row.cells().as_ptr() == pointer)
            .unwrap_or(0);
        screen.history().len() - index
    });
}

fn tracked(screen: &mut Screen, offset: &mut usize, action: impl FnOnce(&mut Screen)) {
    let before = anchor(screen, *offset);
    action(screen);
    reconcile(screen, offset, before);
}

fn check_viewport(screen: &Screen, primary_offset: usize) {
    let offset = if screen.active_buffer() == ScreenBuffer::Primary {
        primary_offset
    } else {
        0
    };
    let viewport = screen.viewport();
    assert_eq!(screen.viewport_offset(), offset);
    assert_eq!(viewport.offset(), offset);
    let top = screen.view().history_rows() - offset;
    assert_eq!(viewport.range(), top..top + screen.grid().rows());
    assert_eq!(viewport.rows(), screen.grid().rows());
    let cursor = screen.cursor();
    let actual = viewport.cursor();
    if offset + cursor.row < viewport.rows() {
        let mut expected = cursor;
        expected.row += offset;
        assert_eq!(actual, Some(expected));
    } else {
        assert!(actual.is_none());
    }
}

fuzz_target!(|data: &[u8]| {
    let mut bulk = Screen::new(4, 3).unwrap();
    let mut fragmented = Screen::new(4, 3).unwrap();
    let mut ordinary = Screen::new(4, 3).unwrap();
    for screen in [&mut bulk, &mut fragmented] {
        screen
            .set_history_limits(HistoryLimits {
                max_rows: 4,
                max_bytes: MAX_HISTORY_BYTES,
            })
            .unwrap();
    }
    let mut primary_offset = 0;
    for bytes in data.as_chunks::<4>().0.iter().take(512) {
        let style = StyleId(u32::from_le_bytes(*bytes));
        let operation = bytes[0] % 20;
        for (index, screen) in [&mut bulk, &mut fragmented, &mut ordinary]
            .into_iter()
            .enumerate()
        {
            let old_anchor = if index == 1 {
                anchor(screen, primary_offset)
            } else {
                None
            };
            let mut tracked_steps = false;
            match operation {
                0 if index == 1 => {
                    tracked_steps = true;
                    for _ in 0..usize::from(bytes[1]).min(screen.scroll_region().len()) {
                        tracked(screen, &mut primary_offset, |screen| {
                            screen.scroll_up(1, style).unwrap()
                        });
                    }
                }
                0 => screen.scroll_up(usize::from(bytes[1]), style).unwrap(),
                1 => screen.line_feed(style).unwrap(),
                2 => screen.scroll_down(usize::from(bytes[1]), style),
                3 if index != 2 => {
                    let limits = HistoryLimits {
                        max_rows: if bytes[3] == 255 {
                            MAX_HISTORY_ROWS + 1
                        } else {
                            usize::from(bytes[1] % 8)
                        },
                        max_bytes: if bytes[2] & 1 == 0 {
                            MAX_HISTORY_BYTES
                        } else {
                            0
                        },
                    };
                    let before = screen.history().limits();
                    if bytes[3] == 255 {
                        assert!(screen.set_history_limits(limits).is_err());
                        assert_eq!(screen.history().limits(), before);
                    } else {
                        screen.set_history_limits(limits).unwrap();
                    }
                }
                4 => screen.clear_history(),
                5 => screen.reverse_index(style),
                6 => {
                    let text = ["x", "水", "e\u{301}", "👩\u{200d}💻"][bytes[1] as usize % 4];
                    let width = if bytes[2] & 1 == 0 {
                        CellWidth::Single
                    } else {
                        CellWidth::Double
                    };
                    let _ = screen.write(text, width, style);
                }
                7 => {
                    let run =
                        &b"abcdefghijklmnopqrstuvwxyz0123456789"[..usize::from(bytes[1] % 36) + 1];
                    if index == 1 {
                        tracked_steps = true;
                        for &byte in run {
                            tracked(screen, &mut primary_offset, |screen| {
                                screen.write_ascii(&[byte], style).unwrap()
                            });
                        }
                    } else {
                        screen.write_ascii(run, style).unwrap();
                    }
                }
                8 => screen.set_cursor(bytes[1] as usize, bytes[2] as usize),
                9 => screen
                    .resize(
                        usize::from(bytes[1] % 12) + 1,
                        usize::from(bytes[2] % 5) + 1,
                        style,
                    )
                    .unwrap(),
                10 => {
                    let height = screen.grid().rows();
                    let start = bytes[1] as usize % height;
                    let end = start + 1 + bytes[2] as usize % (height - start);
                    screen.set_scroll_region(start..end).unwrap();
                    screen.set_origin_mode(bytes[3] & 1 != 0);
                }
                11 => screen.select_buffer(if bytes[1] & 1 == 0 {
                    ScreenBuffer::Primary
                } else {
                    ScreenBuffer::Alternate
                }),
                12 => screen.erase_display(EraseMode::All, style),
                13 => {
                    screen.set_auto_wrap(bytes[1] & 1 != 0);
                    screen.set_insert_mode(bytes[2] & 1 != 0);
                }
                14 => screen.insert_lines(usize::from(bytes[1]), style),
                15 => screen.delete_lines(usize::from(bytes[1]), style),
                16 => screen.erase_line(EraseMode::All, style),
                17 => screen.reset(style),
                18 => screen.carriage_return(),
                19 => {
                    tracked_steps = true;
                    let count = if bytes[1] == 255 {
                        usize::MAX
                    } else {
                        usize::from(bytes[1])
                    };
                    match bytes[2] % 3 {
                        0 => {
                            screen.scroll_viewport_up(count);
                            if index == 1 && screen.active_buffer() == ScreenBuffer::Primary {
                                for _ in 0..count.min(screen.history().len()) {
                                    if primary_offset < screen.history().len() {
                                        primary_offset += 1;
                                    }
                                }
                            }
                        }
                        1 => {
                            screen.scroll_viewport_down(count);
                            if index == 1 && screen.active_buffer() == ScreenBuffer::Primary {
                                for _ in 0..count.min(primary_offset) {
                                    primary_offset -= 1;
                                }
                            }
                        }
                        _ => {
                            screen.scroll_viewport_to_bottom();
                            if index == 1 && screen.active_buffer() == ScreenBuffer::Primary {
                                primary_offset = 0;
                            }
                        }
                    }
                }
                _ => {}
            }
            if index == 1 && !tracked_steps {
                reconcile(screen, &mut primary_offset, old_anchor);
            }
        }
        check_viewport(&bulk, primary_offset);
        check_viewport(&fragmented, primary_offset);
        if bytes[0] & 3 == 0 {
            check_text(&bulk, *bytes);
        }
        check(&bulk, &ordinary);
        check(&bulk, &fragmented);
        assert_eq!(
            bulk.history().iter_rows().map(row).collect::<Vec<_>>(),
            fragmented
                .history()
                .iter_rows()
                .map(row)
                .collect::<Vec<_>>()
        );
        for screen in [&bulk, &fragmented] {
            assert!(screen.history().len() <= screen.history().limits().max_rows);
            assert!(screen.history().retained_bytes() <= screen.history().limits().max_bytes);
            let history = screen.history();
            assert_eq!(
                history.style_ids().collect::<Vec<_>>(),
                history
                    .iter_rows()
                    .flat_map(|row| row.cells().iter().map(|cell| cell.style()))
                    .collect::<Vec<_>>()
            );
        }
    }
});
