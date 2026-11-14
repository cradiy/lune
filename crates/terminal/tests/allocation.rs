#[path = "support/allocator.rs"]
mod allocator;

use std::{
    fmt::{self, Write},
    hint::black_box,
};

use lune_terminal::{
    CellPosition, CellText, CellWidth, Cursor, EraseMode, Grid, HistoryLimits, Modifiers,
    ProcessFailure, Processor, ReflowAffinity, Row, RowHistory, Screen, ScreenBuffer, StorageError,
    Style, StyleError, StyleId, StyleTable, TextSink, UnicodeWriter,
};

#[global_allocator]
static ALLOCATOR: allocator::TestAllocator = allocator::TestAllocator;

#[derive(Debug, PartialEq, Eq)]
struct HistorySnapshot {
    rows: Vec<Vec<SnapshotCell>>,
    wraps: Vec<Option<usize>>,
    limits: HistoryLimits,
    bytes: usize,
}
fn retained(history: &RowHistory) -> HistorySnapshot {
    HistorySnapshot {
        rows: history
            .iter_rows()
            .map(|row| {
                row.cells()
                    .iter()
                    .enumerate()
                    .map(|(column, cell)| SnapshotCell {
                        text: match row.text(column).unwrap() {
                            CellText::Scalar(value) => value.to_string(),
                            CellText::Extended(value) => value.to_owned(),
                            CellText::Continuation => String::new(),
                        },
                        width: cell.width(),
                        style: cell.style(),
                        erased: cell.is_erased(),
                    })
                    .collect()
            })
            .collect(),
        wraps: history.iter_rows().map(|row| row.wrap_end()).collect(),
        limits: history.limits(),
        bytes: history.retained_bytes(),
    }
}

#[test]
fn history_allocation_failures_preserve_owners_and_saturated_capture_reuses_rows() {
    fn prepared(case: usize) -> Screen {
        let mut screen = Screen::new(4, 3).unwrap();
        screen
            .set_history_limits(HistoryLimits {
                max_rows: if case == 1 { 1 } else { 32 },
                max_bytes: 4096,
            })
            .unwrap();
        if case != 0 {
            for _ in 0..if case == 1 { 1 } else { 8 } {
                screen.set_cursor(0, 0);
                screen
                    .write("水\u{301}", CellWidth::Double, StyleId(1))
                    .unwrap();
                screen.scroll_up(1, StyleId(0)).unwrap();
            }
        }
        if case == 1 {
            screen.resize(9, 4, StyleId(2)).unwrap();
        }
        screen.set_cursor(0, 0);
        screen
            .write("e\u{301}", CellWidth::Single, StyleId(3))
            .unwrap();
        screen.set_cursor(1, 2);
        screen.save_cursor();
        screen.set_insert_mode(true);
        screen.clear_tab_stops();
        screen.set_tab_stop();
        screen.scroll_viewport_up(1);
        screen
    }
    for case in 0..3 {
        let mut expected_screen = prepared(case);
        let (result, stats) = allocator::measure(None, || expected_screen.scroll_up(1, StyleId(7)));
        result.unwrap();
        assert!(stats.attempts > 0);
        let expected = retained(expected_screen.history());
        let expected_grid = snapshot(expected_screen.grid());
        for point in 0..stats.attempts {
            let mut screen = prepared(case);
            let before_history = retained(screen.history());
            let before_grid = snapshot(screen.grid());
            let alternate = snapshot(screen.buffer(ScreenBuffer::Alternate));
            let cursor = screen.cursor();
            let offset = screen.viewport_offset();
            let cells = screen.grid().row(0).unwrap().cells().as_ptr();
            let (result, failed) =
                allocator::measure(Some(point), || screen.scroll_up(1, StyleId(7)));
            assert!(matches!(result, Err(StorageError::Allocation(_))));
            assert_eq!(failed.failures, 1);
            assert_eq!(retained(screen.history()), before_history);
            assert_eq!(snapshot(screen.grid()), before_grid);
            assert_eq!(screen.grid().row(0).unwrap().cells().as_ptr(), cells);
            assert_eq!(snapshot(screen.buffer(ScreenBuffer::Alternate)), alternate);
            assert_eq!(screen.cursor(), cursor);
            assert_eq!(screen.viewport_offset(), offset);
            assert!(screen.insert_mode());
            assert_eq!(screen.is_tab_stop(2), Some(true));
            assert_eq!(screen.is_tab_stop(0), Some(false));
            screen.scroll_up(1, StyleId(7)).unwrap();
            assert_eq!(retained(screen.history()), expected);
            assert_eq!(snapshot(screen.grid()), expected_grid);
            assert_eq!(screen.viewport_offset(), expected_screen.viewport_offset());
            screen.carriage_return();
            screen.restore_cursor();
            assert_eq!(screen.cursor(), cursor);
        }
    }
    let mut owned = prepared(2);
    let bytes = owned.history().retained_bytes();
    let ((), released) = allocator::measure(Some(0), || owned.clear_history());
    assert_eq!(released.attempts, 0);
    assert_eq!(released.live_bytes, -(bytes as i128));
    let mut screen = Screen::new(4, 3).unwrap();
    let (result, stats) = allocator::measure(Some(0), || {
        screen.set_history_limits(HistoryLimits {
            max_rows: 3,
            max_bytes: 4096,
        })
    });
    result.unwrap();
    assert_eq!(stats.attempts, 0);
    for _ in 0..3 {
        screen.scroll_up(1, StyleId(0)).unwrap();
    }
    let ((), stats) = allocator::measure(Some(0), || {
        for _ in 0..128 {
            screen.scroll_up(1, StyleId(1)).unwrap();
        }
        screen.scroll_viewport_up(usize::MAX);
        black_box(screen.viewport().iter_rows().next());
        black_box(screen.viewport().cursor());
        screen.scroll_viewport_down(usize::MAX);
        screen.scroll_viewport_to_bottom();
        screen
            .set_history_limits(HistoryLimits {
                max_rows: 1,
                max_bytes: 1,
            })
            .unwrap();
        screen.scroll_up(1, StyleId(1)).unwrap();
        screen.clear_history();
    });
    assert_eq!(stats.attempts, 0);
}

#[test]
fn automatic_history_preflights_complete_text_callbacks_and_counted_scrolls() {
    for case in 0..9 {
        let prepare = || {
            let mut screen = Screen::new(4, if case == 2 || case == 3 { 1 } else { 2 }).unwrap();
            screen
                .set_history_limits(HistoryLimits {
                    max_rows: if case == 8 {
                        8
                    } else if case >= 6 {
                        1
                    } else {
                        3
                    },
                    max_bytes: if case == 8 { 512 } else { 4096 },
                })
                .unwrap();
            let mut writer = UnicodeWriter::default();
            if case == 8 {
                for _ in 0..4 {
                    screen.scroll_up(1, StyleId(0)).unwrap();
                }
                screen.set_cursor(0, 0);
                screen
                    .write(
                        &(String::from("e") + &"\u{301}".repeat(160)),
                        CellWidth::Single,
                        StyleId(1),
                    )
                    .unwrap();
                screen.set_cursor(1, 3);
            } else if case >= 6 {
                screen.set_cursor(0, usize::from(case == 7));
                screen
                    .write("e\u{301}", CellWidth::Single, StyleId(1))
                    .unwrap();
                screen.scroll_up(1, StyleId(0)).unwrap();
                screen.set_cursor(1, 3);
                writer.write(&mut screen, "❤", StyleId(1)).unwrap();
            } else if case == 3 {
                writer.write(&mut screen, "abc", StyleId(0)).unwrap();
                writer.write(&mut screen, "\u{600}", StyleId(1)).unwrap();
            } else {
                screen
                    .write("e\u{301}", CellWidth::Single, StyleId(1))
                    .unwrap();
                screen.set_cursor(screen.grid().rows() - 1, 3);
                if case == 2 {
                    writer.write(&mut screen, "❤", StyleId(1)).unwrap();
                }
            }
            if case == 4 {
                screen.resize(8, 2, StyleId(0)).unwrap();
            }
            if case == 5 {
                for _ in 0..3 {
                    screen.scroll_up(1, StyleId(0)).unwrap();
                }
                screen.set_cursor(0, 0);
                screen
                    .write("e\u{301}", CellWidth::Single, StyleId(1))
                    .unwrap();
                screen.set_cursor(1, 3);
            }
            screen.scroll_viewport_up(1);
            (screen, writer)
        };
        let operation = |screen: &mut Screen, writer: &mut UnicodeWriter| match case {
            0 | 4 | 5 | 8 => screen.write_ascii(
                b"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJK",
                StyleId(2),
            ),
            1 => screen.scroll_up(usize::MAX, StyleId(2)),
            2 | 6 | 7 => writer.write(screen, "\u{fe0f}", StyleId(2)),
            _ => writer.write(screen, "abcdefghijkl", StyleId(2)),
        };
        let (mut expected, mut writer) = prepare();
        let (result, stats) = allocator::measure(None, || operation(&mut expected, &mut writer));
        result.unwrap();
        assert!(stats.attempts > 0);
        if case == 5 {
            let (result, warm) =
                allocator::measure(Some(0), || operation(&mut expected, &mut writer));
            result.unwrap();
            assert_eq!(warm.attempts, 0);
            // Restore the one-callback expectation for fault-injection retries.
            let (mut screen, mut writer) = prepare();
            operation(&mut screen, &mut writer).unwrap();
            expected = screen;
        }
        for point in 0..stats.attempts {
            let (mut screen, mut writer) = prepare();
            let before = snapshot(screen.grid());
            let before_history = retained(screen.history());
            let alternate = snapshot(screen.buffer(ScreenBuffer::Alternate));
            let cursor = screen.cursor();
            let offset = screen.viewport_offset();
            let (result, failed) =
                allocator::measure(Some(point), || operation(&mut screen, &mut writer));
            assert!(
                matches!(result, Err(StorageError::Allocation(_))),
                "case {case}, point {point}"
            );
            assert_eq!(failed.failures, 1);
            assert_eq!(
                snapshot(screen.grid()),
                before,
                "case {case}, point {point}"
            );
            assert_eq!(retained(screen.history()), before_history);
            assert_eq!(snapshot(screen.buffer(ScreenBuffer::Alternate)), alternate);
            assert_eq!(screen.cursor(), cursor);
            assert_eq!(screen.viewport_offset(), offset);
            operation(&mut screen, &mut writer).unwrap();
            assert_eq!(snapshot(screen.grid()), snapshot(expected.grid()));
            assert_eq!(retained(screen.history()), retained(expected.history()));
            assert_eq!(screen.cursor(), expected.cursor());
            assert_eq!(screen.viewport_offset(), expected.viewport_offset());
        }
    }
}

#[test]
fn history_control_failures_retain_consumed_events_and_retry_without_partial_scrolling() {
    for (control, new_line) in [
        (b"\n".as_slice(), false),
        (b"\x0b", false),
        (b"\x0c", false),
        (b"\n", true),
        (b"\x1bD", false),
        (b"\x1bE", false),
        (b"\x1b[2S", false),
    ] {
        let prepare = || {
            let mut screen = Screen::new(4, 2).unwrap();
            screen
                .set_history_limits(HistoryLimits {
                    max_rows: 3,
                    max_bytes: 4096,
                })
                .unwrap();
            screen
                .write("e\u{301}", CellWidth::Single, StyleId(0))
                .unwrap();
            screen.set_cursor(1, 3);
            screen.write_ascii(b"x", StyleId(0)).unwrap();
            let mut processor = Processor::default();
            let mut writer = UnicodeWriter::default();
            if new_line {
                processor
                    .advance(b"\x1b[20h", &mut screen, &mut writer)
                    .unwrap();
            }
            (screen, processor, writer)
        };
        let wire = [control, b"\r"].concat();
        let (mut expected, mut processor, mut writer) = prepare();
        let (result, stats) = allocator::measure(None, || {
            processor.advance(&wire, &mut expected, &mut writer)
        });
        result.unwrap();
        assert!(stats.attempts > 0);
        for point in 0..stats.attempts {
            let (mut screen, mut processor, mut writer) = prepare();
            let before = snapshot(screen.grid());
            let cursor = screen.cursor();
            let (result, failed) = allocator::measure(Some(point), || {
                processor.advance(&wire, &mut screen, &mut writer)
            });
            let error = result.unwrap_err();
            assert_eq!(error.consumed, control.len());
            assert!(matches!(
                error.error,
                ProcessFailure::Screen(StorageError::Allocation(_))
            ));
            assert_eq!(failed.failures, 1);
            assert_eq!(snapshot(screen.grid()), before);
            assert_eq!(screen.cursor(), cursor);
            assert!(screen.history().is_empty());
            let suffix = &wire[error.consumed..];
            let (again, _) = allocator::measure(Some(0), || {
                processor.advance(suffix, &mut screen, &mut writer)
            });
            let again = again.unwrap_err();
            assert_eq!(again.consumed, 0);
            assert!(matches!(
                again.error,
                ProcessFailure::Screen(StorageError::Allocation(_))
            ));
            assert_eq!(snapshot(screen.grid()), before);
            assert_eq!(screen.cursor(), cursor);
            processor.advance(suffix, &mut screen, &mut writer).unwrap();
            assert_eq!(snapshot(screen.grid()), snapshot(expected.grid()));
            assert_eq!(retained(screen.history()), retained(expected.history()));
            assert_eq!(screen.cursor(), expected.cursor());
        }
    }
}

#[test]
fn unicode_writer_failures_preserve_clusters_and_retry_processor_owned_bytes() {
    let prefixes = [
        "\u{600}",
        "abc❤",
        "\x1b[2;3r\x1b[3;4H❤",
        "abcdef\r\x1b[C\x1b[4h❤",
        "ab⌚",
        "e\u{301}",
    ];
    let suffixes = [
        "abcdefgh",
        "\u{fe0f}X",
        "\u{fe0f}X",
        "\u{fe0f}X",
        "\u{fe0e}X",
        "\u{308}X",
    ];
    for (index, (prefix, suffix)) in prefixes.into_iter().zip(suffixes).enumerate() {
        let prepare = || {
            let mut screen = Screen::new(if index == 0 || index == 3 { 8 } else { 4 }, 3).unwrap();
            let mut processor = Processor::default();
            let mut writer = UnicodeWriter::default();
            processor
                .advance(prefix.as_bytes(), &mut screen, &mut writer)
                .unwrap();
            (screen, processor, writer)
        };
        let (mut expected, mut processor, mut writer) = prepare();
        let (result, stats) = allocator::measure(None, || {
            processor.advance(suffix.as_bytes(), &mut expected, &mut writer)
        });
        result.unwrap();
        assert!(stats.attempts > 0);
        for point in 0..stats.attempts {
            let (mut screen, mut processor, mut writer) = prepare();
            let before = snapshot(screen.grid());
            let cursor = screen.cursor();
            let (result, failed) = allocator::measure(Some(point), || {
                processor.advance(suffix.as_bytes(), &mut screen, &mut writer)
            });
            let error = result.unwrap_err();
            assert!(matches!(
                error.error,
                ProcessFailure::Text(StorageError::Allocation(_))
            ));
            assert_eq!(failed.failures, 1);
            assert_eq!(snapshot(screen.grid()), before);
            assert_eq!(screen.cursor(), cursor);
            let suffix = &suffix.as_bytes()[error.consumed..];
            let (again, _) = allocator::measure(Some(0), || {
                processor.advance(suffix, &mut screen, &mut writer)
            });
            let again = again.unwrap_err();
            assert_eq!(again.consumed, 0);
            assert!(matches!(
                again.error,
                ProcessFailure::Text(StorageError::Allocation(_))
            ));
            assert_eq!(snapshot(screen.grid()), before);
            assert_eq!(screen.cursor(), cursor);
            processor.advance(suffix, &mut screen, &mut writer).unwrap();
            assert_eq!(snapshot(screen.grid()), snapshot(expected.grid()));
            assert_eq!(screen.cursor(), expected.cursor());
        }
    }
    let (writer, stats) = allocator::measure(Some(0), UnicodeWriter::default);
    assert_eq!(stats.attempts, 0);
    let mut writer = writer;
    let mut screen = Screen::new(8, 2).unwrap();
    let mut processor = Processor::default();
    let (result, stats) = allocator::measure(Some(0), || {
        processor.advance(&[b'x'; 4096], &mut screen, &mut writer)
    });
    result.unwrap();
    assert_eq!(stats.attempts, 0);
}

#[test]
fn unit_revision_reserves_before_changes_and_reuses_warmed_storage() {
    fn prepared(case: usize, insert: bool) -> (Screen, CellPosition) {
        let mut screen = Screen::new(7, 4).unwrap();
        screen.write_ascii(b"primary", StyleId(8)).unwrap();
        screen.select_buffer(ScreenBuffer::Alternate);
        for row in 0..4 {
            screen.set_cursor(row, 0);
            screen.write_ascii(b"abcdefg", StyleId(1)).unwrap();
        }
        let position = CellPosition {
            row: if case == 4 { 2 } else { 1 },
            column: if case >= 3 { 6 } else { 1 },
        };
        screen.set_cursor(position.row, position.column);
        screen
            .write(
                if case == 1 || case == 2 {
                    "e\u{301}"
                } else {
                    "❤"
                },
                if case == 2 {
                    CellWidth::Double
                } else {
                    CellWidth::Single
                },
                StyleId(2),
            )
            .unwrap();
        screen.save_cursor();
        screen.set_scroll_region(1..3).unwrap();
        screen.set_origin_mode(true);
        screen.set_cursor(0, 4);
        screen.set_insert_mode(insert);
        screen.clear_tab_stops();
        screen.set_tab_stop();
        (screen, position)
    }
    let text = String::from("❤\u{fe0f}") + &"\u{301}".repeat(200);
    for case in 0..5 {
        for insert in [false, true] {
            let width = if case == 2 {
                CellWidth::Single
            } else {
                CellWidth::Double
            };
            let (mut expected, position) = prepared(case, insert);
            let (result, stats) = allocator::measure(None, || {
                expected.revise_unit(position, &text, width, StyleId(9))
            });
            let target = result.unwrap();
            let expected_cursor = expected.cursor();
            assert!(stats.attempts > 0);
            let expected_primary = snapshot(expected.buffer(ScreenBuffer::Primary));
            let expected_alternate = snapshot(expected.buffer(ScreenBuffer::Alternate));
            expected.restore_cursor();
            let expected_saved = expected.cursor();
            for point in 0..stats.attempts {
                let (mut screen, position) = prepared(case, insert);
                let primary = snapshot(screen.buffer(ScreenBuffer::Primary));
                let alternate = snapshot(screen.buffer(ScreenBuffer::Alternate));
                let cursor = screen.cursor();
                let (result, failed) = allocator::measure(Some(point), || {
                    screen.revise_unit(position, &text, width, StyleId(9))
                });
                assert!(matches!(result, Err(StorageError::Allocation(_))));
                assert_eq!(failed.failures, 1);
                assert_eq!(snapshot(screen.buffer(ScreenBuffer::Primary)), primary);
                assert_eq!(snapshot(screen.buffer(ScreenBuffer::Alternate)), alternate);
                assert_eq!(screen.cursor(), cursor);
                assert_eq!(screen.active_buffer(), ScreenBuffer::Alternate);
                assert_eq!(screen.scroll_region(), 1..3);
                assert!(screen.origin_mode());
                assert_eq!(screen.insert_mode(), insert);
                assert_eq!(screen.is_tab_stop(4), Some(true));
                assert_eq!(screen.is_tab_stop(0), Some(false));
                assert_eq!(
                    screen
                        .revise_unit(position, &text, width, StyleId(9))
                        .unwrap(),
                    target
                );
                assert_eq!(
                    snapshot(screen.buffer(ScreenBuffer::Primary)),
                    expected_primary
                );
                assert_eq!(
                    snapshot(screen.buffer(ScreenBuffer::Alternate)),
                    expected_alternate
                );
                assert_eq!(screen.cursor(), expected_cursor);
                screen.restore_cursor();
                assert_eq!(screen.cursor(), expected_saved);
            }
        }
    }
    for insert in [false, true] {
        let (mut screen, position) = prepared(0, insert);
        screen
            .revise_unit(position, &text, CellWidth::Double, StyleId(9))
            .unwrap();
        let ((), stats) = allocator::measure(Some(0), || {
            for _ in 0..32 {
                for width in [CellWidth::Single, CellWidth::Double, CellWidth::Double] {
                    screen
                        .revise_unit(position, &text, width, StyleId(9))
                        .unwrap();
                }
            }
        });
        assert_eq!(stats.attempts, 0);
    }
}

#[test]
fn paired_resize_failures_preserve_both_buffers_and_saved_cursors() {
    use ScreenBuffer::{Alternate, Primary};
    fn prepared() -> Screen {
        let mut screen = Screen::new(3, 3).unwrap();
        screen.set_cursor(1, 1);
        screen
            .write("水\u{301}", CellWidth::Double, StyleId(1))
            .unwrap();
        screen.save_cursor();
        screen.select_buffer(Alternate);
        screen.set_cursor(2, 0);
        screen
            .write("e\u{301}", CellWidth::Single, StyleId(2))
            .unwrap();
        screen.save_cursor();
        screen.set_scroll_region(1..3).unwrap();
        screen.set_origin_mode(true);
        screen.clear_tab_stops();
        screen.set_tab_stop();
        screen.set_insert_mode(true);
        screen
    }
    let mut expected = prepared();
    let (result, stats) = allocator::measure(None, || expected.resize(130, 6, StyleId(7)));
    result.unwrap();
    let expected_primary = snapshot(expected.buffer(Primary));
    let expected_alternate = snapshot(expected.buffer(Alternate));
    for point in 0..stats.attempts {
        let mut screen = prepared();
        let primary = snapshot(screen.buffer(Primary));
        let alternate = snapshot(screen.buffer(Alternate));
        let cursor = screen.cursor();
        let (result, failed) =
            allocator::measure(Some(point), || screen.resize(130, 6, StyleId(7)));
        assert!(matches!(result, Err(StorageError::Allocation(_))));
        assert_eq!(failed.failures, 1);
        assert_eq!(screen.active_buffer(), Alternate);
        assert_eq!(snapshot(screen.buffer(Primary)), primary);
        assert_eq!(snapshot(screen.buffer(Alternate)), alternate);
        assert_eq!(screen.cursor(), cursor);
        assert_eq!(screen.scroll_region(), 1..3);
        assert!(screen.origin_mode() && screen.insert_mode());
        assert_eq!(screen.is_tab_stop(0), Some(true));
        assert_eq!(screen.is_tab_stop(1), Some(false));
        screen.resize(130, 6, StyleId(7)).unwrap();
        assert_eq!(snapshot(screen.buffer(Primary)), expected_primary);
        assert_eq!(snapshot(screen.buffer(Alternate)), expected_alternate);
        assert_eq!(screen.cursor(), expected.cursor());
        screen.select_buffer(Primary);
        screen.restore_cursor();
        assert_eq!(
            (
                screen.cursor().row,
                screen.cursor().column,
                screen.cursor().wrap_pending
            ),
            (1, 2, false)
        );
        screen.select_buffer(Alternate);
        screen.restore_cursor();
        assert_eq!((screen.cursor().row, screen.cursor().column), (2, 1));
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SnapshotCell {
    text: String,
    width: usize,
    style: StyleId,
    erased: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    columns: usize,
    rows: Vec<Vec<SnapshotCell>>,
    wraps: Vec<Option<usize>>,
}

fn snapshot(grid: &Grid) -> Snapshot {
    let rows = grid
        .iter_rows()
        .map(|row| {
            assert_eq!(row.cells().len(), grid.columns());
            row.cells()
                .iter()
                .enumerate()
                .map(|(column, cell)| {
                    if cell.is_continuation() {
                        assert!(column > 0);
                        assert_eq!(row.cells()[column - 1].width(), 2);
                        assert_eq!(row.cells()[column - 1].style(), cell.style());
                    } else if cell.width() == 2 {
                        assert!(column + 1 < grid.columns());
                        assert!(row.cells()[column + 1].is_continuation());
                    }
                    let text = match row.text(column).unwrap() {
                        CellText::Scalar(character) => character.to_string(),
                        CellText::Extended(text) => text.to_owned(),
                        CellText::Continuation => String::new(),
                    };
                    SnapshotCell {
                        text,
                        width: cell.width(),
                        style: cell.style(),
                        erased: cell.is_erased(),
                    }
                })
                .collect()
        })
        .collect();
    Snapshot {
        columns: grid.columns(),
        rows,
        wraps: grid.iter_rows().map(|row| row.wrap_end()).collect(),
    }
}

fn tab_stops(screen: &Screen) -> Vec<usize> {
    (0..screen.grid().columns())
        .filter(|&column| screen.is_tab_stop(column).unwrap())
        .collect()
}

fn populated() -> Grid {
    let mut grid = Grid::new(4, 3).unwrap();
    grid.write(0, 0, "水", CellWidth::Double, StyleId(1))
        .unwrap();
    grid.write(1, 2, "界\u{301}", CellWidth::Double, StyleId(2))
        .unwrap();
    grid.write_ascii(2, 0, b"last", StyleId(3)).unwrap();
    grid
}

fn extended() -> Grid {
    let mut grid = Grid::new(6, 3).unwrap();
    grid.write(0, 0, "e\u{301}", CellWidth::Single, StyleId(1))
        .unwrap();
    grid.write(0, 1, "水\u{301}", CellWidth::Double, StyleId(2))
        .unwrap();
    grid
}

fn dense() -> Grid {
    let mut grid = Grid::new(32, 2).unwrap();
    for column in 0..16 {
        grid.write(0, column, "e\u{301}", CellWidth::Single, StyleId(1))
            .unwrap();
    }
    grid
}

struct Scenario {
    name: &'static str,
    setup: fn() -> Grid,
    mutate: fn(&mut Grid) -> Result<(), StorageError>,
}

#[test]
fn screen_failures_preserve_contents_cursor_and_pending_wrap() {
    let (created, stats) = allocator::measure(None, || Screen::new(130, 2));
    drop(created.unwrap());
    for point in 0..stats.attempts {
        let (result, failed) = allocator::measure(Some(point), || Screen::new(130, 2));
        assert!(matches!(result, Err(StorageError::Allocation(_))));
        assert_eq!(failed.failures, 1);
        assert_eq!(failed.live_bytes, 0);
    }
    fn pending(rows: usize, gap: bool) -> Screen {
        let mut screen = Screen::new(4, rows).unwrap();
        screen.write_ascii(b"top!next", StyleId(1)).unwrap();
        screen.set_cursor(rows - 1, 2);
        screen.write("旧", CellWidth::Double, StyleId(2)).unwrap();
        if gap {
            screen.set_cursor(rows - 1, 3);
        }
        screen
    }

    type Case = (
        &'static str,
        fn() -> Screen,
        fn(&mut Screen) -> Result<(), StorageError>,
    );
    fn regional(region: std::ops::Range<usize>, gap: bool) -> Screen {
        let mut screen = pending(4, false);
        screen.set_scroll_region(region.clone()).unwrap();
        screen.set_cursor(region.end - 1, 2);
        screen.write("旧", CellWidth::Double, StyleId(2)).unwrap();
        if gap {
            screen.set_cursor(region.end - 1, 3);
        }
        screen
    }
    let cases: [Case; 18] = [
        (
            "overwrite a soft-wrap gap boundary",
            || {
                let mut screen = Screen::new(4, 3).unwrap();
                screen.write_ascii(b"abc", StyleId(1)).unwrap();
                screen.write("水", CellWidth::Double, StyleId(2)).unwrap();
                screen.set_cursor(0, 2);
                screen
            },
            |screen| screen.write("e\u{301}", CellWidth::Single, StyleId(3)),
        ),
        (
            "insert owned text into shifted suffix",
            || {
                let mut s = Screen::new(8, 2).unwrap();
                s.write("水\u{301}", CellWidth::Double, StyleId(1)).unwrap();
                s.set_cursor(0, 1);
                s.set_insert_mode(true);
                s
            },
            |s| s.write("e\u{301}", CellWidth::Single, StyleId(3)),
        ),
        (
            "insert after wide gap on physical bottom",
            || {
                let mut s = regional(0..1, true);
                s.set_cursor(3, 3);
                s.set_insert_mode(true);
                s
            },
            |s| s.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "insert after regional scrolling",
            || {
                let mut s = regional(1..3, false);
                s.set_insert_mode(true);
                s
            },
            |s| s.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "origin-mode wrap with owned text",
            || {
                let mut screen = regional(1..3, false);
                screen.set_origin_mode(true);
                screen.set_cursor(usize::MAX, usize::MAX);
                screen.write("!", CellWidth::Single, StyleId(2)).unwrap();
                screen
            },
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "origin-mode resize with pending wrap",
            || {
                let mut screen = regional(1..3, false);
                screen.set_origin_mode(true);
                screen.set_cursor(usize::MAX, usize::MAX);
                screen.write("!", CellWidth::Single, StyleId(2)).unwrap();
                screen
            },
            |screen| screen.resize(130, 6, StyleId(3)),
        ),
        (
            "write within row",
            || Screen::new(4, 2).unwrap(),
            |screen| screen.write("e\u{301}", CellWidth::Single, StyleId(3)),
        ),
        (
            "wrap into next row",
            || {
                let mut screen = pending(2, false);
                screen.set_cursor(0, 3);
                screen.write("x", CellWidth::Single, StyleId(1)).unwrap();
                screen
            },
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "wrap and scroll",
            || pending(2, false),
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "wide edge and scroll",
            || pending(2, true),
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "single-row wrap",
            || pending(1, false),
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "grow recycled text",
            || {
                let mut screen = pending(2, false);
                screen.set_cursor(0, 0);
                screen
                    .write("e\u{301}", CellWidth::Single, StyleId(1))
                    .unwrap();
                screen.set_cursor(1, 3);
                screen.write("x", CellWidth::Single, StyleId(1)).unwrap();
                screen
            },
            |screen| {
                screen.write(
                    "e\u{301}\u{302}\u{303}\u{304}",
                    CellWidth::Double,
                    StyleId(3),
                )
            },
        ),
        (
            "resize with pending wrap",
            || regional(1..3, false),
            |screen| screen.resize(12, 6, StyleId(3)),
        ),
        (
            "grow tabs and grid with pending wrap",
            || {
                let mut screen = regional(1..3, false);
                screen.clear_tab_stops();
                screen.set_cursor(0, 2);
                screen.set_tab_stop();
                screen.set_cursor(2, 3);
                screen.write("!", CellWidth::Single, StyleId(2)).unwrap();
                screen
            },
            |screen| screen.resize(130, 6, StyleId(3)),
        ),
        (
            "write owned text with automatic wrapping disabled",
            || {
                let mut screen = regional(1..3, false);
                screen.set_auto_wrap(false);
                screen
            },
            |screen| screen.write("e\u{301}", CellWidth::Single, StyleId(3)),
        ),
        (
            "regional wrap",
            || regional(1..3, false),
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "regional wide edge",
            || regional(1..3, true),
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
        (
            "single-row region",
            || regional(1..2, false),
            |screen| screen.write("水\u{301}", CellWidth::Double, StyleId(3)),
        ),
    ];
    let state = |screen: &Screen| {
        (
            snapshot(screen.grid()),
            screen.cursor(),
            screen.scroll_region(),
            tab_stops(screen),
            screen.auto_wrap(),
            screen.origin_mode(),
            screen.insert_mode(),
        )
    };
    for (name, setup, mutate) in cases {
        let mut baseline = setup();
        baseline.save_cursor();
        let (result, measured) = allocator::measure(None, || mutate(&mut baseline));
        result.unwrap();
        assert!(measured.attempts > 0, "{name}");
        let expected = state(&baseline);
        baseline.restore_cursor();
        let restored = state(&baseline);
        for point in 0..measured.attempts {
            let mut screen = setup();
            screen.save_cursor();
            let before = state(&screen);
            let (result, failed) = allocator::measure(Some(point), || mutate(&mut screen));
            assert!(
                matches!(result, Err(StorageError::Allocation(_))),
                "{name} at {point}: {result:?}"
            );
            assert_eq!(failed.failures, 1);
            assert_eq!(state(&screen), before, "{name} at {point}");
            screen.restore_cursor();
            assert_eq!(state(&screen), before, "{name} saved state at {point}");
            mutate(&mut screen).unwrap();
            assert_eq!(state(&screen), expected, "{name} retry at {point}");
            screen.restore_cursor();
            assert_eq!(
                state(&screen),
                restored,
                "{name} saved state after retry at {point}"
            );
        }
        eprintln!("{name}: {} failure points covered", measured.attempts);
    }
}

#[test]
fn allocation_failures_preserve_contents_and_allow_retry() {
    let scenarios = [
        Scenario {
            name: "write new extended text over a continuation",
            setup: populated,
            mutate: |grid| grid.write(0, 1, "e\u{301}", CellWidth::Single, StyleId(4)),
        },
        Scenario {
            name: "grow existing text while replacing a neighboring wide unit",
            setup: extended,
            mutate: |grid| {
                grid.write(
                    0,
                    0,
                    "e\u{301}\u{302}\u{303}\u{304}",
                    CellWidth::Double,
                    StyleId(4),
                )
            },
        },
        Scenario {
            name: "grow the extended-text index",
            setup: dense,
            mutate: |grid| grid.write(0, 16, "e\u{301}", CellWidth::Single, StyleId(4)),
        },
        Scenario {
            name: "append first extended text through a continuation",
            setup: populated,
            mutate: |grid| grid.append_text(0, 1, "\u{301}"),
        },
        Scenario {
            name: "grow appended text",
            setup: extended,
            mutate: |grid| grid.append_text(0, 0, "\u{301}\u{302}\u{303}\u{304}"),
        },
        Scenario {
            name: "append a display unit to erased storage",
            setup: || Grid::new(4, 2).unwrap(),
            mutate: |grid| grid.append_text(0, 0, "\u{301}"),
        },
        Scenario {
            name: "grow rows and columns",
            setup: populated,
            mutate: |grid| grid.resize(12, 6, StyleId(4)),
        },
        Scenario {
            name: "grow columns before discarding bottom rows",
            setup: populated,
            mutate: |grid| grid.resize(12, 1, StyleId(4)),
        },
        Scenario {
            name: "allocate new rows before cropping a wide unit",
            setup: populated,
            mutate: |grid| grid.resize(3, 6, StyleId(4)),
        },
    ];

    for scenario in scenarios {
        let mut baseline = (scenario.setup)();
        let (result, measured) =
            allocator::measure(None, || (scenario.mutate)(black_box(&mut baseline)));
        result.unwrap();
        assert!(
            measured.attempts > 0,
            "{} did not exercise allocation",
            scenario.name
        );
        let expected = snapshot(&baseline);

        for point in 0..measured.attempts {
            let mut grid = (scenario.setup)();
            let before = snapshot(&grid);
            let (result, failed) =
                allocator::measure(Some(point), || (scenario.mutate)(black_box(&mut grid)));
            assert!(
                matches!(result, Err(StorageError::Allocation(_))),
                "{} at {point}: {result:?}",
                scenario.name
            );
            assert_eq!(failed.failures, 1, "{} at {point}", scenario.name);
            assert_eq!(failed.attempts, point + 1, "{} at {point}", scenario.name);
            assert_eq!(snapshot(&grid), before, "{} at {point}", scenario.name);

            (scenario.mutate)(&mut grid).unwrap();
            assert_eq!(
                snapshot(&grid),
                expected,
                "{} retry at {point}",
                scenario.name
            );
            grid.write_ascii(0, 0, b"Z", StyleId(5)).unwrap();
            grid.append_text(0, 0, "\u{301}").unwrap();
            grid.resize(6, 4, StyleId(5)).unwrap();
            snapshot(&grid);
        }
        eprintln!(
            "{}: {} failure points covered",
            scenario.name, measured.attempts
        );
    }
}

#[test]
fn warm_writes_edits_scrolling_and_column_growth_make_no_allocation_requests() {
    for width in [CellWidth::Single, CellWidth::Double] {
        let mut grid = Grid::new(4, 1).unwrap();
        let text = "e\u{301}".repeat(64);
        grid.write(0, 0, &text, width, StyleId(1)).unwrap();
        let (result, stats) =
            allocator::measure(Some(0), || grid.write(0, 0, "x", width, StyleId(2)));
        result.unwrap();
        assert_eq!(stats.attempts, 0);
        assert!(
            stats.live_bytes < 0,
            "Overwriting owned text releases its payload"
        );
        snapshot(&grid);
    }

    fn owned_rows(region: std::ops::Range<usize>, insert: bool) -> Screen {
        let mut screen = Screen::new(8, 3).unwrap();
        let text = "水\u{301}".repeat(32);
        for row in 0..3 {
            screen.set_cursor(row, 0);
            for _ in 0..4 {
                screen
                    .write(&text, CellWidth::Double, StyleId(row as u32))
                    .unwrap();
            }
        }
        screen.set_scroll_region(region.clone()).unwrap();
        screen.set_cursor(region.end - 1, 7);
        screen.write("!", CellWidth::Single, StyleId(4)).unwrap();
        screen.set_insert_mode(insert);
        screen
    }
    for region in [0..1, 0..3, 1..3] {
        for insert in [false, true] {
            for count in [1, 8, 17] {
                let mut bulk = owned_rows(region.clone(), insert);
                let mut scalar = owned_rows(region.clone(), insert);
                let bytes = [b'x'; 17];
                let (result, stats) =
                    allocator::measure(Some(0), || bulk.write_ascii(&bytes[..count], StyleId(9)));
                result.unwrap();
                assert_eq!(stats.attempts, 0);
                assert!(
                    stats.live_bytes < 0,
                    "Recycled rows release their owned text"
                );
                let (result, scalar_stats) = allocator::measure(Some(0), || {
                    for _ in 0..count {
                        scalar.write("x", CellWidth::Single, StyleId(9))?;
                    }
                    Ok::<_, StorageError>(())
                });
                result.unwrap();
                assert_eq!(scalar_stats.attempts, 0);
                assert_eq!(stats.live_bytes, scalar_stats.live_bytes);
                assert_eq!(bulk.cursor(), scalar.cursor());
                assert_eq!(snapshot(bulk.grid()), snapshot(scalar.grid()));
            }
        }
    }

    let mut reset = Screen::new(130, 4).unwrap();
    reset.resize(65, 3, StyleId(0)).unwrap();
    for row in 0..3 {
        for column in 0..65 {
            reset.set_cursor(row, column);
            reset
                .write("e\u{301}", CellWidth::Single, StyleId(1))
                .unwrap();
        }
    }
    reset.set_origin_mode(true);
    reset.save_cursor();
    let ((), stats) = allocator::measure(Some(0), || {
        reset.reset(StyleId(2));
        reset.reset(StyleId(3));
        reset.resize(130, 3, StyleId(0)).unwrap();
        reset.reset(StyleId(4));
        reset.resize(65, 3, StyleId(0)).unwrap();
    });
    assert_eq!(stats.attempts, 0);
    snapshot(reset.grid());

    let mut grid = Grid::new(240, 80).unwrap();
    grid.resize(120, 80, StyleId(0)).unwrap();
    let ascii = [b'x'; 120];
    let (result, stats) = allocator::measure(Some(0), || {
        for _ in 0..32 {
            grid.write_ascii(0, 0, black_box(&ascii), StyleId(1))?;
            grid.write(1, 0, "水", CellWidth::Double, StyleId(2))?;
            grid.insert_cells(0, 17, 1, StyleId(0))?;
            grid.delete_cells(0, 17, 1, StyleId(0))?;
            grid.scroll_up(0..80, 17, StyleId(0))?;
            grid.scroll_down(1..79, 1, StyleId(0))?;
            grid.scroll_up(1..79, 17, StyleId(0))?;
            grid.scroll_down(0..80, 1, StyleId(0))?;
            grid.scroll_up(2..78, usize::MAX, StyleId(0))?;
            grid.clear_row(0, 0..120, StyleId(0))?;
            grid.resize(240, 80, StyleId(0))?;
            grid.resize(120, 80, StyleId(0))?;
        }
        Ok::<_, StorageError>(())
    });
    result.unwrap();
    assert_eq!(stats.attempts, 0);
    assert_eq!(stats.live_bytes, 0);
    snapshot(&grid);

    let mut grid = Grid::new(4, 9).unwrap();
    for row in 0..9 {
        grid.write(row, 1, "水\u{301}", CellWidth::Double, StyleId(2))
            .unwrap();
    }
    grid.scroll_up(0..9, 3, StyleId(0)).unwrap();
    let (result, stats) = allocator::measure(Some(0), || {
        for row in 0..6 {
            grid.insert_cells(row, 0, 1, StyleId(0))?;
            grid.delete_cells(row, 0, 1, StyleId(0))?;
        }
        grid.insert_cells(0, 2, 1, StyleId(0))?;
        grid.delete_cells(1, 1, 1, StyleId(0))?;
        grid.scroll_down(1..8, 2, StyleId(0))?;
        grid.scroll_up(1..8, 3, StyleId(0))?;
        grid.scroll_down(1..8, 4, StyleId(0))
    });
    result.unwrap();
    assert_eq!(stats.attempts, 0);
    assert!(stats.live_bytes <= 0);
    snapshot(&grid);

    let mut screen = Screen::new(240, 24).unwrap();
    screen.resize(120, 24, StyleId(0)).unwrap();
    let (result, stats) = allocator::measure(Some(0), || {
        for _ in 0..64 {
            screen.set_insert_mode(true);
            screen.set_auto_wrap(false);
            screen.set_cursor(21, 119);
            screen.write("水\u{301}", CellWidth::Double, StyleId(2))?;
            screen.write_ascii(&[b'x'; 240], StyleId(1))?;
            screen.save_cursor();
            screen.set_auto_wrap(true);
            screen.set_tab_stop();
            screen.clear_tab_stop();
            screen.clear_tab_stops();
            screen.tab_forward(0);
            screen.tab_forward(1);
            screen.tab_backward(8);
            screen.tab_forward(usize::MAX);
            screen.tab_backward(usize::MAX);
            screen.set_scroll_region(2..22)?;
            screen.set_origin_mode(true);
            screen.set_cursor(usize::MAX, usize::MAX);
            screen.write_ascii(&[b'x'; 240], StyleId(1))?;
            screen.backspace();
            screen.write("水", CellWidth::Double, StyleId(2))?;
            screen.carriage_return();
            screen.line_feed(StyleId(0)).unwrap();
            screen.set_cursor(0, 0);
            screen.reverse_index(StyleId(0));
            screen.set_scroll_region(0..24)?;
            screen.set_cursor(23, 0);
            screen.write_ascii(&[b'x'; 240], StyleId(1))?;
            screen.resize(240, 24, StyleId(0))?;
            screen.resize(120, 24, StyleId(0))?;
            screen.set_origin_mode(false);
            screen.restore_cursor();
        }
        Ok::<_, StorageError>(())
    });
    result.unwrap();
    assert_eq!(stats.attempts, 0);
    assert_eq!(stats.live_bytes, 0);
    snapshot(screen.grid());

    let prepare = || {
        let mut screen = Screen::new(8, 4).unwrap();
        for row in 0..4 {
            for column in [0, 2, 4, 6] {
                screen.set_cursor(row, column);
                screen
                    .write("水\u{301}", CellWidth::Double, StyleId(1))
                    .unwrap();
            }
        }
        screen.set_scroll_region(1..3).unwrap();
        screen.set_cursor(2, 3);
        screen
    };
    let mut screens: Vec<_> = [false, true]
        .into_iter()
        .flat_map(|display| {
            [EraseMode::FromCursor, EraseMode::ToCursor, EraseMode::All]
                .map(|mode| (display, mode, prepare()))
        })
        .collect();
    let edits = [
        Screen::insert_cells,
        Screen::delete_cells,
        Screen::erase_cells,
    ];
    let mut edit_screens = [prepare(), prepare(), prepare()];
    let line_edits = [Screen::insert_lines, Screen::delete_lines];
    let mut line_screens = [prepare(), prepare()];
    let scrolls: [fn(&mut Screen, usize, StyleId); 2] = [
        |screen, count, style| screen.scroll_up(count, style).unwrap(),
        Screen::scroll_down,
    ];
    let mut scroll_screens = [prepare(), prepare()];
    for screen in &mut scroll_screens {
        screen.set_cursor(0, 7);
        screen.write("!", CellWidth::Single, StyleId(1)).unwrap();
    }
    for screen in &mut line_screens {
        screen.set_cursor(1, 7);
        screen.write("!", CellWidth::Single, StyleId(1)).unwrap();
    }
    let mut moving = prepare();
    let moves = [
        Screen::move_cursor_up,
        Screen::move_cursor_down,
        Screen::move_cursor_left,
        Screen::move_cursor_right,
    ];
    let ((), stats) = allocator::measure(Some(0), || {
        for move_cursor in moves {
            for row in 0..4 {
                moving.set_cursor(row, 7);
                moving.write("!", CellWidth::Single, StyleId(1)).unwrap();
                for count in [0, 1, usize::MAX] {
                    move_cursor(&mut moving, count);
                }
            }
        }
        for (display, mode, screen) in &mut screens {
            if *display {
                screen.erase_display(*mode, StyleId(9));
            } else {
                screen.erase_line(*mode, StyleId(9));
            }
        }
        for (edit, screen) in edits.iter().zip(&mut edit_screens) {
            edit(screen, 1, StyleId(9));
            edit(screen, 0, StyleId(8));
            edit(screen, usize::MAX, StyleId(9));
        }
        for (edit, screen) in line_edits.iter().zip(&mut line_screens) {
            edit(screen, 1, StyleId(9));
            edit(screen, 0, StyleId(8));
            edit(screen, usize::MAX, StyleId(9));
        }
        for (scroll, screen) in scrolls.iter().zip(&mut scroll_screens) {
            scroll(screen, 0, StyleId(8));
            scroll(screen, 1, StyleId(9));
            scroll(screen, usize::MAX, StyleId(9));
        }
    });
    assert_eq!(stats.attempts, 0);
    assert!(stats.live_bytes <= 0);
    for (_, _, screen) in &screens {
        snapshot(screen.grid());
    }
    for screen in edit_screens.iter().chain(&line_screens) {
        snapshot(screen.grid());
    }
    for screen in &scroll_screens {
        snapshot(screen.grid());
    }
}

#[test]
fn resize_history_reaches_a_stable_requested_memory_high_water() {
    let ((initial, retained, peaks), released) = allocator::measure(None, || {
        let mut grid = Grid::new(128, 8).unwrap();
        let initial = allocator::stats().live_bytes;
        let mut retained = [0; 16];
        let mut peaks = [0; 16];
        for cycle in 0..16 {
            for (columns, rows) in [
                (128, 8),
                (64, 16),
                (32, 32),
                (16, 64),
                (8, 128),
                (4, 256),
                (2, 512),
                (1, 1024),
            ] {
                grid.resize(columns, rows, StyleId(0)).unwrap();
            }
            grid.resize(128, 8, StyleId(0)).unwrap();
            grid.write(0, 0, "水\u{301}", CellWidth::Double, StyleId(1))
                .unwrap();
            grid.append_text(0, 1, "\u{302}").unwrap();
            let measured = allocator::stats();
            retained[cycle] = measured.live_bytes;
            peaks[cycle] = measured.peak_bytes;
        }
        drop(grid);
        (initial, retained, peaks)
    });

    assert!(initial > 0);
    assert!(retained[0] >= initial);
    assert!(retained.iter().all(|&bytes| bytes == retained[0]));
    assert!(peaks[1..].iter().all(|&bytes| bytes == peaks[1]));
    assert_eq!(released.live_bytes, 0);
    eprintln!(
        "Requested heap bytes: initial={initial}, retained={}, peak={}, after_drop={}",
        retained[0], released.peak_bytes, released.live_bytes
    );
}

#[test]
fn screen_reflow_text_transfer_allocations_follow_row_count() {
    let count = |columns, rows, target| {
        let mut screen = Screen::new(columns, rows).unwrap();
        screen
            .set_history_limits(HistoryLimits {
                max_rows: 8,
                max_bytes: 1 << 20,
            })
            .unwrap();
        for _ in 0..columns * rows {
            screen
                .write("e\u{301}", CellWidth::Single, StyleId(1))
                .unwrap();
        }
        let (result, stats) = allocator::measure(None, || screen.reflow(target, 2, StyleId(9)));
        result.unwrap();
        stats.attempts
    };
    assert_eq!(count(64, 2, 32), count(1024, 2, 512));
    assert_eq!(count(60, 4, 80), count(120, 2, 80));
}

#[test]
fn screen_reflow_failures_release_staging_and_preserve_both_buffers_and_saved_states() {
    let prepare = |alternate: bool| {
        let mut screen = Screen::new(5, 4).unwrap();
        screen
            .set_history_limits(HistoryLimits {
                max_rows: 5,
                max_bytes: 1 << 20,
            })
            .unwrap();
        for index in 0..40 {
            screen
                .write(
                    if index % 2 == 0 {
                        "e\u{301}"
                    } else {
                        "👩\u{200d}💻"
                    },
                    if index % 2 == 0 {
                        CellWidth::Single
                    } else {
                        CellWidth::Double
                    },
                    StyleId(index % 4),
                )
                .unwrap();
        }
        screen.scroll_viewport_up(2);
        screen.set_scroll_region(1..4).unwrap();
        screen.set_origin_mode(true);
        screen.set_cursor(1, 2);
        screen.save_cursor();
        screen.select_buffer(ScreenBuffer::Alternate);
        screen.set_origin_mode(false);
        screen.write_ascii(b"abcdefghijk", StyleId(6)).unwrap();
        screen.save_cursor();
        screen.set_cursor(0, 2);
        screen.set_tab_stop();
        screen.set_insert_mode(true);
        if !alternate {
            screen.select_buffer(ScreenBuffer::Primary);
            screen.restore_cursor();
        }
        screen
    };
    let state = |screen: &Screen| {
        (
            snapshot(screen.buffer(ScreenBuffer::Primary)),
            snapshot(screen.buffer(ScreenBuffer::Alternate)),
            retained(screen.history()),
            (
                screen.cursor(),
                screen.cursor_appearance(),
                screen.scroll_region(),
                screen.origin_mode(),
                screen.auto_wrap(),
                screen.insert_mode(),
                screen.active_buffer(),
            ),
            (screen.viewport().range(), screen.viewport_offset()),
            (0..screen.grid().columns())
                .map(|column| screen.is_tab_stop(column))
                .collect::<Vec<_>>(),
            [ScreenBuffer::Primary, ScreenBuffer::Alternate]
                .into_iter()
                .flat_map(|buffer| screen.buffer(buffer).iter_rows())
                .chain(screen.history().iter_rows())
                .map(|row| {
                    (
                        row.cells().as_ptr(),
                        (0..row.cells().len())
                            .filter_map(|column| {
                                if let CellText::Extended(text) = row.text(column).unwrap() {
                                    Some(text.as_ptr())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>(),
        )
    };
    let saved = |screen: &mut Screen| {
        screen.select_buffer(ScreenBuffer::Primary);
        screen.restore_cursor();
        let primary = (
            screen.cursor(),
            screen.origin_mode(),
            screen.auto_wrap(),
            screen.viewport_offset(),
        );
        screen.select_buffer(ScreenBuffer::Alternate);
        screen.restore_cursor();
        (
            primary,
            (screen.cursor(), screen.origin_mode(), screen.auto_wrap()),
        )
    };
    for (alternate, columns, height) in [(false, 3, 3), (true, 9, 6)] {
        let mut expected = prepare(alternate);
        let before = state(&expected);
        let (result, noop) = allocator::measure(Some(0), || expected.reflow(5, 4, StyleId(9)));
        result.unwrap();
        assert_eq!(noop.attempts, 0);
        assert_eq!(state(&expected), before);
        let (result, stats) =
            allocator::measure(None, || expected.reflow(columns, height, StyleId(9)));
        result.unwrap();
        assert!(stats.attempts > 0);
        let expected_state = state(&expected);
        let expected_saved = saved(&mut expected);
        for point in 0..stats.attempts {
            let mut screen = prepare(alternate);
            let before = state(&screen);
            let (result, failed) =
                allocator::measure(Some(point), || screen.reflow(columns, height, StyleId(9)));
            assert!(matches!(result, Err(StorageError::Allocation(_))));
            assert_eq!(failed.failures, 1);
            assert_eq!(failed.live_bytes, 0);
            assert_eq!(state(&screen), before);
            screen.reflow(columns, height, StyleId(9)).unwrap();
            let actual = state(&screen);
            // Independent successful builds own different buffers; compare their values.
            assert_eq!(
                (
                    &actual.0, &actual.1, &actual.2, &actual.3, &actual.4, &actual.5
                ),
                (
                    &expected_state.0,
                    &expected_state.1,
                    &expected_state.2,
                    &expected_state.3,
                    &expected_state.4,
                    &expected_state.5
                )
            );
            assert_eq!(saved(&mut screen), expected_saved);
        }
    }
}

#[test]
fn reflow_row_allocation_failures_release_partial_storage_and_preserve_sources() {
    let contents = |row: &Row| {
        (
            row.cells()
                .iter()
                .enumerate()
                .map(|(column, cell)| {
                    let text = match row.text(column).unwrap() {
                        CellText::Scalar(value) => value.to_string(),
                        CellText::Extended(value) => value.to_owned(),
                        CellText::Continuation => String::new(),
                    };
                    (cell.width(), cell.style(), cell.is_erased(), text)
                })
                .collect::<Vec<_>>(),
            row.wrap_end(),
        )
    };
    let long = String::from("e") + &"\u{301}".repeat(511);
    for case in 0..4 {
        let mut screen = Screen::new(5, 3).unwrap();
        screen
            .set_history_limits(HistoryLimits {
                max_rows: 8,
                max_bytes: 1 << 20,
            })
            .unwrap();
        for index in 0..32 {
            let (text, width) = match case {
                0 => ("x", CellWidth::Single),
                1 => ("水", CellWidth::Double),
                2 if index % 3 == 0 => ("👩\u{200d}💻", CellWidth::Double),
                2 => ("e\u{301}", CellWidth::Single),
                _ => (long.as_str(), CellWidth::Single),
            };
            screen.write(text, width, StyleId(index % 4)).unwrap();
        }
        screen.scroll_viewport_up(1);
        let before = snapshot(screen.grid());
        let history = retained(screen.history());
        let cursor = screen.cursor();
        let offset = screen.viewport_offset();
        let columns = if case == 1 { 3 } else { 17 };
        let layout = screen
            .view()
            .logical_line(0)
            .unwrap()
            .reflow(columns)
            .unwrap()
            .next()
            .unwrap();
        let (result, stats) = allocator::measure(None, || layout.materialize(StyleId(9)));
        let expected = contents(&result.unwrap());
        assert!(stats.attempts > 0);
        for point in 0..stats.attempts {
            let (result, failed) =
                allocator::measure(Some(point), || layout.materialize(StyleId(9)));
            assert!(matches!(result, Err(StorageError::Allocation(_))));
            assert_eq!(failed.failures, 1);
            assert_eq!(failed.live_bytes, 0);
            assert_eq!(snapshot(screen.grid()), before);
            assert_eq!(retained(screen.history()), history);
            assert_eq!(screen.cursor(), cursor);
            assert_eq!(screen.viewport_offset(), offset);
            assert_eq!(contents(&layout.materialize(StyleId(9)).unwrap()), expected);
        }
    }
}

#[test]
fn text_views_stream_without_allocation_and_propagate_sink_failures() {
    struct FailingSink {
        remaining: usize,
    }
    impl Write for FailingSink {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            self.remaining = self.remaining.checked_sub(text.len()).ok_or(fmt::Error)?;
            Ok(())
        }
    }
    for source in 0..3 {
        let with_history = source != 0;
        let mut screen = Screen::new(130, 4).unwrap();
        if with_history {
            screen
                .set_history_limits(HistoryLimits {
                    max_rows: 8,
                    max_bytes: 1 << 20,
                })
                .unwrap();
        }
        let long = "e\u{301}".repeat(341);
        for row in 0..4 {
            for column in 0..130 {
                screen
                    .write(
                        if (row + column) % 3 == 0 { &long } else { "é" },
                        CellWidth::Single,
                        StyleId(1),
                    )
                    .unwrap();
            }
        }
        screen.set_cursor(3, 0);
        screen.line_feed(StyleId(0)).unwrap();
        screen.set_cursor(3, 129);
        screen
            .write("水\u{301}", CellWidth::Double, StyleId(2))
            .unwrap();
        if with_history {
            screen.resize(66, 4, StyleId(0)).unwrap();
            screen.scroll_up(1, StyleId(0)).unwrap();
        }
        if source == 2 {
            screen.scroll_viewport_up(1);
        }
        let before = snapshot(screen.grid());
        let history = retained(screen.history());
        let cursor = screen.cursor();
        let range = CellPosition { row: 0, column: 1 }..CellPosition {
            row: if source == 2 {
                screen.viewport().rows()
            } else {
                screen.view().rows()
            } - 1,
            column: screen.grid().columns(),
        };
        let selection = || match source {
            0 => screen.grid().text(range.clone()).unwrap(),
            1 => screen.view().text(range.clone()).unwrap(),
            _ => screen.viewport().text(range.clone()).unwrap(),
        };
        let expected = selection().to_string();
        let mut output = String::with_capacity(expected.len());
        let (result, stats) = allocator::measure(Some(0), || {
            for _ in 0..3 {
                let view = selection();
                output.clear();
                write!(&mut output, "{view}")?;
            }
            Ok::<_, fmt::Error>(())
        });
        result.unwrap();
        assert_eq!(stats.attempts, 0);
        assert_eq!(output, expected);
        for remaining in [0, 1, expected.len() / 2] {
            let (result, stats) = allocator::measure(Some(0), || {
                write!(FailingSink { remaining }, "{}", selection())
            });
            assert!(result.is_err());
            assert_eq!(stats.attempts, 0);
        }
        assert_eq!(snapshot(screen.grid()), before);
        assert_eq!(retained(screen.history()), history);
        assert_eq!(screen.cursor(), cursor);
        let expected_lines: String = screen
            .view()
            .logical_lines()
            .map(|line| line.text().to_string())
            .collect();
        output = String::with_capacity(expected_lines.len());
        let (result, stats) = allocator::measure(Some(0), || {
            let view = screen.view();
            for line in view.logical_lines() {
                let found = view.logical_line(black_box(line.range().start)).unwrap();
                black_box(found.iter_rows().count());
                write!(&mut output, "{}", line.text())?;
            }
            for line in view.logical_lines().rev() {
                black_box(line.range());
            }
            Ok::<_, fmt::Error>(())
        });
        result.unwrap();
        assert_eq!(stats.attempts, 0);
        assert_eq!(output, expected_lines);
        for columns in [2, 7, 129] {
            output.clear();
            let (result, stats) = allocator::measure(Some(0), || {
                for line in screen.view().logical_lines() {
                    let _ = black_box(line.reflow(1));
                    let last = line.range().end - 1;
                    let end = screen.view().row(last).unwrap().cells().len();
                    let point = CellPosition {
                        row: last,
                        column: end,
                    };
                    black_box(
                        line.map_position(columns, point, ReflowAffinity::After)
                            .unwrap(),
                    );
                    black_box(
                        line.map_cursor(
                            columns,
                            Cursor {
                                row: last,
                                column: end - 1,
                                wrap_pending: true,
                            },
                        )
                        .unwrap(),
                    );
                    let anchor = line.anchor(point).unwrap();
                    for row in line.reflow(columns).unwrap() {
                        black_box(row.map_anchor(anchor).unwrap());
                        black_box((row.source_range(), row.used_columns(), row.wrap_end()));
                        write!(&mut output, "{}", row.text())?;
                    }
                }
                Ok::<_, fmt::Error>(())
            });
            result.unwrap();
            assert_eq!(stats.attempts, 0);
            assert_eq!(output, expected_lines);
        }
    }
}

#[test]
fn processor_retries_allocation_failures_from_owned_input_positions() {
    fn text(screen: &mut Screen, value: &str, style: StyleId) -> Result<(), StorageError> {
        if value.is_ascii() {
            screen.write_ascii(value.as_bytes(), style)
        } else {
            screen.write("水\u{301}", CellWidth::Double, style)
        }
    }
    let input = "AB\x1b[2;2H水\nZ".as_bytes();
    let mut expected_screen = Screen::new(8, 4).unwrap();
    let mut expected_processor = Processor::default();
    let (result, stats) = allocator::measure(None, || {
        expected_processor.advance(input, &mut expected_screen, &mut text)
    });
    assert_eq!(result.unwrap(), input.len());
    assert!(stats.attempts > 0);
    let expected = snapshot(expected_screen.grid());
    for split in [0, input.iter().position(|&byte| byte == 0xb0).unwrap() + 1] {
        for point in 0..stats.attempts {
            let mut screen = Screen::new(8, 4).unwrap();
            let mut processor = Processor::default();
            processor
                .advance(&input[..split], &mut screen, &mut text)
                .unwrap();
            let (result, failed) = allocator::measure(Some(point), || {
                processor.advance(&input[split..], &mut screen, &mut text)
            });
            let error = result.unwrap_err();
            assert!(matches!(
                error.error,
                ProcessFailure::Text(StorageError::Allocation(_))
            ));
            assert_eq!(failed.failures, 1);
            let suffix = &input[split + error.consumed..];
            let checkpoint = snapshot(screen.grid());
            let cursor = screen.cursor();
            let (result, _) =
                allocator::measure(Some(0), || processor.advance(&[], &mut screen, &mut text));
            let again = result.unwrap_err();
            assert_eq!(again.consumed, 0);
            assert_eq!(snapshot(screen.grid()), checkpoint);
            assert_eq!(screen.cursor(), cursor);
            assert_eq!(
                processor.advance(suffix, &mut screen, &mut text).unwrap(),
                suffix.len()
            );
            processor.finish(&mut screen, &mut text).unwrap();
            assert_eq!(snapshot(screen.grid()), expected);
            assert_eq!(screen.cursor(), expected_screen.cursor());
        }
    }
    let mut processor = Processor::default();
    let mut screen = Screen::new(8, 4).unwrap();
    let (result, stats) = allocator::measure(Some(0), || {
        for _ in 0..32 {
            processor.advance(
                b"abc\r\n\x1b[2;3H!\x1b[0K",
                &mut screen,
                &mut |s: &mut Screen, value: &str, style: StyleId| {
                    s.write_ascii(value.as_bytes(), style)
                },
            )?;
        }
        Ok::<_, lune_terminal::ProcessError<StorageError>>(())
    });
    result.unwrap();
    assert_eq!(stats.attempts, 0);
}

#[test]
fn sgr_interpretation_preserves_zero_allocation_for_accepted_and_rejected_groups() {
    let mut parameters = [lune_vt::Parameter::default(); 32];
    let mut length = 0;
    lune_vt::Parser::default().advance(b"\x1b[1;38;2;0;128;255;48:5:255;21m", |event| {
        if let lune_vt::Event::Csi(sequence) = event {
            length = sequence.parameters.len();
            parameters[..length].copy_from_slice(sequence.parameters);
        }
    });
    let mut style = Style::default();
    let ((), stats) = allocator::measure(Some(0), || {
        for _ in 0..1024 {
            assert!(style.apply_sgr(&parameters[..length]));
            let before = style;
            assert!(!style.apply_sgr(&parameters[..5]));
            assert_eq!(style, before);
            assert!(style.apply_sgr(&[]));
        }
    });
    assert_eq!(stats.attempts, 0);
}

#[test]
fn style_table_failures_preserve_ids_and_collection_and_hits_do_not_allocate() {
    fn value(index: u8) -> Style {
        Style {
            foreground: lune_terminal::Color::Indexed(index),
            ..Style::default()
        }
    }
    fn populated(count: u8) -> (StyleTable, Vec<StyleId>) {
        let mut table = StyleTable::new(128).unwrap();
        let ids = (0..count)
            .map(|n| table.intern(value(n)).unwrap())
            .collect();
        (table, ids)
    }
    for count in 0..=32 {
        let (mut table, _) = populated(count);
        let (result, stats) = allocator::measure(None, || table.intern(value(127)));
        result.unwrap();
        for point in 0..stats.attempts {
            let (mut table, ids) = populated(count);
            let (result, failed) = allocator::measure(Some(point), || table.intern(value(127)));
            assert!(matches!(result, Err(StyleError::Allocation(_))));
            assert_eq!(failed.failures, 1);
            assert_eq!(table.style_count(), count as usize + 1);
            for (n, id) in ids.iter().copied().enumerate() {
                assert_eq!(table.get(id), Some(&value(n as u8)));
            }
            let new = table.intern(value(127)).unwrap();
            assert_eq!(table.get(new), Some(&value(127)));
            assert_eq!(table.style_count(), count as usize + 2);
        }
    }
    let (created, stats) = allocator::measure(Some(0), || StyleTable::new(64));
    let mut table = created.unwrap();
    assert_eq!(stats.attempts, 0);
    let ids: Vec<_> = (0..32).map(|n| table.intern(value(n)).unwrap()).collect();
    let ((), stats) = allocator::measure(Some(0), || {
        for (n, &id) in ids.iter().enumerate() {
            assert_eq!(table.intern(value(n as u8)).unwrap(), id);
            assert_eq!(table.get(id), Some(&value(n as u8)));
        }
        assert!(matches!(
            table.collect([ids[0], StyleId(u32::MAX)]),
            Err(StyleError::InvalidId(_))
        ));
        assert_eq!(table.collect(ids.iter().copied().skip(16)).unwrap(), 16);
        assert_eq!(table.collect([]).unwrap(), 16);
        assert_eq!(table.style_count(), 1);
    });
    assert_eq!(stats.attempts, 0);
}

#[test]
fn processor_retries_style_and_text_allocations_without_replaying_accepted_input() {
    fn text(screen: &mut Screen, value: &str, style: StyleId) -> Result<(), StorageError> {
        if value.is_ascii() {
            screen.write_ascii(value.as_bytes(), style)
        } else {
            screen.write("水\u{301}", CellWidth::Double, style)
        }
    }
    let input = "A\x1b[1;31mB\x1b[48:2::0:128:255m水\nZ".as_bytes();
    let utf8 = input.iter().position(|&byte| byte == 0xe6).unwrap();
    let mut style_failures = 0;
    let mut text_failures = 0;
    for split in [0, 4, utf8 + 2] {
        let mut expected_screen = Screen::new(8, 4).unwrap();
        let mut expected_processor = Processor::default();
        expected_processor
            .advance(&input[..split], &mut expected_screen, &mut text)
            .unwrap();
        let (result, stats) = allocator::measure(None, || {
            expected_processor.advance(&input[split..], &mut expected_screen, &mut text)
        });
        result.unwrap();
        let expected = snapshot(expected_screen.grid());
        for point in 0..stats.attempts {
            let mut screen = Screen::new(8, 4).unwrap();
            let mut processor = Processor::default();
            processor
                .advance(&input[..split], &mut screen, &mut text)
                .unwrap();
            let (result, failed) = allocator::measure(Some(point), || {
                processor.advance(&input[split..], &mut screen, &mut text)
            });
            let error = result.unwrap_err();
            match error.error {
                ProcessFailure::Style(StyleError::Allocation(_)) => style_failures += 1,
                ProcessFailure::Text(StorageError::Allocation(_)) => text_failures += 1,
                error => panic!("Unexpected failure: {error}"),
            }
            assert_eq!(failed.failures, 1);
            let checkpoint = snapshot(screen.grid());
            let style = processor.style_id();
            let cursor = screen.cursor();
            let (result, _) =
                allocator::measure(Some(0), || processor.advance(&[], &mut screen, &mut text));
            assert_eq!(result.unwrap_err().consumed, 0);
            assert_eq!(snapshot(screen.grid()), checkpoint);
            assert_eq!(processor.style_id(), style);
            assert_eq!(screen.cursor(), cursor);
            let suffix = &input[split + error.consumed..];
            processor.advance(suffix, &mut screen, &mut text).unwrap();
            processor.finish(&mut screen, &mut text).unwrap();
            assert_eq!(snapshot(screen.grid()), expected);
            assert_eq!(screen.cursor(), expected_screen.cursor());
            assert_eq!(
                processor.styles().get(processor.style_id()),
                expected_processor
                    .styles()
                    .get(expected_processor.style_id())
            );
        }
    }
    assert!(style_failures > 0 && text_failures > 0);
    let mut screen = Screen::new(8, 4).unwrap();
    let (processor, stats) = allocator::measure(Some(0), Processor::default);
    assert_eq!(stats.attempts, 0);
    let mut processor = processor;
    let warm =
        b"\x1b[1;31mabc\x1b[0m\x1b[K\r\n\x1b7\x1b[34m\x1b8\x1b[?12;25l\x1b[6 q\x1b[?12;25h\x1b[ q";
    processor.advance(warm, &mut screen, &mut text).unwrap();
    let ((), stats) = allocator::measure(Some(0), || {
        for _ in 0..32 {
            processor.advance(warm, &mut screen, &mut text).unwrap();
            processor
                .advance(
                    b"\x1b[?1049hALT\x1b[?1048h\x1b[?47l\x1b[?47h\x1b[?1048l\x1b[?1047l",
                    &mut screen,
                    &mut text,
                )
                .unwrap();
            processor
                .advance(
                    b"\x1b(0lqqk\r\nx  x\r\nmqqj\x1b(B",
                    &mut screen,
                    &mut |s: &mut Screen, value: &str, style: StyleId| {
                        if value.is_ascii() {
                            s.write_ascii(value.as_bytes(), style)
                        } else {
                            s.write(value, CellWidth::Single, style)
                        }
                    },
                )
                .unwrap();
            processor.collect_styles(&screen, []).unwrap();
        }
        processor.reset(&mut screen, &mut text);
        processor.collect_styles(&screen, []).unwrap();
        for _ in 0..32 {
            processor.advance(b"x", &mut screen, &mut text).unwrap();
            let error = processor
                .advance(b"\x1b[5n\x1b[6n", &mut screen, &mut text)
                .unwrap_err();
            assert!(matches!(error.error, ProcessFailure::ReplyPending));
            assert_eq!(error.consumed, 8);
            while !processor.reply_bytes().is_empty() {
                black_box(processor.reply_bytes());
                assert!(processor.consume_reply(1));
            }
            processor.advance(&[], &mut screen, &mut text).unwrap();
            while !processor.reply_bytes().is_empty() {
                black_box(processor.reply_bytes());
                assert!(processor.consume_reply(1));
            }
        }
        let mut queries = &b"\x1b[4$p\x1b[20$p\x1b[?7$p\x1b[?1004$p\x1b[?1003$p\x1b[?65535$p"[..];
        loop {
            let result = processor.advance(queries, &mut screen, &mut text);
            while !processor.reply_bytes().is_empty() {
                black_box(processor.reply_bytes());
                assert!(processor.consume_reply(1));
            }
            match result {
                Ok(_) => break,
                Err(error) => {
                    assert!(matches!(error.error, ProcessFailure::ReplyPending));
                    queries = &queries[error.consumed..];
                }
            }
        }
        processor
            .advance(
                b"\x1b[?1;66;67;1003;1004;1006;2004h\x1b[20h",
                &mut screen,
                &mut text,
            )
            .unwrap();
        let modes = processor.input_modes();
        for mouse_tracking in [
            lune_terminal::MouseTracking::Disabled,
            lune_terminal::MouseTracking::Buttons,
            lune_terminal::MouseTracking::ButtonMotion,
            lune_terminal::MouseTracking::AnyMotion,
        ] {
            for sgr_mouse in [false, true] {
                let modes = lune_terminal::InputModes {
                    mouse_tracking,
                    sgr_mouse,
                    ..modes
                };
                for event in [
                    lune_terminal::MouseEvent::Press(lune_terminal::MouseButton::Left),
                    lune_terminal::MouseEvent::Release(lune_terminal::MouseButton::Right),
                    lune_terminal::MouseEvent::Motion(Some(lune_terminal::MouseButton::Middle)),
                    lune_terminal::MouseEvent::Motion(None),
                    lune_terminal::MouseEvent::WheelUp,
                    lune_terminal::MouseEvent::WheelDown,
                ] {
                    for position in [
                        CellPosition { row: 0, column: 0 },
                        CellPosition {
                            row: 222,
                            column: 222,
                        },
                        CellPosition {
                            row: 65534,
                            column: 65534,
                        },
                        CellPosition {
                            row: usize::MAX,
                            column: usize::MAX,
                        },
                    ] {
                        black_box(modes.encode_mouse(
                            black_box(event),
                            position,
                            Modifiers {
                                shift: true,
                                alt: true,
                                control: true,
                            },
                        ));
                    }
                }
            }
        }
        for enabled in [false, true] {
            for focused in [false, true] {
                black_box(
                    lune_terminal::InputModes {
                        focus_reporting: enabled,
                        ..modes
                    }
                    .encode_focus(black_box(focused)),
                );
            }
        }
        for bracketed_paste in [false, true] {
            let modes = lune_terminal::InputModes {
                bracketed_paste,
                ..modes
            };
            for payload in ["", "line\nnext", "水e\u{301}"] {
                let mut paste = modes.encode_paste(payload);
                while !paste.is_empty() {
                    black_box(paste.bytes());
                    assert!(paste.consume(paste.bytes().len().min(2)));
                }
            }
        }
        for flags in 0..8 {
            let modifiers = Modifiers {
                shift: flags & 1 != 0,
                alt: flags & 2 != 0,
                control: flags & 4 != 0,
            };
            for character in (0..=127).map(char::from).chain([
                '\u{80}',
                '\u{7ff}',
                '\u{800}',
                '水',
                '🦀',
                '\u{10ffff}',
            ]) {
                black_box(lune_terminal::KeySequence::from_character(
                    black_box(character),
                    modifiers,
                ));
            }
            for digit in 0..=255 {
                black_box(modes.encode_key(lune_terminal::Key::KeypadDigit(digit), modifiers));
                black_box(modes.encode_key(lune_terminal::Key::Function(digit), modifiers));
            }
            for key in [
                lune_terminal::Key::Up,
                lune_terminal::Key::Enter,
                lune_terminal::Key::KeypadEnter,
                lune_terminal::Key::Home,
                lune_terminal::Key::End,
                lune_terminal::Key::Insert,
                lune_terminal::Key::Delete,
                lune_terminal::Key::PageUp,
                lune_terminal::Key::PageDown,
                lune_terminal::Key::Tab,
                lune_terminal::Key::Escape,
                lune_terminal::Key::Backspace,
                lune_terminal::Key::KeypadMultiply,
                lune_terminal::Key::KeypadAdd,
                lune_terminal::Key::KeypadDivide,
                lune_terminal::Key::KeypadEqual,
            ] {
                black_box(modes.encode_key(key, modifiers));
            }
        }
    });
    assert_eq!(stats.attempts, 0);
}
