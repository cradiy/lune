use std::ops::Range;

use lune_terminal::{CellPosition, Viewport};

pub(super) fn range(
    viewport: Viewport<'_>,
    mut anchor: CellPosition,
    mut focus: CellPosition,
) -> Option<Range<CellPosition>> {
    if anchor.row != focus.row
        && let Some(line) = sized_line(viewport, anchor)
        && sized_line(viewport, focus) == Some(line)
    {
        let row = line.saturating_sub(viewport.range().start);
        anchor.row = row;
        focus.row = row;
    }
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
    viewport.selection_bounds(start..end)
}

fn sized_line(viewport: Viewport<'_>, position: CellPosition) -> Option<usize> {
    let fragment = viewport.row(position.row)?.sized_text_at(position.column)?;
    (viewport.range().start + position.row).checked_sub(fragment.row_offset)
}

#[cfg(test)]
mod tests {
    use super::range;
    use lune_terminal::{
        CellPosition, HistoryLimits, MAX_STYLES, Processor, Screen, StyleId, UnicodeWriter,
    };

    #[test]
    fn dragging_across_fragments_on_the_same_sized_line_selects_in_column_order() {
        let (palette, _) = super::super::theme::palette(winit::window::Theme::Dark);
        let mut processor = Processor::new(MAX_STYLES, palette).unwrap();
        let mut screen = Screen::new(16, 5).unwrap();
        screen
            .set_history_limits(HistoryLimits {
                max_rows: 4,
                max_bytes: 4096,
            })
            .unwrap();
        processor
            .advance(
                b"\x1b]66;s=3;ABCD\x07",
                &mut screen,
                &mut UnicodeWriter::default(),
            )
            .unwrap();

        let left = CellPosition { row: 1, column: 1 };
        let right = CellPosition { row: 0, column: 10 };
        for retained in [false, true] {
            if retained {
                screen.scroll_up(1, StyleId::default()).unwrap();
            }
            let viewport = screen.viewport();
            let selected = range(viewport, left, right).unwrap();
            assert_eq!(range(viewport, right, left), Some(selected.clone()));
            assert_eq!(
                viewport.selection_text(selected).unwrap().to_string(),
                "ABCD"
            );
        }
    }
}
