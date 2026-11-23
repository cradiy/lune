use std::ops::Range;

use lune_terminal::{CellPosition, Viewport};

pub(super) fn range(
    viewport: Viewport<'_>,
    anchor: CellPosition,
    focus: CellPosition,
) -> Option<Range<CellPosition>> {
    let mut start = anchor.min(focus);
    let mut end = anchor.max(focus);
    let start_cells = viewport.row(start.row)?.cells();
    let end_cells = viewport.row(end.row)?.cells();
    start.column = start.column.min(start_cells.len());
    if start_cells
        .get(start.column)
        .is_some_and(|cell| cell.is_continuation())
    {
        start.column -= 1;
    }
    end.column = (end.column + 1).min(end_cells.len());
    if end_cells
        .get(end.column)
        .is_some_and(|cell| cell.is_continuation())
    {
        end.column += 1;
    }
    (start < end).then_some(start..end)
}

pub(super) fn columns(
    range: &Range<CellPosition>,
    row: usize,
    width: usize,
) -> Option<Range<usize>> {
    if row < range.start.row || row > range.end.row {
        return None;
    }
    let start = if row == range.start.row {
        range.start.column.min(width)
    } else {
        0
    };
    let end = if row == range.end.row {
        range.end.column.min(width)
    } else {
        width
    };
    (start < end).then_some(start..end)
}
