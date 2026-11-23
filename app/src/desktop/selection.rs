use std::ops::Range;
use std::time::{Duration, Instant};

use lune_terminal::{CellPosition, Row, Viewport};

const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Unit {
    Cell,
    Word,
    Line,
}

pub(super) struct Click {
    position: CellPosition,
    time: Instant,
    shift: bool,
    unit: Unit,
}

impl Click {
    pub(super) fn new(position: CellPosition, time: Instant, shift: bool) -> Self {
        Self {
            position,
            time,
            shift,
            unit: Unit::Cell,
        }
    }

    pub(super) fn next(self, position: CellPosition, time: Instant, shift: bool) -> Option<Self> {
        (self.position == position
            && self.shift == shift
            && time.duration_since(self.time) <= MULTI_CLICK_INTERVAL)
            .then_some(Self {
                time,
                unit: match self.unit {
                    Unit::Cell => Unit::Word,
                    Unit::Word => Unit::Line,
                    Unit::Line => Unit::Cell,
                },
                ..self
            })
    }

    pub(super) fn drag(&self, viewport: Viewport<'_>) -> Drag {
        let position = self.position;
        let anchor = (self.unit != Unit::Cell
            && cell_range(viewport, position, position).is_some())
        .then(|| self.unit.span(viewport, position))
        .flatten();
        Anchor {
            position,
            unit: self.unit,
            span: anchor,
        }
        .drag(self.shift || self.unit != Unit::Cell)
    }
}

#[derive(Clone)]
pub(super) struct Anchor {
    position: CellPosition,
    unit: Unit,
    span: Option<Range<CellPosition>>,
}

impl Anchor {
    pub(super) fn drag(self, select_on_click: bool) -> Drag {
        Drag {
            anchor: self,
            focus: None,
            select_on_click,
        }
    }
}

pub(super) struct Drag {
    pub(super) anchor: Anchor,
    focus: Option<CellPosition>,
    select_on_click: bool,
}

impl Drag {
    pub(super) fn move_to(&mut self, focus: CellPosition) -> bool {
        if !self.select_on_click && self.focus.is_none() && focus == self.anchor.position {
            return false;
        }
        self.focus.replace(focus) != Some(focus)
    }

    pub(super) fn moved(&self) -> bool {
        self.focus
            .is_some_and(|focus| focus != self.anchor.position)
    }

    pub(super) fn range(&self, viewport: Viewport<'_>) -> Option<Range<CellPosition>> {
        let focus = self.focus?;
        let Some(anchor) = &self.anchor.span else {
            return cell_range(viewport, self.anchor.position, focus);
        };
        if focus == self.anchor.position {
            return viewport.selection_bounds(anchor.clone());
        }
        let focus = self.anchor.unit.span(viewport, focus)?;
        viewport.selection_bounds(anchor.start.min(focus.start)..anchor.end.max(focus.end))
    }
}

impl Unit {
    fn span(self, viewport: Viewport<'_>, position: CellPosition) -> Option<Range<CellPosition>> {
        let position = match sized_line(viewport, position) {
            Some(row) => CellPosition {
                row: row.saturating_sub(viewport.range().start),
                ..position
            },
            None => position,
        };
        match self {
            Self::Cell => cell_range(viewport, position, position),
            Self::Word => word_span(viewport, position),
            Self::Line => line_span(viewport, position.row),
        }
    }
}

fn cell_range(
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Text,
    Space,
}

fn word_class(ch: char) -> Option<WordClass> {
    if ch.is_alphanumeric() || ch == '_' {
        Some(WordClass::Text)
    } else if ch.is_whitespace() {
        Some(WordClass::Space)
    } else {
        None
    }
}

fn word_span(viewport: Viewport<'_>, position: CellPosition) -> Option<Range<CellPosition>> {
    let (mut span, class) = word_unit(viewport, position)?;
    let Some(class) = class else {
        return Some(span);
    };
    while let Some(before) = previous_cell(viewport, span.start)
        && let Some((previous, Some(previous_class))) = word_unit(viewport, before)
        && previous_class == class
    {
        span.start = previous.start;
    }
    while let Some(after) = next_cell(viewport, span.end)
        && let Some((next, Some(next_class))) = word_unit(viewport, after)
        && next_class == class
    {
        span.end = next.end;
    }
    Some(span)
}

fn word_unit(
    viewport: Viewport<'_>,
    position: CellPosition,
) -> Option<(Range<CellPosition>, Option<WordClass>)> {
    let row = viewport.row(position.row)?;
    let limit = row_end(row);
    let (columns, class) = if position.column >= limit {
        (limit..limit, None)
    } else if let Some(part) = row.sized_text_at(position.column) {
        let class = part.text().chars().next().and_then(word_class);
        let class =
            class.filter(|class| part.text().chars().all(|ch| word_class(ch) == Some(*class)));
        (part.columns(), class)
    } else {
        let cells = row.cells();
        let start = position.column - usize::from(cells[position.column].is_continuation());
        let cell = cells[start];
        let class = (!cell.is_erased())
            .then(|| cell.character().and_then(word_class))
            .flatten();
        (start..start + cell.width(), class)
    };
    Some((
        CellPosition {
            column: columns.start,
            ..position
        }..CellPosition {
            column: columns.end,
            ..position
        },
        class,
    ))
}

fn previous_cell(viewport: Viewport<'_>, position: CellPosition) -> Option<CellPosition> {
    if let Some(column) = position.column.checked_sub(1) {
        return Some(CellPosition { column, ..position });
    }
    let row = position.row.checked_sub(1)?;
    let column = viewport.row(row)?.wrap_end()? - 1;
    Some(CellPosition { row, column })
}

fn next_cell(viewport: Viewport<'_>, position: CellPosition) -> Option<CellPosition> {
    let row = viewport.row(position.row)?;
    if position.column < row_end(row) {
        Some(position)
    } else {
        row.wrap_end()?;
        Some(CellPosition {
            row: position.row + 1,
            column: 0,
        })
    }
}

fn line_span(viewport: Viewport<'_>, row: usize) -> Option<Range<CellPosition>> {
    let mut first = row;
    while let Some(previous) = first.checked_sub(1)
        && viewport.row(previous)?.wrap_end().is_some()
    {
        first = previous;
    }
    let mut last = row;
    let mut end = viewport.row(last)?;
    while end.wrap_end().is_some()
        && let Some(next) = viewport.row(last + 1)
    {
        last += 1;
        end = next;
    }
    Some(
        CellPosition {
            row: first,
            column: 0,
        }..CellPosition {
            row: last,
            column: row_end(end),
        },
    )
}

fn row_end(row: &Row) -> usize {
    row.wrap_end().unwrap_or(row.cells().len())
}

fn sized_line(viewport: Viewport<'_>, position: CellPosition) -> Option<usize> {
    let fragment = viewport.row(position.row)?.sized_text_at(position.column)?;
    (viewport.range().start + position.row).checked_sub(fragment.row_offset)
}

#[cfg(test)]
mod tests {
    use super::{Click, MULTI_CLICK_INTERVAL, cell_range};
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
        let now = std::time::Instant::now();
        let word = Click::new(left, now, false).next(left, now, false).unwrap();
        for retained in [false, true] {
            if retained {
                screen.scroll_up(1, StyleId::default()).unwrap();
            }
            let viewport = screen.viewport();
            let selected = cell_range(viewport, left, right).unwrap();
            assert_eq!(cell_range(viewport, right, left), Some(selected.clone()));
            assert_eq!(
                viewport.selection_text(selected).unwrap().to_string(),
                "ABCD"
            );
            let mut drag = word.drag(viewport);
            drag.move_to(left);
            assert_eq!(
                viewport
                    .selection_text(drag.range(viewport).unwrap())
                    .unwrap()
                    .to_string(),
                "ABCD"
            );
        }
    }

    #[test]
    fn multi_click_selection_respects_soft_wraps_and_wide_cells() {
        let (palette, _) = super::super::theme::palette(winit::window::Theme::Dark);
        let mut processor = Processor::new(MAX_STYLES, palette).unwrap();
        let mut screen = Screen::new(8, 4).unwrap();
        processor
            .advance(
                "! abcdef界ij xyz\r\nend".as_bytes(),
                &mut screen,
                &mut UnicodeWriter::default(),
            )
            .unwrap();
        let view = screen.viewport();
        let point = |row, column| CellPosition { row, column };
        let text = |click: &Click, focus| {
            let mut drag = click.drag(view);
            drag.move_to(focus);
            Some(view.selection_text(drag.range(view)?).unwrap().to_string())
        };
        let origin = point(1, 1);
        let now = std::time::Instant::now();
        let single = Click::new(origin, now, false);
        let mut drag = single.drag(view);
        assert!(!drag.move_to(origin));
        assert!(drag.range(view).is_none());
        assert!(drag.move_to(point(1, 0)));
        assert!(drag.move_to(origin));
        assert_eq!(
            view.selection_text(drag.range(view).unwrap())
                .unwrap()
                .to_string(),
            "界"
        );

        let word = single.next(origin, now, false).unwrap();
        assert_eq!(text(&word, origin).as_deref(), Some("abcdef界ij"));
        assert_eq!(text(&word, point(0, 0)).as_deref(), Some("! abcdef界ij"));

        let line = word.next(origin, now, false).unwrap();
        assert_eq!(text(&line, origin).as_deref(), Some("! abcdef界ij xyz"));
        assert_eq!(
            text(&line, point(2, 1)).as_deref(),
            Some("! abcdef界ij xyz\nend")
        );
        let later = now + MULTI_CLICK_INTERVAL * 2;
        assert!(line.next(origin, later, false).is_none());
        let new = Click::new(origin, later, false);
        assert_eq!(text(&new, origin), None);

        let blank = point(2, 7);
        assert!(new.next(blank, later, false).is_none());
        let third = Click::new(blank, later, false)
            .next(blank, later, false)
            .unwrap()
            .next(blank, later, false)
            .unwrap();
        assert_eq!(text(&third, blank), None);
    }
}
