use super::{SavedRow, row};
use lune_terminal::{
    CellPosition, Cursor, EraseMode, MAX_HISTORY_BYTES, Screen, ScreenBuffer, StorageError, StyleId,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Cell {
    text: String,
    width: usize,
    style: StyleId,
    erased: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    cells: Vec<Cell>,
    wrap: Option<usize>,
}

fn plain(source: SavedRow) -> Row {
    Row {
        cells: source
            .cells
            .iter()
            .enumerate()
            .map(|(index, cell)| Cell {
                text: source.text[index].clone(),
                width: cell.width(),
                style: cell.style(),
                erased: cell.is_erased(),
            })
            .collect(),
        wrap: source.wrap,
    }
}

fn blank(columns: usize) -> Row {
    Row {
        cells: vec![
            Cell {
                text: " ".into(),
                width: 1,
                style: StyleId(9),
                erased: true
            };
            columns
        ],
        wrap: None,
    }
}

fn cursor(point: CellPosition, columns: usize) -> Cursor {
    let pending = point.column == columns;
    Cursor {
        row: point.row,
        column: point.column - usize::from(pending),
        wrap_pending: pending,
    }
}

fn crop_cursor(mut point: Cursor, start: usize, height: usize) -> Cursor {
    if !(start..start + height).contains(&point.row) {
        point.wrap_pending = false;
    }
    point.row = point.row.saturating_sub(start).min(height - 1);
    point
}

fn setup_saved(screen: &mut Screen, buffer: ScreenBuffer, bytes: [u8; 4]) -> (Cursor, bool, bool) {
    screen.select_buffer(buffer);
    screen.set_origin_mode(bytes[0] & 1 != 0);
    screen.set_cursor(usize::from(bytes[1]), usize::from(bytes[2]));
    if bytes[3] & 1 != 0 {
        screen.set_auto_wrap(true);
        let current = screen.cursor();
        let top = if screen.origin_mode() {
            screen.scroll_region().start
        } else {
            0
        };
        screen.set_cursor(current.row - top, screen.grid().columns() - 1);
        screen.write_ascii(b"z", StyleId(3)).unwrap();
    }
    screen.save_cursor();
    (screen.cursor(), screen.origin_mode(), screen.auto_wrap())
}

pub fn check(screen: &mut Screen, bytes: [u8; 4]) {
    let primary_saved = setup_saved(screen, ScreenBuffer::Primary, bytes);
    screen.scroll_viewport_up(usize::from(bytes[2]));
    let offset = screen.viewport_offset();
    let alternate_saved = setup_saved(
        screen,
        ScreenBuffer::Alternate,
        [bytes[3], bytes[2], bytes[1], bytes[0]],
    );
    let active = if bytes[0] & 2 == 0 {
        ScreenBuffer::Primary
    } else {
        ScreenBuffer::Alternate
    };
    screen.select_buffer(active);
    screen.set_tab_stop();
    let live = screen.cursor();
    let modes = (
        screen.origin_mode(),
        screen.auto_wrap(),
        screen.insert_mode(),
        screen.cursor_appearance(),
    );
    let old_region = screen.scroll_region();
    let old_columns = screen.grid().columns();
    let old_height = screen.grid().rows();
    let tabs = (0..old_columns)
        .map(|column| screen.is_tab_stop(column).unwrap())
        .collect::<Vec<_>>();
    let sources = screen
        .history()
        .iter_rows()
        .chain(screen.buffer(ScreenBuffer::Primary).iter_rows())
        .map(|value| plain(row(value)))
        .collect::<Vec<_>>();
    let alternate = screen
        .buffer(ScreenBuffer::Alternate)
        .iter_rows()
        .map(|value| plain(row(value)))
        .collect::<Vec<_>>();
    let history_rows = screen.history().len();
    let limits = screen.history().limits();
    let columns = [1, 2, 3, 7, 63, 64, 65, 129][usize::from(bytes[1] % 8)];
    let height = usize::from(bytes[2] % 5) + 1;
    let pivot = if active == ScreenBuffer::Primary {
        live
    } else {
        primary_saved.0
    };
    let points = [
        Some(CellPosition {
            row: history_rows + pivot.row,
            column: pivot.column + usize::from(pivot.wrap_pending),
        }),
        Some(CellPosition {
            row: history_rows + primary_saved.0.row,
            column: primary_saved.0.column + usize::from(primary_saved.0.wrap_pending),
        }),
        (offset != 0).then_some(CellPosition {
            row: history_rows - offset,
            column: 0,
        }),
    ];
    let preceding = [pivot.wrap_pending, primary_saved.0.wrap_pending, false];
    let mut destinations = [None; 3];
    let mut output = Vec::new();
    let mut first = 0;
    let incompatible = columns == 1
        && sources
            .iter()
            .any(|row| row.cells.iter().any(|cell| cell.width == 2));
    let unchanged = columns == old_columns && height == old_height;
    if !incompatible && !unchanged {
        for last in 0..sources.len() {
            if sources[last].wrap.is_some() && last + 1 < sources.len() {
                continue;
            }
            let mut units = Vec::new();
            for row in &sources[first..=last] {
                let end = row.wrap.unwrap_or(row.cells.len());
                let mut column = 0;
                while column < end {
                    let width = row.cells[column].width;
                    units.push(row.cells[column..column + width].to_vec());
                    column += width;
                }
            }
            let mut groups: Vec<Vec<Cell>> = vec![Vec::new()];
            for unit in units {
                if groups.last().unwrap().len() + unit.len() > columns {
                    groups.push(Vec::new());
                }
                groups.last_mut().unwrap().extend(unit);
            }
            for (index, point) in points.iter().enumerate() {
                if let Some(point) = point
                    && (first..=last).contains(&point.row)
                {
                    let offset = sources[first..point.row]
                        .iter()
                        .map(|row| row.wrap.unwrap_or(row.cells.len()))
                        .sum::<usize>()
                        + point.column.min(
                            sources[point.row]
                                .wrap
                                .unwrap_or(sources[point.row].cells.len()),
                        );
                    let mut base = 0;
                    for (group, cells) in groups.iter().enumerate() {
                        if (base..=base + cells.len()).contains(&offset)
                            && (destinations[index].is_none() || !preceding[index])
                        {
                            destinations[index] = Some(CellPosition {
                                row: output.len() + group,
                                column: offset - base,
                            });
                        }
                        base += cells.len();
                    }
                }
            }
            let count = groups.len();
            for (index, cells) in groups.into_iter().enumerate() {
                let used = cells.len();
                let mut row = blank(columns);
                row.cells[..used].clone_from_slice(&cells);
                row.wrap = (index + 1 < count).then_some(used);
                output.push(row);
            }
            first = last + 1;
        }
    }
    let before_primary = screen
        .buffer(ScreenBuffer::Primary)
        .iter_rows()
        .map(|value| plain(row(value)))
        .collect::<Vec<_>>();
    let before_bytes = screen.history().retained_bytes();
    let result = screen.reflow(columns, height, StyleId(9));
    if incompatible && !unchanged {
        assert!(matches!(result, Err(StorageError::OutOfBounds)));
    } else {
        result.unwrap();
    }
    if incompatible || unchanged {
        assert_eq!(
            screen
                .buffer(ScreenBuffer::Primary)
                .iter_rows()
                .map(|value| plain(row(value)))
                .collect::<Vec<_>>(),
            before_primary
        );
        assert_eq!(
            screen
                .buffer(ScreenBuffer::Alternate)
                .iter_rows()
                .map(|value| plain(row(value)))
                .collect::<Vec<_>>(),
            alternate
        );
        assert_eq!(
            screen
                .history()
                .iter_rows()
                .map(|value| plain(row(value)))
                .collect::<Vec<_>>(),
            sources[..history_rows]
        );
        assert_eq!(screen.history().retained_bytes(), before_bytes);
        assert_eq!(screen.cursor(), live);
        assert_eq!(screen.scroll_region(), old_region);
        return;
    }
    let pivot = cursor(destinations[0].unwrap(), columns);
    let start = output.len().saturating_sub(height).min(pivot.row);
    let mut visible = output[start..(start + height).min(output.len())].to_vec();
    visible.resize(height, blank(columns));
    visible.last_mut().unwrap().wrap = None;
    assert_eq!(
        screen
            .buffer(ScreenBuffer::Primary)
            .iter_rows()
            .map(|value| plain(row(value)))
            .collect::<Vec<_>>(),
        visible
    );
    let max_history = if limits.max_bytes == 0 {
        0
    } else {
        assert_eq!(limits.max_bytes, MAX_HISTORY_BYTES);
        limits.max_rows
    };
    let retained_start = start.saturating_sub(max_history);
    assert_eq!(
        screen
            .history()
            .iter_rows()
            .map(|value| plain(row(value)))
            .collect::<Vec<_>>(),
        output[retained_start..start]
    );
    assert!(screen.history().retained_bytes() <= limits.max_bytes);
    let mut cropped = vec![blank(columns); height];
    for (index, source) in alternate.iter().take(height).enumerate() {
        let mut end = columns.min(source.cells.len());
        if end < source.cells.len() && source.cells[end].width == 0 {
            end -= 1;
        }
        cropped[index].cells[..end].clone_from_slice(&source.cells[..end]);
        cropped[index].wrap = if columns == old_columns && index + 1 < height.min(old_height) {
            source.wrap
        } else {
            None
        };
    }
    assert_eq!(
        screen
            .buffer(ScreenBuffer::Alternate)
            .iter_rows()
            .map(|value| plain(row(value)))
            .collect::<Vec<_>>(),
        cropped
    );
    let expected_live = if active == ScreenBuffer::Primary {
        crop_cursor(pivot, start, height)
    } else {
        Cursor {
            row: live.row.min(height - 1),
            column: live.column.min(columns - 1),
            wrap_pending: false,
        }
    };
    assert_eq!(screen.cursor(), expected_live);
    assert_eq!(screen.active_buffer(), active);
    assert_eq!(screen.scroll_region(), 0..height);
    assert_eq!(
        (
            screen.origin_mode(),
            screen.auto_wrap(),
            screen.insert_mode(),
            screen.cursor_appearance()
        ),
        modes
    );
    for column in 0..columns {
        assert_eq!(
            screen.is_tab_stop(column),
            Some(tabs.get(column).copied().unwrap_or(column % 8 == 0))
        );
    }
    screen.select_buffer(ScreenBuffer::Primary);
    let expected_offset = if offset == 0 {
        0
    } else {
        let anchor = destinations[2].unwrap().row;
        if anchor >= start {
            0
        } else {
            start - anchor.max(retained_start)
        }
    };
    assert_eq!(screen.viewport_offset(), expected_offset);
    screen.restore_cursor();
    assert_eq!(
        screen.cursor(),
        crop_cursor(cursor(destinations[1].unwrap(), columns), start, height)
    );
    assert_eq!(
        (screen.origin_mode(), screen.auto_wrap()),
        (primary_saved.1, primary_saved.2)
    );
    screen.select_buffer(ScreenBuffer::Alternate);
    screen.restore_cursor();
    assert_eq!(
        screen.cursor(),
        Cursor {
            row: alternate_saved.0.row.min(height - 1),
            column: alternate_saved.0.column.min(columns - 1),
            wrap_pending: alternate_saved.0.wrap_pending
                && alternate_saved.0.row < height
                && alternate_saved.0.column == columns - 1
        }
    );
    assert_eq!(
        (screen.origin_mode(), screen.auto_wrap()),
        (alternate_saved.1, alternate_saved.2)
    );
    // Continue through ordinary mutations to exercise committed capture bookkeeping.
    screen.select_buffer(ScreenBuffer::Primary);
    screen.set_origin_mode(false);
    screen.set_cursor(height - 1, 0);
    screen.line_feed(StyleId(9)).unwrap();
    screen.erase_display(EraseMode::FromCursor, StyleId(9));
    assert!(screen.history().len() <= limits.max_rows);
    assert!(screen.history().retained_bytes() <= limits.max_bytes);
}
