mod client;

use std::ops::Range;

use lune_terminal::{CellPosition, Row, ScreenBuffer, ScreenView, Viewport};

use super::selection::{cell_range, line_span};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Command {
    Move(Motion),
    Scroll(isize),
    TopPrefix,
    Visual(VisualKind),
    InnerOrExit,
    AroundOrExit,
    WordObject(bool),
    Yank,
    Exit,
    Cancel,
    Search(Direction),
    RepeatSearch(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Motion {
    Left,
    Down,
    Up,
    Right,
    First,
    FirstOccupied,
    Last,
    NextWord,
    PreviousWord,
    EndWord,
    Top,
    Bottom,
    High,
    Middle,
    Low,
    Page(i8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    Forward,
    Backward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VisualKind {
    Cell,
    Line,
}

#[derive(Clone, Copy)]
enum Pending {
    Top,
    WordObject(bool),
}

struct Visual {
    anchor: CellPosition,
    kind: VisualKind,
}

pub(super) struct Mode {
    pub(super) buffer: ScreenBuffer,
    pub(super) cursor: CellPosition,
    visual: Option<Visual>,
    pending: Option<Pending>,
    discarded_rows: u64,
    preferred_column: usize,
}

impl Mode {
    pub(super) fn new(buffer: ScreenBuffer, viewport: Viewport<'_>, columns: usize) -> Self {
        let start = viewport.range().start;
        let cursor = viewport.cursor().map_or(
            CellPosition {
                row: start,
                column: 0,
            },
            |cursor| CellPosition {
                row: start + cursor.row,
                column: cursor.column,
            },
        );
        let view = viewport.source();
        Self {
            buffer,
            cursor: normalize(view, cursor, columns),
            visual: None,
            pending: None,
            discarded_rows: view.discarded_history_rows(),
            preferred_column: cursor.column,
        }
    }

    pub(super) fn resolve(&mut self, command: Option<Command>) -> Option<Command> {
        let pending = self.pending.take();
        let command = command?;
        Some(match (pending, command) {
            (Some(Pending::WordObject(around)), Command::Move(Motion::NextWord)) => {
                Command::WordObject(around)
            }
            (Some(Pending::WordObject(_)), _) => return None,
            (Some(Pending::Top), Command::TopPrefix) => Command::Move(Motion::Top),
            (_, Command::TopPrefix) => {
                self.pending = Some(Pending::Top);
                return None;
            }
            (_, Command::InnerOrExit | Command::AroundOrExit) => {
                if self.visual.is_some() {
                    self.pending = Some(Pending::WordObject(command == Command::AroundOrExit));
                    return None;
                }
                Command::Exit
            }
            (_, Command::Cancel) if pending.is_some() => return None,
            (_, command) => command,
        })
    }

    pub(super) fn clear_visual(&mut self) {
        self.visual = None;
        self.cancel_pending();
    }

    pub(super) fn cancel_pending(&mut self) {
        self.pending = None;
    }

    pub(super) fn toggle_visual(&mut self, kind: VisualKind) {
        self.visual = match self.visual.take() {
            Some(visual) if visual.kind == kind => None,
            Some(visual) => Some(Visual { kind, ..visual }),
            None => Some(Visual {
                anchor: self.cursor,
                kind,
            }),
        };
    }

    pub(super) fn selection(&self, view: ScreenView<'_>) -> Option<Range<CellPosition>> {
        let visual = self.visual.as_ref()?;
        match visual.kind {
            VisualKind::Cell => cell_range(view, visual.anchor, self.cursor),
            VisualKind::Line => {
                let start = line_span(view, visual.anchor.row.min(self.cursor.row))?;
                let end = line_span(view, visual.anchor.row.max(self.cursor.row))?;
                Some(start.start..end.end)
            }
        }
    }

    pub(super) fn linewise(&self) -> bool {
        self.visual
            .as_ref()
            .is_some_and(|visual| visual.kind == VisualKind::Line)
    }

    pub(super) fn select_word(&mut self, view: ScreenView<'_>, columns: usize, around: bool) {
        if self.visual.is_none() {
            return;
        }
        let mut start = self.cursor;
        let mut end = self.cursor;
        while let Some(previous) = adjacent(view, start, columns, Direction::Backward, false)
            && same_word(view, previous, start)
        {
            start = previous;
        }
        while let Some(next) = adjacent(view, end, columns, Direction::Forward, false)
            && same_word(view, end, next)
        {
            end = next;
        }
        if around && class_at(view, start) != WordClass::Space {
            let mut whitespace = end;
            let mut next = adjacent(view, whitespace, columns, Direction::Forward, false);
            while let Some(point) = next.filter(|&point| class_at(view, point) == WordClass::Space)
            {
                whitespace = point;
                next = adjacent(view, whitespace, columns, Direction::Forward, false);
            }
            if next.is_some() {
                end = whitespace;
            } else if whitespace != end {
                while let Some(previous) =
                    adjacent(view, start, columns, Direction::Backward, false)
                    && class_at(view, previous) == WordClass::Space
                {
                    start = previous;
                }
            }
        }
        self.visual = Some(Visual {
            anchor: start,
            kind: VisualKind::Cell,
        });
        self.set_cursor(end);
    }

    pub(super) fn set_cursor(&mut self, cursor: CellPosition) {
        self.cursor = cursor;
        self.preferred_column = cursor.column;
    }

    pub(super) fn rebase(&mut self, view: ScreenView<'_>, columns: usize) {
        let discarded = view.discarded_history_rows();
        let removed =
            usize::try_from(discarded.wrapping_sub(self.discarded_rows)).unwrap_or(usize::MAX);
        self.discarded_rows = discarded;
        self.cursor.row = self.cursor.row.saturating_sub(removed);
        if let Some(visual) = &mut self.visual {
            if let Some(row) = visual.anchor.row.checked_sub(removed) {
                visual.anchor.row = row;
            } else {
                self.clear_visual();
            }
        }
        self.cursor = normalize(view, self.cursor, columns);
    }

    pub(super) fn move_cursor(
        &mut self,
        motion: Motion,
        viewport: Viewport<'_>,
        columns: usize,
    ) -> isize {
        let cursor = self.cursor;
        let view = viewport.source();
        let rows = viewport.range();
        let mut scroll = 0;
        let vertical = |row| {
            Some(CellPosition {
                row,
                column: self.preferred_column,
            })
        };
        let next = match motion {
            Motion::Up => vertical(cursor.row.saturating_sub(1)),
            Motion::Down => vertical(cursor.row + 1),
            Motion::Left => adjacent(view, cursor, columns, Direction::Backward, false),
            Motion::Right => adjacent(view, cursor, columns, Direction::Forward, false),
            Motion::First => Some(CellPosition {
                column: 0,
                ..cursor
            }),
            Motion::FirstOccupied => Some(occupied(view, cursor.row, columns, false)),
            Motion::Last => Some(occupied(view, cursor.row, columns, true)),
            Motion::NextWord | Motion::PreviousWord | Motion::EndWord => {
                Some(word_motion(view, cursor, columns, motion))
            }
            Motion::High => Some(occupied(view, rows.start, columns, false)),
            Motion::Middle => Some(occupied(view, rows.start + rows.len() / 2, columns, false)),
            Motion::Low => Some(occupied(view, rows.end - 1, columns, false)),
            Motion::Top => {
                scroll = isize::MAX;
                Some(occupied(view, 0, columns, false))
            }
            Motion::Bottom => {
                scroll = isize::MIN;
                Some(occupied(view, view.rows() - 1, columns, false))
            }
            Motion::Page(pages) => {
                let step = if pages.unsigned_abs() == 1 {
                    (rows.len() / 2).max(1)
                } else {
                    rows.len()
                } as isize;
                let delta = step * isize::from(pages.signum());
                scroll = -delta;
                vertical(cursor.row.saturating_add_signed(delta))
            }
        };
        if let Some(next) = next {
            self.cursor = normalize(view, next, columns);
            if !matches!(motion, Motion::Up | Motion::Down | Motion::Page(_)) {
                self.preferred_column = self.cursor.column;
            }
        }
        scroll
    }
}

pub(super) fn normalize(
    view: ScreenView<'_>,
    mut point: CellPosition,
    columns: usize,
) -> CellPosition {
    point.row = point.row.min(view.rows() - 1);
    let row = view.row(point.row).unwrap();
    point.column = point.column.min(row_limit(row, columns) - 1);
    point.column = if let Some(part) = row.sized_text_at(point.column) {
        part.column
    } else {
        point.column - usize::from(row.cells()[point.column].is_continuation())
    };
    point
}

fn row_limit(row: &Row, columns: usize) -> usize {
    row.wrap_end().unwrap_or(row.cells().len()).min(columns)
}

fn occupied(view: ScreenView<'_>, row: usize, columns: usize, last: bool) -> CellPosition {
    let source = view.row(row).unwrap();
    let limit = row_limit(source, columns);
    let columns = 0..limit;
    let written = |column| class_at(view, CellPosition { row, column }) != WordClass::Space;
    let column = if last {
        columns.rev().find(|&column| written(column))
    } else {
        columns.into_iter().find(|&column| written(column))
    };
    normalize(
        view,
        CellPosition {
            row,
            column: column.unwrap_or(0),
        },
        limit,
    )
}

fn adjacent(
    view: ScreenView<'_>,
    point: CellPosition,
    columns: usize,
    direction: Direction,
    hard_wrap: bool,
) -> Option<CellPosition> {
    let row = view.row(point.row)?;
    match direction {
        Direction::Forward => {
            let column = row.sized_text_at(point.column).map_or_else(
                || point.column + row.cells()[point.column].width(),
                |part| part.columns().end,
            );
            if column < row_limit(row, columns) {
                Some(CellPosition { column, ..point })
            } else if point.row + 1 < view.rows() && (hard_wrap || row.wrap_end().is_some()) {
                Some(normalize(
                    view,
                    CellPosition {
                        row: point.row + 1,
                        column: 0,
                    },
                    columns,
                ))
            } else {
                None
            }
        }
        Direction::Backward => {
            if let Some(column) = point.column.checked_sub(1) {
                Some(normalize(view, CellPosition { column, ..point }, columns))
            } else {
                let previous = point.row.checked_sub(1)?;
                let row = view.row(previous)?;
                (hard_wrap || row.wrap_end().is_some()).then(|| {
                    normalize(
                        view,
                        CellPosition {
                            row: previous,
                            column: columns - 1,
                        },
                        columns,
                    )
                })
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Space,
    Text,
    Punctuation,
}

fn class_at(view: ScreenView<'_>, point: CellPosition) -> WordClass {
    let row = view.row(point.row).unwrap();
    let ch = row
        .sized_text_at(point.column)
        .and_then(|part| part.text().chars().next())
        .or_else(|| {
            let leading = point.column - usize::from(row.cells()[point.column].is_continuation());
            row.cells()[leading].character()
        })
        .unwrap_or(' ');
    if ch.is_whitespace() {
        WordClass::Space
    } else if ch.is_alphanumeric() || ch == '_' {
        WordClass::Text
    } else {
        WordClass::Punctuation
    }
}

fn same_word(view: ScreenView<'_>, left: CellPosition, right: CellPosition) -> bool {
    (left.row == right.row
        || view
            .row(left.row)
            .is_some_and(|row| row.wrap_end().is_some()))
        && class_at(view, left) == class_at(view, right)
}

fn word_motion(
    view: ScreenView<'_>,
    start: CellPosition,
    columns: usize,
    motion: Motion,
) -> CellPosition {
    let before = |point| adjacent(view, point, columns, Direction::Backward, true);
    let after = |point| adjacent(view, point, columns, Direction::Forward, true);
    let mut point = start;
    if motion == Motion::PreviousWord {
        let Some(previous) = before(point) else {
            return point;
        };
        point = previous;
        while class_at(view, point) == WordClass::Space {
            let Some(previous) = before(point) else {
                return point;
            };
            point = previous;
        }
        while let Some(previous) = before(point)
            && same_word(view, previous, point)
        {
            point = previous;
        }
    } else {
        if motion == Motion::NextWord {
            while let Some(next) = after(point)
                && same_word(view, point, next)
            {
                point = next;
            }
        }
        let Some(next) = after(point) else {
            return point;
        };
        point = next;
        while class_at(view, point) == WordClass::Space {
            let Some(next) = after(point) else {
                return point;
            };
            point = next;
        }
        if motion == Motion::EndWord {
            while let Some(next) = after(point)
                && same_word(view, point, next)
            {
                point = next;
            }
        }
    }
    point
}

#[cfg(test)]
mod tests {
    use lune_terminal::{MAX_STYLES, Processor, Screen, UnicodeWriter};

    use super::*;

    fn screen(text: &str, columns: usize, rows: usize) -> Screen {
        let (palette, _) = super::super::theme::palette(winit::window::Theme::Dark);
        let mut processor = Processor::new(MAX_STYLES, palette).unwrap();
        let mut screen = Screen::new(columns, rows).unwrap();
        processor
            .advance(text.as_bytes(), &mut screen, &mut UnicodeWriter::default())
            .unwrap();
        screen
    }

    fn point(row: usize, column: usize) -> CellPosition {
        CellPosition { row, column }
    }

    #[test]
    fn word_motions_and_visual_objects_keep_wrapped_wide_cells_intact() {
        let screen = screen("row 000 alpha\r\n  abcdef界ij xyz", 8, 5);
        let viewport = screen.viewport();
        let view = screen.view();
        let mut vi = Mode::new(ScreenBuffer::Primary, viewport, 8);
        vi.set_cursor(point(0, 0));
        vi.move_cursor(Motion::NextWord, viewport, 8);
        assert_eq!(vi.cursor, point(0, 4));
        vi.move_cursor(Motion::NextWord, viewport, 8);
        assert_eq!(vi.cursor, point(1, 0));
        vi.set_cursor(point(3, 1));
        assert_eq!(normalize(view, vi.cursor, 8), point(3, 0));
        vi.set_cursor(point(3, 0));
        vi.toggle_visual(VisualKind::Cell);
        vi.select_word(view, 8, false);
        assert_eq!(
            view.selection_text(vi.selection(view).unwrap())
                .unwrap()
                .to_string(),
            "abcdef界ij"
        );
    }
}
