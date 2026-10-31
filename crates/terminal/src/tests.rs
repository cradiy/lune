use std::ops::Range;

use crate::{CellText, CellWidth, Grid, MAX_CELL_TEXT_BYTES, MAX_CELLS, StorageError, StyleId};

fn assert_invariants(grid: &Grid) {
    for row in grid.iter_rows() {
        assert_eq!(row.cells().len(), grid.columns());
        for (column, cell) in row.cells().iter().enumerate() {
            match cell.width() {
                0 => {
                    assert!(column > 0);
                    assert_eq!(row.cells()[column - 1].width(), 2);
                    assert_eq!(row.cells()[column - 1].style(), cell.style());
                    assert!(!cell.has_extended_text());
                    assert_eq!(row.text(column), Some(CellText::Continuation));
                }
                1 | 2 => {
                    if cell.width() == 2 {
                        assert!(column + 1 < grid.columns());
                        assert!(row.cells()[column + 1].is_continuation());
                    }
                    match row.text(column).unwrap() {
                        CellText::Scalar(character) => {
                            assert_eq!(cell.character(), Some(character));
                            assert!(!cell.has_extended_text());
                        }
                        CellText::Extended(text) => {
                            assert!(cell.has_extended_text());
                            assert_eq!(text.chars().next(), cell.character());
                            assert!(text.len() <= MAX_CELL_TEXT_BYTES);
                            assert!(text.chars().count() > 1);
                        }
                        CellText::Continuation => panic!("Leading cell has continuation text"),
                    }
                }
                _ => panic!("Invalid cell width"),
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SnapshotCell {
    text: String,
    width: usize,
    style: StyleId,
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    columns: usize,
    rows: Vec<Vec<SnapshotCell>>,
}

fn snapshot(grid: &Grid) -> Snapshot {
    let rows = grid
        .iter_rows()
        .map(|row| {
            row.cells()
                .iter()
                .enumerate()
                .map(|(column, cell)| {
                    let text = match row.text(column).unwrap() {
                        CellText::Scalar(character) => character.to_string(),
                        CellText::Extended(text) => text.to_owned(),
                        CellText::Continuation => String::new(),
                    };
                    SnapshotCell {
                        text,
                        width: cell.width(),
                        style: cell.style(),
                    }
                })
                .collect()
        })
        .collect();
    Snapshot {
        columns: grid.columns(),
        rows,
    }
}

#[test]
fn overlapping_wide_units_are_replaced_as_complete_units() {
    let mut grid = Grid::new(4, 1).unwrap();
    grid.write(0, 0, "界\u{301}", CellWidth::Double, StyleId(1))
        .unwrap();
    grid.write(0, 2, "水", CellWidth::Double, StyleId(2))
        .unwrap();
    grid.write(0, 1, "🌙", CellWidth::Double, StyleId(3))
        .unwrap();
    assert_invariants(&grid);
    let row = grid.row(0).unwrap();
    assert_eq!(row.text(0), Some(CellText::Scalar(' ')));
    assert_eq!(row.text(1), Some(CellText::Scalar('🌙')));
    assert_eq!(row.text(2), Some(CellText::Continuation));
    assert_eq!(row.text(3), Some(CellText::Scalar(' ')));
    assert!(row.cells().iter().all(|cell| cell.style() == StyleId(3)));

    grid.append_text(0, 2, "\u{fe0f}").unwrap();
    assert_eq!(
        grid.row(0).unwrap().text(1),
        Some(CellText::Extended("🌙\u{fe0f}"))
    );
    grid.clear_row(0, 2..3, StyleId(4)).unwrap();
    assert_invariants(&grid);
    assert_eq!(grid.row(0).unwrap().text(1), Some(CellText::Scalar(' ')));
    assert_eq!(grid.row(0).unwrap().text(2), Some(CellText::Scalar(' ')));

    grid.write(0, 1, "e\u{301}", CellWidth::Double, StyleId(5))
        .unwrap();
    grid.write_ascii(0, 2, b"xy", StyleId(6)).unwrap();
    assert_invariants(&grid);
    assert_eq!(grid.row(0).unwrap().text(1), Some(CellText::Scalar(' ')));
    assert_eq!(grid.row(0).unwrap().text(2), Some(CellText::Scalar('x')));

    let mut grid = Grid::new(4, 1).unwrap();
    grid.write(0, 0, "e\u{301}", CellWidth::Single, StyleId(1))
        .unwrap();
    grid.write(0, 1, "水\u{301}", CellWidth::Double, StyleId(2))
        .unwrap();
    let buffer = match grid.row(0).unwrap().text(0).unwrap() {
        CellText::Extended(text) => text.as_ptr(),
        _ => unreachable!(),
    };
    grid.write(0, 0, "o\u{302}", CellWidth::Double, StyleId(3))
        .unwrap();
    assert_invariants(&grid);
    match grid.row(0).unwrap().text(0).unwrap() {
        CellText::Extended(text) => {
            assert_eq!(text, "o\u{302}");
            assert_eq!(text.as_ptr(), buffer);
        }
        _ => panic!("Extended text was lost"),
    }
    assert_eq!(grid.row(0).unwrap().text(2), Some(CellText::Scalar(' ')));
}

#[test]
fn resizing_preserves_the_intersection_and_repairs_cut_wide_units() {
    let mut grid = Grid::new(5, 3).unwrap();
    grid.write_ascii(0, 0, b"abcde", StyleId(1)).unwrap();
    grid.write(1, 1, "e\u{301}", CellWidth::Single, StyleId(2))
        .unwrap();
    grid.write(1, 3, "界\u{301}", CellWidth::Double, StyleId(3))
        .unwrap();
    grid.write_ascii(2, 0, b"third", StyleId(4)).unwrap();
    grid.scroll_up(0..grid.rows(), 1, StyleId(0)).unwrap();
    grid.resize(4, 2, StyleId(9)).unwrap();
    assert_invariants(&grid);
    assert_eq!(
        grid.row(0).unwrap().text(1),
        Some(CellText::Extended("e\u{301}"))
    );
    assert_eq!(grid.row(0).unwrap().text(3), Some(CellText::Scalar(' ')));
    assert_eq!(grid.row(0).unwrap().cells()[3].style(), StyleId(9));
    assert_eq!(grid.row(1).unwrap().text(3), Some(CellText::Scalar('r')));

    grid.resize(7, 4, StyleId(11)).unwrap();
    assert_invariants(&grid);
    assert_eq!(grid.row(0).unwrap().cells()[3].style(), StyleId(9));
    assert_eq!(grid.row(0).unwrap().cells()[4].style(), StyleId(11));
    assert!(
        grid.row(3)
            .unwrap()
            .cells()
            .iter()
            .all(|cell| cell.character() == Some(' ') && cell.style() == StyleId(11))
    );
}

#[test]
fn scrolling_preserves_region_boundaries_and_owned_buffers() {
    let height = 9;
    for offset in 0..height {
        for start in 0..=height {
            for end in start..=height {
                for lines in (0..=height).chain([usize::MAX]) {
                    for upward in [true, false] {
                        let mut grid = Grid::new(4, height).unwrap();
                        grid.scroll_up(0..height, offset, StyleId(0)).unwrap();
                        for row in 0..height {
                            grid.write_ascii(row, 0, &[b'a' + row as u8], StyleId(row as u32))
                                .unwrap();
                            grid.write(row, 1, "水\u{301}", CellWidth::Double, StyleId(2))
                                .unwrap();
                        }
                        let before = snapshot(&grid);
                        let buffers: Vec<_> = grid
                            .iter_rows()
                            .map(|row| {
                                let Some(CellText::Extended(text)) = row.text(1) else {
                                    panic!("Extended text is present");
                                };
                                (row.cells().as_ptr(), text.as_ptr())
                            })
                            .collect();
                        let style = StyleId(9);
                        if upward {
                            grid.scroll_up(start..end, lines, style).unwrap();
                        } else {
                            grid.scroll_down(start..end, lines, style).unwrap();
                        }
                        let after = snapshot(&grid);
                        for row in 0..height {
                            let inside = (start..end).contains(&row);
                            let count = lines.min(end - start);
                            let source = if !inside {
                                Some(row)
                            } else if upward {
                                (row + count < end).then_some(row + count)
                            } else {
                                row.checked_sub(count).filter(|&source| source >= start)
                            };
                            let buffer_row = if !inside || count == end - start {
                                row
                            } else if upward {
                                start + (row - start + count) % (end - start)
                            } else {
                                start + (row - start + end - start - count) % (end - start)
                            };
                            assert_eq!(
                                grid.row(row).unwrap().cells().as_ptr(),
                                buffers[buffer_row].0
                            );
                            if let Some(source) = source {
                                assert_eq!(after.rows[row], before.rows[source]);
                                let Some(CellText::Extended(text)) = grid.row(row).unwrap().text(1)
                                else {
                                    panic!("Moved extended text is present");
                                };
                                assert_eq!(text.as_ptr(), buffers[source].1);
                            } else {
                                assert!(after.rows[row].iter().all(|cell| {
                                    cell.text == " " && cell.width == 1 && cell.style == style
                                }));
                            }
                        }
                        assert_invariants(&grid);
                    }
                }
            }
        }
    }
}

#[test]
fn rejected_mutations_preserve_visible_contents() {
    let mut grid = Grid::new(4, 2).unwrap();
    grid.write(0, 0, "e\u{301}", CellWidth::Single, StyleId(u32::MAX))
        .unwrap();
    let expected = snapshot(&grid);
    assert!(matches!(
        grid.write(0, 3, "水", CellWidth::Double, StyleId(1)),
        Err(StorageError::OutOfBounds)
    ));
    assert!(matches!(
        grid.write(0, 0, "", CellWidth::Single, StyleId(1)),
        Err(StorageError::EmptyText)
    ));
    assert!(matches!(
        grid.write_ascii(0, 0, b"ab\nc", StyleId(1)),
        Err(StorageError::InvalidAscii)
    ));
    assert!(
        grid.clear_row(0, Range { start: 3, end: 1 }, StyleId(1))
            .is_err()
    );
    for region in [Range { start: 2, end: 1 }, 0..3, 3..3, 0..usize::MAX] {
        for lines in [0, 1, usize::MAX] {
            assert!(matches!(
                grid.scroll_up(region.clone(), lines, StyleId(1)),
                Err(StorageError::OutOfBounds)
            ));
            assert!(matches!(
                grid.scroll_down(region.clone(), lines, StyleId(1)),
                Err(StorageError::OutOfBounds)
            ));
        }
    }
    assert!(grid.resize(0, 2, StyleId(1)).is_err());
    assert!(grid.resize(2048, MAX_CELLS / 2048 + 1, StyleId(1)).is_err());
    let large = "x".repeat(MAX_CELL_TEXT_BYTES);
    assert!(matches!(
        grid.append_text(0, 0, &large),
        Err(StorageError::TextTooLong)
    ));
    assert!(matches!(
        grid.write(0, 0, &(large + "x"), CellWidth::Single, StyleId(1)),
        Err(StorageError::TextTooLong)
    ));
    assert_eq!(snapshot(&grid), expected);
    assert_invariants(&grid);
}

#[test]
fn mixed_mutations_preserve_occupancy_and_failure_atomicity() {
    let mut grid = Grid::new(8, 4).unwrap();
    let mut seed = 1_u64;
    for _ in 0..10000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let bits = seed >> 32;
        let row = bits as usize % grid.rows();
        let column = (bits >> 8) as usize % grid.columns();
        let style = StyleId((bits >> 16) as u32);
        let before = snapshot(&grid);
        let result = match bits % 8 {
            0 => grid.write(row, column, "\u{10ffff}", CellWidth::Single, style),
            1 => grid.write(row, column, "👩\u{200d}💻", CellWidth::Double, style),
            2 => grid.append_text(row, column, "\u{301}"),
            3 => grid.clear_row(row, column..column + 1, style),
            4 => grid.write_ascii(row, column, b"ab", style),
            5 => grid.resize(
                (bits >> 12) as usize % 16 + 1,
                (bits >> 20) as usize % 8 + 1,
                style,
            ),
            6 => grid.scroll_up(
                row..(bits >> 20) as usize % (grid.rows() + 2),
                (bits >> 12) as usize % 10,
                style,
            ),
            _ => grid.scroll_down(
                row..(bits >> 20) as usize % (grid.rows() + 2),
                (bits >> 12) as usize % 10,
                style,
            ),
        };
        if result.is_err() {
            assert_eq!(snapshot(&grid), before);
        }
        assert_invariants(&grid);
    }
}
