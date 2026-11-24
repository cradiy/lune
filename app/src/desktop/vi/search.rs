use std::ops::Range;

use lune_terminal::{CellPosition, CellText, Row, ScreenView};
use regex::{Regex, RegexBuilder};

use super::{Direction, line_span};

const QUERY_LIMIT: usize = 1024;
const REGEX_LIMIT: usize = 1 << 20;

impl Direction {
    fn opposite(self) -> Self {
        match self {
            Self::Forward => Self::Backward,
            Self::Backward => Self::Forward,
        }
    }
}

pub(in crate::desktop) struct Prompt {
    direction: Direction,
    text: String,
}

impl Prompt {
    pub(super) fn new(direction: Direction) -> Self {
        let text = match direction {
            Direction::Forward => "/",
            Direction::Backward => "?",
        }
        .to_owned();
        Self { direction, text }
    }

    pub(in crate::desktop) fn text(&self) -> &str {
        &self.text
    }

    pub(in crate::desktop) fn push(&mut self, text: &str) {
        for character in text.chars().filter(|ch| !ch.is_control()) {
            if self.text.len() + character.len_utf8() > QUERY_LIMIT {
                break;
            }
            self.text.push(character);
        }
    }

    pub(in crate::desktop) fn backspace(&mut self) {
        if self.text.len() > 1 {
            self.text.pop();
        }
    }

    pub(super) fn confirm(self, search: &mut Option<Search>) -> bool {
        let query = &self.text[1..];
        if query.is_empty() {
            if let Some(search) = search {
                search.direction = self.direction;
                return true;
            }
            return false;
        }
        let Ok(regex) = RegexBuilder::new(query)
            .size_limit(REGEX_LIMIT)
            .dfa_size_limit(REGEX_LIMIT)
            .build()
        else {
            return false;
        };
        *search = Some(Search {
            direction: self.direction,
            regex,
        });
        true
    }
}

pub(in crate::desktop) struct Search {
    direction: Direction,
    regex: Regex,
}

impl Search {
    pub(super) fn find(
        &self,
        view: ScreenView<'_>,
        columns: usize,
        point: CellPosition,
        reverse: bool,
    ) -> Option<CellPosition> {
        let direction = if reverse {
            self.direction.opposite()
        } else {
            self.direction
        };
        let starting_line = line_span(view, point.row)?;
        let start = starting_line.start.row..starting_line.end.row + 1;
        let mut text = Line::default();
        text.read(view, start.clone(), columns);
        if let Some(byte) = text.find(
            &self.regex,
            direction,
            Some((view.row(point.row)?, point)),
            columns,
        ) {
            return text.position(view, byte, columns);
        }
        let step = |rows: &Range<usize>| match direction {
            Direction::Forward => rows.end % view.rows(),
            Direction::Backward => rows.start.checked_sub(1).unwrap_or(view.rows() - 1),
        };
        let mut row = step(&start);
        while !start.contains(&row) {
            let span = line_span(view, row)?;
            let rows = span.start.row..span.end.row + 1;
            text.read(view, rows.clone(), columns);
            if let Some(byte) = text.find(&self.regex, direction, None, columns) {
                return text.position(view, byte, columns);
            }
            row = step(&rows);
        }
        text.read(view, start, columns);
        let byte = text.find(&self.regex, direction, None, columns)?;
        text.position(view, byte, columns)
    }
}

#[derive(Default)]
struct Line {
    text: String,
    first_row: usize,
    row_starts: Vec<usize>,
}

impl Line {
    fn read(&mut self, view: ScreenView<'_>, rows: Range<usize>, columns: usize) {
        self.text.clear();
        self.first_row = rows.start;
        self.row_starts.clear();
        for index in rows {
            self.row_starts.push(self.text.len());
            visit_row(view.row(index).unwrap(), columns, |_, text| match text {
                CellText::Scalar(character) => self.text.push(character),
                CellText::Extended(text) => self.text.push_str(text),
                CellText::Continuation => unreachable!(),
            });
        }
    }

    fn find(
        &self,
        regex: &Regex,
        direction: Direction,
        origin: Option<(&Row, CellPosition)>,
        columns: usize,
    ) -> Option<usize> {
        if self.text.is_empty() {
            return None;
        }
        let boundary = origin.map(|(row, point)| {
            let mut byte = self.row_starts[point.row - self.first_row];
            visit_row(row, columns, |column, text| {
                if column < point.column
                    || (column == point.column && direction == Direction::Forward)
                {
                    byte += utf8_len(text);
                }
            });
            byte
        });
        match direction {
            Direction::Forward => {
                let start = boundary.unwrap_or(0);
                (start < self.text.len())
                    .then(|| regex.find_at(&self.text, start))
                    .flatten()
                    .map(|matched| matched.start())
            }
            Direction::Backward => regex
                .find_iter(&self.text)
                .map(|matched| matched.start())
                .take_while(|&byte| boundary.is_none_or(|end| byte < end))
                .last(),
        }
    }

    fn position(
        &self,
        view: ScreenView<'_>,
        offset: usize,
        columns: usize,
    ) -> Option<CellPosition> {
        let byte = offset.min(self.text.len().checked_sub(1)?);
        let index = self.row_starts.partition_point(|&start| start <= byte) - 1;
        let row = self.first_row + index;
        let mut start = self.row_starts[index];
        let mut position = None;
        visit_row(view.row(row)?, columns, |column, text| {
            if start <= byte {
                position = Some(CellPosition { row, column });
            }
            start += utf8_len(text);
        });
        position
    }
}

fn utf8_len(text: CellText<'_>) -> usize {
    match text {
        CellText::Scalar(character) => character.len_utf8(),
        CellText::Extended(text) => text.len(),
        CellText::Continuation => unreachable!(),
    }
}

fn visit_row(row: &Row, columns: usize, mut visit: impl FnMut(usize, CellText<'_>)) {
    let end = if let Some(wrap) = row.wrap_end() {
        wrap.min(columns)
    } else {
        let limit = row.cells().len().min(columns);
        let cells = row.cells()[..limit]
            .iter()
            .rposition(|cell| !cell.is_erased())
            .map_or(0, |last| last + 1);
        row.sized_text_rows()
            .take_while(|part| part.column < limit)
            .last()
            .map_or(cells, |part| cells.max(part.columns().end.min(limit)))
    };
    let mut column = 0;
    for part in row.sized_text_rows().take_while(|part| part.column < end) {
        for unit in row.display_units(column..part.column) {
            visit(unit.column, unit.text);
        }
        if part.row_offset == 0 {
            visit(part.column, CellText::Extended(part.text()));
        } else {
            for _ in part.column..part.columns().end.min(end) {
                visit(part.column, CellText::Scalar(' '));
            }
        }
        column = part.columns().end;
    }
    for unit in row.display_units(column..end) {
        visit(unit.column, unit.text);
    }
}
