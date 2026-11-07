#![no_main]

use libfuzzer_sys::fuzz_target;
use lune_terminal::{
    Cell, CellPosition, CellText, CellWidth, EraseMode, HistoryLimits, MAX_HISTORY_BYTES,
    MAX_HISTORY_ROWS, Row, Screen, ScreenBuffer, StorageError, StyleId,
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
            assert!(matches!(view.text(range), Err(StorageError::OutOfBounds)));
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
        assert_eq!(view.text(range).unwrap().to_string(), expected);
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
    for bytes in data.as_chunks::<4>().0.iter().take(512) {
        let style = StyleId(u32::from_le_bytes(*bytes));
        let operation = bytes[0] % 20;
        for (index, screen) in [&mut bulk, &mut fragmented, &mut ordinary]
            .into_iter()
            .enumerate()
        {
            match operation {
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
                        for &byte in run {
                            screen.write_ascii(&[byte], style).unwrap();
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
                _ => {}
            }
        }
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
