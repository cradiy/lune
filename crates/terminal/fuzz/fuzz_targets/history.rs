#![no_main]

use std::collections::VecDeque;

use libfuzzer_sys::fuzz_target;
use lune_terminal::{
    Cell, CellText, CellWidth, EraseMode, HistoryLimits, MAX_HISTORY_BYTES, MAX_HISTORY_ROWS, Row,
    RowHistory, Screen, ScreenBuffer, StyleId,
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

fuzz_target!(|data: &[u8]| {
    let mut screen = Screen::new(4, 3).unwrap();
    let mut ordinary = Screen::new(4, 3).unwrap();
    let mut history = RowHistory::new(HistoryLimits {
        max_rows: 4,
        max_bytes: MAX_HISTORY_BYTES,
    })
    .unwrap();
    let mut saved = VecDeque::<SavedRow>::new();
    for bytes in data.as_chunks::<4>().0.iter().take(512) {
        let style = StyleId(u32::from_le_bytes(*bytes));
        let operation = bytes[0] % 14;
        match operation {
            0..=2 => {
                let enabled = history.limits().max_rows != 0 && history.limits().max_bytes != 0;
                let primary = screen.active_buffer() == ScreenBuffer::Primary;
                let full = screen.scroll_region() == (0..screen.grid().rows());
                if primary && full && enabled {
                    let incoming = row(ordinary.grid().row(0).unwrap());
                    if saved
                        .back()
                        .is_some_and(|last| last.cells.len() != incoming.cells.len())
                    {
                        saved.back_mut().unwrap().wrap = None;
                    }
                    if saved.len() == history.limits().max_rows {
                        saved.pop_front();
                    }
                    saved.push_back(incoming);
                } else if primary
                    && screen.scroll_region().start == 0
                    && let Some(last) = saved.back_mut()
                {
                    last.wrap = None;
                }
                assert_eq!(
                    history.scroll_up(&mut screen, style).unwrap(),
                    primary && full && enabled
                );
                ordinary.scroll_up(1, style);
            }
            3 => {
                let invalid = bytes[3] == 255;
                let limits = HistoryLimits {
                    max_rows: if invalid {
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
                let before = history.limits();
                if invalid {
                    assert!(history.set_limits(limits).is_err());
                    assert_eq!(history.limits(), before);
                } else {
                    history.set_limits(limits).unwrap();
                    if limits.max_bytes == 0 {
                        saved.clear();
                    }
                    while saved.len() > limits.max_rows {
                        saved.pop_front();
                    }
                }
            }
            4 => {
                history.clear();
                saved.clear();
            }
            5 => {
                history.disconnect();
                if let Some(last) = saved.back_mut() {
                    last.wrap = None;
                }
            }
            _ => {
                // External mutations explicitly end the history-to-screen join.
                history.disconnect();
                if let Some(last) = saved.back_mut() {
                    last.wrap = None;
                }
                for screen in [&mut screen, &mut ordinary] {
                    match operation {
                        6 => {
                            let text =
                                ["x", "水", "e\u{301}", "👩\u{200d}💻"][bytes[1] as usize % 4];
                            let width = if bytes[2] & 1 == 0 {
                                CellWidth::Single
                            } else {
                                CellWidth::Double
                            };
                            let _ = screen.write(text, width, style);
                        }
                        7 => {
                            screen.write_ascii(b"abcdefghijk", style).unwrap();
                        }
                        8 => {
                            screen.set_cursor(bytes[1] as usize, bytes[2] as usize);
                        }
                        9 => {
                            screen
                                .resize(
                                    usize::from(bytes[1] % 12) + 1,
                                    usize::from(bytes[2] % 5) + 1,
                                    style,
                                )
                                .unwrap();
                        }
                        10 => {
                            let height = screen.grid().rows();
                            let start = bytes[1] as usize % height;
                            let end = start + 1 + bytes[2] as usize % (height - start);
                            screen.set_scroll_region(start..end).unwrap();
                            screen.set_origin_mode(bytes[3] & 1 != 0);
                        }
                        11 => {
                            screen.select_buffer(if bytes[1] & 1 == 0 {
                                ScreenBuffer::Primary
                            } else {
                                ScreenBuffer::Alternate
                            });
                        }
                        12 => {
                            screen.erase_display(EraseMode::All, style);
                        }
                        _ => {
                            screen.set_auto_wrap(bytes[1] & 1 != 0);
                            screen.set_insert_mode(bytes[2] & 1 != 0);
                        }
                    }
                }
            }
        }
        check(&screen, &ordinary);
        assert_eq!(
            history.iter_rows().map(row).collect::<Vec<_>>(),
            saved.iter().cloned().collect::<Vec<_>>()
        );
        assert_eq!(
            history.style_ids().collect::<Vec<_>>(),
            saved
                .iter()
                .flat_map(|row| row.cells.iter().map(|cell| cell.style()))
                .collect::<Vec<_>>()
        );
        assert!(history.len() <= history.limits().max_rows);
        assert!(history.retained_bytes() <= history.limits().max_bytes);
    }
});
