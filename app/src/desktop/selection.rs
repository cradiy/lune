use std::collections::VecDeque;
use std::ops::Range;
use std::time::{Duration, Instant};

use lune_terminal::{CellPosition, Row, RowRevision, ScreenView};

const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const EDGE_SCROLL_DELAY: Duration = Duration::from_millis(120);
const EDGE_SCROLL_INTERVAL: Duration = Duration::from_millis(40);

pub(super) struct Selection {
    pub(super) range: Range<CellPosition>,
    pub(super) anchor: Option<Anchor>,
    rows: VecDeque<RowRevision>,
    discarded_rows: u64,
}

impl Selection {
    pub(super) fn new(
        view: ScreenView<'_>,
        range: Range<CellPosition>,
        anchor: Option<Anchor>,
    ) -> Self {
        let rows = row_revisions(view, range.start.row..range.end.row + 1).collect();
        Self {
            range,
            anchor,
            rows,
            discarded_rows: view.discarded_history_rows(),
        }
    }

    pub(super) fn set_range(&mut self, view: ScreenView<'_>, range: Range<CellPosition>) {
        let old = self.range.start.row..self.range.end.row + 1;
        let new = range.start.row..range.end.row + 1;
        self.range = range;
        if old == new {
            return;
        }
        let shared = old.start.max(new.start)..old.end.min(new.end);
        if shared.is_empty() {
            self.rows.clear();
            self.rows.extend(row_revisions(view, new));
        } else {
            drop(self.rows.drain(..shared.start - old.start));
            self.rows.truncate(shared.len());
            for revision in row_revisions(view, new.start..shared.start).rev() {
                self.rows.push_front(revision);
            }
            self.rows.extend(row_revisions(view, shared.end..new.end));
        }
    }

    pub(super) fn refresh(&mut self, view: ScreenView<'_>) -> bool {
        let discarded_rows = view.discarded_history_rows();
        let removed = discarded_rows.wrapping_sub(self.discarded_rows);
        if removed > self.range.start.row as u64 {
            return false;
        }
        if removed != 0 {
            let removed = removed as usize;
            self.range.start.row -= removed;
            self.range.end.row -= removed;
            if let Some(anchor) = &mut self.anchor
                && anchor.rebase(removed).is_none()
            {
                self.anchor = None;
            }
            self.discarded_rows = discarded_rows;
        }
        row_revisions(view, self.range.start.row..self.range.end.row + 1)
            .eq(self.rows.iter().copied())
    }
}

fn row_revisions(
    view: ScreenView<'_>,
    rows: Range<usize>,
) -> impl DoubleEndedIterator<Item = RowRevision> {
    (rows.start..rows.end.min(view.rows())).map(move |index| {
        view.row(index)
            .expect("A selection row is in bounds")
            .revision()
    })
}

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

    pub(super) fn drag(&self, view: ScreenView<'_>) -> Drag {
        let position = self.position;
        let anchor = (self.unit != Unit::Cell && cell_range(view, position, position).is_some())
            .then(|| self.unit.span(view, position))
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
    fn rebase(&mut self, removed: usize) -> Option<()> {
        self.position.row = self.position.row.checked_sub(removed)?;
        if let Some(span) = &mut self.span {
            span.start.row = span.start.row.checked_sub(removed)?;
            span.end.row -= removed;
        }
        Some(())
    }

    pub(super) fn drag(self, select_on_click: bool) -> Drag {
        Drag {
            anchor: self,
            focus: None,
            select_on_click,
            edge_scroll: None,
        }
    }
}

pub(super) struct Drag {
    pub(super) anchor: Anchor,
    pub(super) edge_scroll: Option<EdgeScroll>,
    focus: Option<CellPosition>,
    select_on_click: bool,
}

pub(super) struct EdgeScroll {
    pub(super) deadline: Instant,
    rows: isize,
}

impl EdgeScroll {
    pub(super) fn due(&mut self, now: Instant) -> Option<isize> {
        if now < self.deadline {
            return None;
        }
        self.deadline = now + EDGE_SCROLL_INTERVAL;
        Some(self.rows)
    }
}

impl Drag {
    pub(super) fn rebase(&mut self, removed: usize) -> Option<()> {
        self.anchor.rebase(removed)?;
        if let Some(focus) = &mut self.focus {
            focus.row = focus.row.checked_sub(removed)?;
        }
        Some(())
    }

    pub(super) fn set_edge_scroll(&mut self, rows: isize, now: Instant) {
        if rows == 0 {
            self.edge_scroll = None;
        } else if self
            .edge_scroll
            .as_ref()
            .is_none_or(|scroll| scroll.rows != rows)
        {
            self.edge_scroll = Some(EdgeScroll {
                rows,
                deadline: now + EDGE_SCROLL_DELAY,
            });
        }
    }

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

    pub(super) fn range(&self, view: ScreenView<'_>) -> Option<Range<CellPosition>> {
        let focus = self.focus?;
        let Some(anchor) = &self.anchor.span else {
            return cell_range(view, self.anchor.position, focus);
        };
        if focus == self.anchor.position {
            return view.selection_bounds(anchor.clone());
        }
        let focus = self.anchor.unit.span(view, focus)?;
        view.selection_bounds(anchor.start.min(focus.start)..anchor.end.max(focus.end))
    }
}

impl Unit {
    fn span(self, view: ScreenView<'_>, position: CellPosition) -> Option<Range<CellPosition>> {
        let position = match sized_line(view, position) {
            Some(row) => CellPosition { row, ..position },
            None => position,
        };
        match self {
            Self::Cell => cell_range(view, position, position),
            Self::Word => word_span(view, position),
            Self::Line => line_span(view, position.row),
        }
    }
}

pub(super) fn cell_range(
    view: ScreenView<'_>,
    mut anchor: CellPosition,
    mut focus: CellPosition,
) -> Option<Range<CellPosition>> {
    if anchor.row != focus.row
        && let Some(line) = sized_line(view, anchor)
        && sized_line(view, focus) == Some(line)
    {
        anchor.row = line;
        focus.row = line;
    }
    let mut start = anchor.min(focus);
    let mut end = anchor.max(focus);
    let start_cells = view.row(start.row)?.cells();
    let end_cells = view.row(end.row)?.cells();
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
    view.selection_bounds(start..end)
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

fn word_span(view: ScreenView<'_>, position: CellPosition) -> Option<Range<CellPosition>> {
    let (mut span, class) = word_unit(view, position)?;
    let Some(class) = class else {
        return Some(span);
    };
    while let Some(before) = previous_cell(view, span.start)
        && let Some((previous, Some(previous_class))) = word_unit(view, before)
        && previous_class == class
    {
        span.start = previous.start;
    }
    while let Some(after) = next_cell(view, span.end)
        && let Some((next, Some(next_class))) = word_unit(view, after)
        && next_class == class
    {
        span.end = next.end;
    }
    Some(span)
}

fn word_unit(
    view: ScreenView<'_>,
    position: CellPosition,
) -> Option<(Range<CellPosition>, Option<WordClass>)> {
    let row = view.row(position.row)?;
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

fn previous_cell(view: ScreenView<'_>, position: CellPosition) -> Option<CellPosition> {
    if let Some(column) = position.column.checked_sub(1) {
        return Some(CellPosition { column, ..position });
    }
    let row = position.row.checked_sub(1)?;
    let column = view.row(row)?.wrap_end()? - 1;
    Some(CellPosition { row, column })
}

fn next_cell(view: ScreenView<'_>, position: CellPosition) -> Option<CellPosition> {
    let row = view.row(position.row)?;
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

pub(super) fn line_span(view: ScreenView<'_>, row: usize) -> Option<Range<CellPosition>> {
    let mut first = row;
    while let Some(previous) = first.checked_sub(1)
        && view.row(previous)?.wrap_end().is_some()
    {
        first = previous;
    }
    let mut last = row;
    let mut end = view.row(last)?;
    while end.wrap_end().is_some()
        && let Some(next) = view.row(last + 1)
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

fn sized_line(view: ScreenView<'_>, position: CellPosition) -> Option<usize> {
    let fragment = view.row(position.row)?.sized_text_at(position.column)?;
    position.row.checked_sub(fragment.row_offset)
}

#[cfg(test)]
mod tests {
    use super::{Click, MULTI_CLICK_INTERVAL, Selection, cell_range};
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

        for retained in [false, true] {
            if retained {
                screen.scroll_up(1, StyleId::default()).unwrap();
            }
            let first = screen.viewport().range().start;
            let left = CellPosition {
                row: first + 1,
                column: 1,
            };
            let right = CellPosition {
                row: first,
                column: 10,
            };
            let now = std::time::Instant::now();
            let word = Click::new(left, now, false).next(left, now, false).unwrap();
            let view = screen.view();
            let selected = cell_range(view, left, right).unwrap();
            assert_eq!(cell_range(view, right, left), Some(selected.clone()));
            assert_eq!(view.selection_text(selected).unwrap().to_string(), "ABCD");
            let mut drag = word.drag(view);
            drag.move_to(left);
            assert_eq!(
                view.selection_text(drag.range(view).unwrap())
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
        let view = screen.view();
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

    #[test]
    fn selection_follows_retained_rows_until_its_content_is_evicted() {
        let (palette, _) = super::super::theme::palette(winit::window::Theme::Dark);
        let mut processor = Processor::new(MAX_STYLES, palette).unwrap();
        let mut screen = Screen::new(8, 3).unwrap();
        screen
            .set_history_limits(HistoryLimits {
                max_rows: 3,
                max_bytes: 4096,
            })
            .unwrap();
        let mut writer = UnicodeWriter::default();
        processor
            .advance(b"a\r\nb\r\nc\r\nd\r\ne\r\nf", &mut screen, &mut writer)
            .unwrap();
        let point = |row, column| CellPosition { row, column };
        let view = screen.view();
        let click = Click::new(point(1, 0), std::time::Instant::now(), false);
        let mut drag = click.drag(view);
        drag.move_to(point(2, 0));
        let original = drag.range(view).unwrap();
        let mut selection = Selection::new(view, original.clone(), Some(drag.anchor.clone()));
        for range in [point(0, 0)..point(3, 1), point(4, 0)..point(5, 1), original] {
            selection.set_range(view, range);
            assert!(selection.refresh(view));
        }

        screen.scroll_viewport_up(3);
        assert!(selection.refresh(screen.view()));
        processor
            .advance(b"\r\ng", &mut screen, &mut writer)
            .unwrap();
        let view = screen.view();
        assert!(selection.refresh(view));
        assert_eq!(selection.range, point(0, 0)..point(1, 1));
        assert!(drag.rebase(1).is_some());
        assert_eq!(drag.range(view), Some(selection.range.clone()));
        assert_eq!(
            view.selection_text(selection.range.clone())
                .unwrap()
                .to_string(),
            "b\nc"
        );
        let mut extension = selection.anchor.clone().unwrap().drag(true);
        assert!(extension.move_to(point(2, 0)));
        assert_eq!(extension.range(view), Some(point(0, 0)..point(2, 1)));

        processor
            .advance(b"\r\nh", &mut screen, &mut writer)
            .unwrap();
        assert!(!selection.refresh(screen.view()));
        assert!(drag.rebase(1).is_none());
    }
}
