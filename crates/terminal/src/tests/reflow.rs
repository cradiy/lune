use crate::{
    CellPosition, CellText, CellWidth, Cursor, HistoryLimits, MAX_DIMENSION, ReflowAffinity,
    Screen, ScreenBuffer, StorageError, StyleId,
};

#[test]
fn reflow_partitions_complete_units_across_widths_and_source_wrap_gaps() {
    for source_columns in 2..8 {
        for pattern in 0..256 {
            let mut screen = Screen::new(source_columns, 2).unwrap();
            screen
                .set_history_limits(HistoryLimits {
                    max_rows: 16,
                    max_bytes: 1 << 20,
                })
                .unwrap();
            for index in 0..8 {
                let wide = pattern & (1 << index) != 0;
                screen
                    .write(
                        if wide { "水\u{301}" } else { "x" },
                        if wide {
                            CellWidth::Double
                        } else {
                            CellWidth::Single
                        },
                        StyleId(index),
                    )
                    .unwrap();
            }
            let view = screen.view();
            for line in view.logical_lines() {
                let mut units = Vec::new();
                for row_index in line.range() {
                    let row = view.row(row_index).unwrap();
                    for (column, cell) in row
                        .cells()
                        .iter()
                        .enumerate()
                        .take(row.wrap_end().unwrap_or(row.cells().len()))
                    {
                        if cell.width() != 0 {
                            let text = match row.text(column).unwrap() {
                                CellText::Scalar(character) => character.to_string(),
                                CellText::Extended(text) => text.to_owned(),
                                CellText::Continuation => unreachable!(),
                            };
                            units.push((
                                CellPosition {
                                    row: row_index,
                                    column,
                                },
                                cell.width(),
                                text,
                            ));
                        }
                    }
                }
                let end_row = line.range().end - 1;
                let end = CellPosition {
                    row: end_row,
                    column: view.row(end_row).unwrap().cells().len(),
                };
                for columns in 1..10 {
                    if columns == 1 && units.iter().any(|unit| unit.1 == 2) {
                        assert!(matches!(
                            line.reflow(columns),
                            Err(StorageError::OutOfBounds)
                        ));
                        continue;
                    }
                    // Pack an owned, flat unit list independently of physical row edges.
                    let mut groups: Vec<Vec<usize>> = vec![Vec::new()];
                    let mut used = 0;
                    for (index, unit) in units.iter().enumerate() {
                        if used + unit.1 > columns {
                            groups.push(Vec::new());
                            used = 0;
                        }
                        groups.last_mut().unwrap().push(index);
                        used += unit.1;
                    }
                    let mut rows = line.reflow(columns).unwrap();
                    let mut joined = String::new();
                    for (index, group) in groups.iter().enumerate() {
                        let row = rows.next().unwrap();
                        let first = *group.first().unwrap();
                        let last = *group.last().unwrap();
                        let next = units.get(last + 1).map_or(end, |unit| unit.0);
                        let count = group.iter().map(|&i| units[i].1).sum::<usize>();
                        let expected: String = group.iter().map(|&i| units[i].2.as_str()).collect();
                        assert_eq!(row.source_range(), units[first].0..next);
                        assert_eq!(row.used_columns(), count);
                        assert_eq!(row.wrap_end(), (index + 1 < groups.len()).then_some(count));
                        assert_eq!(row.text().to_string(), expected);
                        let owned = row.materialize(StyleId(u32::MAX)).unwrap();
                        assert_eq!(owned.cells().len(), columns);
                        assert_eq!(owned.wrap_end(), row.wrap_end());
                        let mut column = 0;
                        for &i in group {
                            let (position, width, _) = &units[i];
                            let source = view.row(position.row).unwrap();
                            assert_eq!(
                                &owned.cells()[column..column + width],
                                &source.cells()[position.column..position.column + width]
                            );
                            assert_eq!(owned.text(column), source.text(position.column));
                            column += width;
                        }
                        for cell in &owned.cells()[count..] {
                            assert!(cell.is_erased());
                            assert_eq!(cell.width(), 1);
                            assert_eq!(cell.style(), StyleId(u32::MAX));
                        }
                        for (column, cell) in owned.cells().iter().enumerate() {
                            let gap = row.wrap_end().is_some_and(|end| end < columns);
                            assert_eq!(
                                cell.row_wrap(),
                                (column + 1 == columns)
                                    .then_some(gap)
                                    .filter(|_| row.wrap_end().is_some())
                            );
                            assert_eq!(cell.is_row_gap(), gap && column + 2 >= columns);
                        }
                        joined.push_str(&expected);
                    }
                    assert!(rows.next().is_none());
                    assert_eq!(rows.size_hint(), (0, Some(0)));
                    assert!(rows.next().is_none());
                    assert_eq!(joined, line.text().to_string());
                    let mut source_base = 0;
                    for source_row in line.range() {
                        let source = view.row(source_row).unwrap();
                        let source_end = source.wrap_end().unwrap_or(source.cells().len());
                        for column in 0..=source.cells().len() {
                            let point = CellPosition {
                                row: source_row,
                                column,
                            };
                            let offset = source_base + column.min(source_end);
                            let mut base = 0;
                            let mut expected = Vec::new();
                            for (index, group) in groups.iter().enumerate() {
                                let count = group.iter().map(|&i| units[i].1).sum::<usize>();
                                if (base..=base + count).contains(&offset) {
                                    expected.push(CellPosition {
                                        row: index,
                                        column: offset - base,
                                    });
                                }
                                base += count;
                            }
                            for (affinity, destination) in [
                                (ReflowAffinity::Before, *expected.first().unwrap()),
                                (ReflowAffinity::After, *expected.last().unwrap()),
                            ] {
                                assert_eq!(
                                    line.map_position(columns, point, affinity).unwrap(),
                                    destination
                                );
                            }
                            let anchor = line.anchor(point).unwrap();
                            let actual = line
                                .reflow(columns)
                                .unwrap()
                                .enumerate()
                                .filter_map(|(index, row)| {
                                    row.map_anchor(anchor)
                                        .unwrap()
                                        .map(|column| CellPosition { row: index, column })
                                })
                                .collect::<Vec<_>>();
                            assert_eq!(actual, expected);
                        }
                        source_base += source_end;
                    }
                }
            }
        }
    }
}

#[test]
fn reflow_maps_cursor_pending_wraps_continuations_gaps_and_rejects_invalid_positions() {
    let mut screen = Screen::new(4, 3).unwrap();
    screen.write_ascii(b"abc", StyleId(0)).unwrap();
    screen.write("水", CellWidth::Double, StyleId(1)).unwrap();
    screen.write_ascii(b"xyZ", StyleId(0)).unwrap();
    let line = screen.view().logical_line(0).unwrap();
    for (columns, source, expected) in [
        (
            3,
            Cursor {
                row: 0,
                column: 3,
                wrap_pending: false,
            },
            Cursor {
                row: 1,
                column: 0,
                wrap_pending: false,
            },
        ),
        (
            3,
            Cursor {
                row: 1,
                column: 1,
                wrap_pending: false,
            },
            Cursor {
                row: 1,
                column: 1,
                wrap_pending: false,
            },
        ),
        (
            3,
            Cursor {
                row: 1,
                column: 3,
                wrap_pending: true,
            },
            Cursor {
                row: 2,
                column: 1,
                wrap_pending: false,
            },
        ),
        (
            7,
            Cursor {
                row: 1,
                column: 3,
                wrap_pending: true,
            },
            Cursor {
                row: 0,
                column: 6,
                wrap_pending: true,
            },
        ),
        (
            3,
            Cursor {
                row: 0,
                column: 3,
                wrap_pending: true,
            },
            Cursor {
                row: 0,
                column: 2,
                wrap_pending: true,
            },
        ),
    ] {
        assert_eq!(line.map_cursor(columns, source).unwrap(), expected);
    }
    let anchor = line.anchor(CellPosition { row: 1, column: 1 }).unwrap();
    for (columns, destination) in [
        (2, CellPosition { row: 2, column: 1 }),
        (3, CellPosition { row: 1, column: 1 }),
        (7, CellPosition { row: 0, column: 4 }),
    ] {
        let actual = line
            .reflow(columns)
            .unwrap()
            .enumerate()
            .filter_map(|(index, row)| {
                row.map_anchor(anchor)
                    .unwrap()
                    .map(|column| CellPosition { row: index, column })
            })
            .last()
            .unwrap();
        assert_eq!(actual, destination);
    }
    for source in [
        CellPosition {
            row: usize::MAX,
            column: 0,
        },
        CellPosition {
            row: 0,
            column: usize::MAX,
        },
    ] {
        assert!(matches!(
            line.map_position(3, source, ReflowAffinity::After),
            Err(StorageError::OutOfBounds)
        ));
    }
    for source in [
        Cursor {
            row: 0,
            column: 1,
            wrap_pending: true,
        },
        Cursor {
            row: 0,
            column: 4,
            wrap_pending: false,
        },
    ] {
        assert!(matches!(
            line.map_cursor(3, source),
            Err(StorageError::OutOfBounds)
        ));
    }
    for columns in [0, MAX_DIMENSION + 1, usize::MAX] {
        assert!(line.map_cursor(columns, Cursor::default()).is_err());
    }
    screen.set_cursor(2, 0);
    screen.line_feed(StyleId(0)).unwrap();
    let view = screen.view();
    let line = view.logical_line(0).unwrap();
    let outside = CellPosition {
        row: line.range().end,
        column: 0,
    };
    assert!(matches!(
        line.map_position(3, outside, ReflowAffinity::After),
        Err(StorageError::OutOfBounds)
    ));
    let row = line.reflow(3).unwrap().next().unwrap();
    let outside_anchor = view
        .logical_line(outside.row)
        .unwrap()
        .anchor(outside)
        .unwrap();
    assert_eq!(row.map_anchor(outside_anchor).unwrap(), None);
    let other = Screen::new(4, 3).unwrap();
    let other_anchor = other
        .view()
        .logical_line(0)
        .unwrap()
        .anchor(CellPosition { row: 0, column: 0 })
        .unwrap();
    assert!(matches!(
        row.map_anchor(other_anchor),
        Err(StorageError::OutOfBounds)
    ));
}

#[test]
fn reflow_retains_spaces_metadata_history_fragments_and_captured_widths() {
    let mut screen = Screen::new(5, 2).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 4,
            max_bytes: 1 << 20,
        })
        .unwrap();
    screen.write_ascii(b"a  b", StyleId(1)).unwrap();
    screen
        .write("👩\u{200d}💻", CellWidth::Double, StyleId(2))
        .unwrap();
    screen
        .write("e\u{301}", CellWidth::Single, StyleId(3))
        .unwrap();
    screen.write_ascii(b"xyZ", StyleId(4)).unwrap();
    screen
        .set_history_limits(HistoryLimits {
            max_rows: 1,
            max_bytes: 1 << 20,
        })
        .unwrap();
    let check = |screen: &Screen| {
        let view = screen.view();
        for line in view.logical_lines() {
            let original = line.text().to_string();
            for columns in [2, 3, 8, MAX_DIMENSION] {
                let reflowed: String = line
                    .reflow(columns)
                    .unwrap()
                    .map(|row| row.text().to_string())
                    .collect();
                assert_eq!(reflowed, original);
            }
            for (columns, capacity) in [(0, false), (MAX_DIMENSION + 1, true), (usize::MAX, true)] {
                match (line.reflow(columns), capacity) {
                    (Err(StorageError::InvalidDimensions), false)
                    | (Err(StorageError::CapacityExceeded), true) => {}
                    other => panic!("Unexpected width validation: {other:?}"),
                }
            }
        }
    };
    check(&screen);
    let rows = screen
        .view()
        .logical_line(0)
        .unwrap()
        .reflow(3)
        .unwrap()
        .collect::<Vec<_>>();
    assert_eq!(
        rows.iter()
            .map(|row| row.text().to_string())
            .collect::<Vec<_>>(),
        ["a  ", "b👩\u{200d}💻", "e\u{301}xy", "Z  ", "  "]
    );
    assert_eq!(screen.view().row(0).unwrap().cells()[1].style(), StyleId(1));
    assert!(!screen.view().row(0).unwrap().cells()[1].is_erased());
    assert!(screen.grid().row(1).unwrap().cells()[4].is_erased());
    let retained = rows
        .iter()
        .map(|row| row.materialize(StyleId(9)).unwrap())
        .collect::<Vec<_>>();
    drop(rows);
    screen.resize(3, 2, StyleId(0)).unwrap();
    check(&screen);
    screen.write_ascii(b"1234567", StyleId(5)).unwrap();
    check(&screen);
    screen.select_buffer(ScreenBuffer::Alternate);
    screen.set_cursor(0, 0);
    screen.write_ascii(b"abcdefg", StyleId(6)).unwrap();
    check(&screen);
    screen.reset(StyleId(0));
    let line = screen.view().logical_line(0).unwrap();
    assert_eq!(line.reflow(1).unwrap().count(), 3);
    drop(screen);
    assert_eq!(
        retained[1].text(1),
        Some(CellText::Extended("👩\u{200d}💻"))
    );
    assert_eq!(retained[2].text(0), Some(CellText::Extended("e\u{301}")));
    assert_eq!(retained[4].cells()[2].style(), StyleId(9));
    assert!(retained[4].cells()[2].is_erased());
}
