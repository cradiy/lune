//! Unified incremental search over terminal scrollback and visible rows
//! for both normal mode (`Ctrl+Shift+F`) and Vi mode (`/` / `?` / `n` / `N`).

use std::ops::Range;

use lune_terminal::{CellPosition, CellText, Row, ScreenBuffer, ScreenView};
use regex::{Regex, RegexBuilder};

use super::vi::Direction;

const QUERY_LIMIT: usize = 512;
const REGEX_LIMIT: usize = 1 << 20;
const MAX_MATCHES: usize = 10_000;

/// Persisted search settings shared across normal search and Vi search (`n`/`N`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SavedSearch {
    pub(super) query: String,
    pub(super) direction: Direction,
    pub(super) case_sensitive: bool,
    pub(super) regex_mode: bool,
}

impl Default for SavedSearch {
    fn default() -> Self {
        Self {
            query: String::new(),
            direction: Direction::Forward,
            case_sensitive: false,
            regex_mode: false,
        }
    }
}

impl SavedSearch {
    /// Finds the next match from `origin` in `self.direction` (or its opposite
    /// when `reverse` is true), wrapping across the screen view.
    pub(super) fn find_from(
        &self,
        view: ScreenView<'_>,
        columns: usize,
        origin: CellPosition,
        reverse: bool,
    ) -> Option<CellPosition> {
        if self.query.is_empty() || columns == 0 || view.rows() == 0 {
            return None;
        }
        let regex = compile_regex(&self.query, self.case_sensitive, self.regex_mode)?;
        let (matches, _) = collect_matches(view, columns, &regex);
        if matches.is_empty() {
            return None;
        }
        let direction = if reverse {
            opposite(self.direction)
        } else {
            self.direction
        };
        let index = select_from_origin(&matches, origin, direction, true);
        Some(matches[index].start)
    }
}

/// A matched cell span in chronological `ScreenView` coordinates.
/// `start` is inclusive; `end` is exclusive on `end.row`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SearchMatch {
    pub(super) start: CellPosition,
    pub(super) end: CellPosition,
}

impl SearchMatch {
    /// Returns the half-open column span covered by this match on `row`.
    pub(super) fn columns_on_row(&self, row: usize, columns: usize) -> Option<Range<usize>> {
        if row < self.start.row || row > self.end.row || columns == 0 {
            return None;
        }
        let start = if row == self.start.row {
            self.start.column.min(columns)
        } else {
            0
        };
        let end = if row == self.end.row {
            self.end.column.min(columns)
        } else {
            columns
        };
        (start < end).then_some(start..end)
    }
}

pub(super) struct SearchBar {
    pub(super) query: String,
    pub(super) direction: Direction,
    pub(super) case_sensitive: bool,
    pub(super) regex_mode: bool,
    pub(super) vi_mode: bool,
    pub(super) vi_origin: Option<CellPosition>,
    pub(super) invalid_regex: bool,
    pub(super) matches: Vec<SearchMatch>,
    pub(super) current: Option<usize>,
    pub(super) buffer: ScreenBuffer,
    truncated: bool,
}

impl SearchBar {
    /// Opens the unified search bar in normal mode, restoring previous settings.
    pub(super) fn open(
        saved: Option<SavedSearch>,
        buffer: ScreenBuffer,
        view: ScreenView<'_>,
        columns: usize,
        viewport: Range<usize>,
    ) -> Self {
        let saved = saved.unwrap_or_default();
        let mut bar = Self {
            query: saved.query,
            direction: Direction::Forward,
            case_sensitive: saved.case_sensitive,
            regex_mode: saved.regex_mode,
            vi_mode: false,
            vi_origin: None,
            invalid_regex: false,
            matches: Vec::new(),
            current: None,
            buffer,
            truncated: false,
        };
        bar.recompute(view, columns, viewport);
        bar
    }

    /// Opens the unified search bar from Vi mode (`/` or `?`) with a fresh query
    /// anchored at `origin`.
    pub(super) fn open_vi(
        saved: Option<&SavedSearch>,
        direction: Direction,
        origin: CellPosition,
        buffer: ScreenBuffer,
    ) -> Self {
        let case_sensitive = saved.is_some_and(|saved| saved.case_sensitive);
        Self {
            query: String::new(),
            direction,
            case_sensitive,
            regex_mode: true,
            vi_mode: true,
            vi_origin: Some(origin),
            invalid_regex: false,
            matches: Vec::new(),
            current: None,
            buffer,
            truncated: false,
        }
    }

    pub(super) fn into_saved(self, fallback: Option<SavedSearch>) -> SavedSearch {
        if self.query.is_empty()
            && let Some(mut previous) = fallback
        {
            previous.direction = self.direction;
            return previous;
        }
        SavedSearch {
            query: self.query,
            direction: self.direction,
            case_sensitive: self.case_sensitive,
            regex_mode: self.regex_mode,
        }
    }

    pub(super) fn push(&mut self, text: &str) -> bool {
        let before = self.query.len();
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            if self.query.len() + ch.len_utf8() > QUERY_LIMIT {
                break;
            }
            self.query.push(ch);
        }
        self.query.len() != before
    }

    pub(super) fn backspace(&mut self) -> bool {
        self.query.pop().is_some()
    }

    pub(super) fn delete_word(&mut self) -> bool {
        let before = self.query.len();
        while self
            .query
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
        {
            self.query.pop();
        }
        while self
            .query
            .chars()
            .next_back()
            .is_some_and(|ch| !ch.is_whitespace())
        {
            self.query.pop();
        }
        self.query.len() != before
    }

    pub(super) fn clear(&mut self) -> bool {
        if self.query.is_empty() {
            return false;
        }
        self.query.clear();
        true
    }

    pub(super) fn toggle_case(&mut self) {
        self.case_sensitive = !self.case_sensitive;
    }

    pub(super) fn toggle_regex(&mut self) {
        self.regex_mode = !self.regex_mode;
    }

    pub(super) fn recompute(
        &mut self,
        view: ScreenView<'_>,
        columns: usize,
        viewport: Range<usize>,
    ) {
        let previous_anchor = self
            .current
            .and_then(|idx| self.matches.get(idx))
            .map(|m| m.start);
        self.matches.clear();
        self.current = None;
        self.invalid_regex = false;
        self.truncated = false;

        if self.query.is_empty() || columns == 0 || view.rows() == 0 {
            return;
        }
        let Some(regex) = compile_regex(&self.query, self.case_sensitive, self.regex_mode) else {
            self.invalid_regex = true;
            return;
        };

        let (matches, truncated) = collect_matches(view, columns, &regex);
        self.matches = matches;
        self.truncated = truncated;
        if self.matches.is_empty() {
            return;
        }
        self.current = Some(if let Some(origin) = self.vi_origin {
            select_from_origin(&self.matches, origin, self.direction, true)
        } else {
            select_initial(&self.matches, previous_anchor, viewport)
        });
    }

    /// Refreshes matches after terminal content changes.
    pub(super) fn refresh(
        &mut self,
        buffer: ScreenBuffer,
        view: ScreenView<'_>,
        columns: usize,
        viewport: Range<usize>,
    ) {
        self.buffer = buffer;
        self.recompute(view, columns, viewport);
    }

    /// Moves to the next (`forward == true`) or previous match and returns its
    /// chronological start position.
    pub(super) fn step(&mut self, forward: bool) -> Option<CellPosition> {
        let len = self.matches.len();
        if len == 0 {
            return None;
        }
        let next = match self.current {
            Some(index) if forward => (index + 1) % len,
            Some(index) => index.checked_sub(1).unwrap_or(len - 1),
            None if forward => 0,
            None => len - 1,
        };
        self.current = Some(next);
        Some(self.matches[next].start)
    }

    pub(super) fn current_match(&self) -> Option<CellPosition> {
        self.current
            .and_then(|idx| self.matches.get(idx))
            .map(|m| m.start)
    }

    /// Iterates matches overlapping `rows`, yielding `(is_current, match)`.
    pub(super) fn visible_matches(
        &self,
        rows: Range<usize>,
    ) -> impl Iterator<Item = (bool, SearchMatch)> + '_ {
        let first = self.matches.partition_point(|m| m.end.row < rows.start);
        let current = self.current;
        self.matches[first..]
            .iter()
            .enumerate()
            .take_while(move |(_, m)| m.start.row < rows.end)
            .map(move |(offset, &m)| (current == Some(first + offset), m))
    }

    pub(super) fn status_label(&self) -> String {
        if self.invalid_regex {
            return "invalid".to_owned();
        }
        if self.query.is_empty() {
            return String::new();
        }
        let total = self.matches.len();
        if total == 0 {
            return "0/0".to_owned();
        }
        let index = self.current.map_or(0, |idx| idx + 1);
        if self.truncated {
            format!("{index}/{total}+")
        } else {
            format!("{index}/{total}")
        }
    }
}

fn opposite(direction: Direction) -> Direction {
    match direction {
        Direction::Forward => Direction::Backward,
        Direction::Backward => Direction::Forward,
    }
}

fn compile_regex(query: &str, case_sensitive: bool, regex_mode: bool) -> Option<Regex> {
    let pattern = if regex_mode {
        std::borrow::Cow::Borrowed(query)
    } else {
        std::borrow::Cow::Owned(regex::escape(query))
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .size_limit(REGEX_LIMIT)
        .dfa_size_limit(REGEX_LIMIT)
        .build()
        .ok()
}

fn collect_matches(
    view: ScreenView<'_>,
    columns: usize,
    regex: &Regex,
) -> (Vec<SearchMatch>, bool) {
    let mut matches = Vec::new();
    let mut truncated = false;
    let mut line = LogicalText::default();
    for logical in view.logical_lines() {
        let rows = logical.range();
        line.read(view, rows, columns);
        if line.text.is_empty() {
            continue;
        }
        for matched in regex.find_iter(&line.text) {
            if matched.start() == matched.end() {
                continue;
            }
            if let Some(span) = line.span(view, matched.start(), matched.end(), columns) {
                matches.push(span);
                if matches.len() >= MAX_MATCHES {
                    truncated = true;
                    break;
                }
            }
        }
        if truncated {
            break;
        }
    }
    (matches, truncated)
}

fn pos_key(pos: CellPosition) -> (usize, usize) {
    (pos.row, pos.column)
}

fn select_from_origin(
    matches: &[SearchMatch],
    origin: CellPosition,
    direction: Direction,
    exclusive: bool,
) -> usize {
    let key = pos_key(origin);
    match direction {
        Direction::Forward => {
            let idx = if exclusive {
                matches.partition_point(|m| pos_key(m.start) <= key)
            } else {
                matches.partition_point(|m| pos_key(m.start) < key)
            };
            if idx < matches.len() { idx } else { 0 }
        }
        Direction::Backward => {
            let idx = if exclusive {
                matches.partition_point(|m| pos_key(m.start) < key)
            } else {
                matches.partition_point(|m| pos_key(m.start) <= key)
            };
            idx.checked_sub(1).unwrap_or(matches.len() - 1)
        }
    }
}

fn select_initial(
    matches: &[SearchMatch],
    previous: Option<CellPosition>,
    viewport: Range<usize>,
) -> usize {
    if let Some(anchor) = previous
        && let Ok(index) = matches.binary_search_by_key(&anchor, |m| m.start)
    {
        return index;
    }
    let after_viewport = matches.partition_point(|m| m.start.row < viewport.end);
    if after_viewport > 0 {
        after_viewport - 1
    } else {
        0
    }
}

#[derive(Default)]
struct LogicalText {
    text: String,
    first_row: usize,
    row_starts: Vec<usize>,
}

impl LogicalText {
    fn read(&mut self, view: ScreenView<'_>, rows: Range<usize>, columns: usize) {
        self.text.clear();
        self.first_row = rows.start;
        self.row_starts.clear();
        for index in rows {
            self.row_starts.push(self.text.len());
            let Some(row) = view.row(index) else { continue };
            visit_row(row, columns, |_, _, text| match text {
                CellText::Scalar(character) => self.text.push(character),
                CellText::Extended(text) => self.text.push_str(text),
                CellText::Continuation => unreachable!(),
            });
        }
    }

    fn span(
        &self,
        view: ScreenView<'_>,
        start_byte: usize,
        end_byte: usize,
        columns: usize,
    ) -> Option<SearchMatch> {
        let start = self.position(view, start_byte, columns, false)?;
        let last_byte = end_byte.checked_sub(1)?;
        let end = self.position(view, last_byte, columns, true)?;
        Some(SearchMatch { start, end })
    }

    fn position(
        &self,
        view: ScreenView<'_>,
        offset: usize,
        columns: usize,
        exclusive_end: bool,
    ) -> Option<CellPosition> {
        let byte = offset.min(self.text.len().checked_sub(1)?);
        let index = self
            .row_starts
            .partition_point(|&start| start <= byte)
            .checked_sub(1)?;
        let row_index = self.first_row + index;
        let mut start = self.row_starts[index];
        let mut position = None;
        visit_row(view.row(row_index)?, columns, |column, width, text| {
            if start <= byte {
                let col = if exclusive_end {
                    (column + width).min(columns)
                } else {
                    column
                };
                position = Some(CellPosition {
                    row: row_index,
                    column: col,
                });
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

fn visit_row(row: &Row, columns: usize, mut visit: impl FnMut(usize, usize, CellText<'_>)) {
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
            visit(unit.column, unit.cell.width(), unit.text);
        }
        if part.row_offset == 0 {
            visit(
                part.column,
                part.columns().len(),
                CellText::Extended(part.text()),
            );
        } else {
            for col in part.column..part.columns().end.min(end) {
                visit(col, 1, CellText::Scalar(' '));
            }
        }
        column = part.columns().end;
    }
    for unit in row.display_units(column..end) {
        visit(unit.column, unit.cell.width(), unit.text);
    }
}

#[cfg(test)]
mod tests {
    use lune_terminal::{MAX_STYLES, Processor, Screen, ScreenBuffer, UnicodeWriter};

    use super::*;

    fn feed_screen(columns: usize, rows: usize, text: &str) -> Screen {
        let (palette, _) = super::super::theme::palette(winit::window::Theme::Dark);
        let mut processor = Processor::new(MAX_STYLES, palette).unwrap();
        let mut screen = Screen::new(columns, rows).unwrap();
        let mut writer = UnicodeWriter::default();
        processor
            .advance(text.as_bytes(), &mut screen, &mut writer)
            .unwrap();
        screen
    }

    #[test]
    fn literal_case_insensitive_and_case_sensitive_search() {
        let screen = feed_screen(40, 5, "Alpha alpha ALPHA\r\nfoo.bar fooXbar");
        let view = screen.view();
        let mut bar = SearchBar::open(None, ScreenBuffer::Primary, view, 40, 0..5);

        bar.push("alpha");
        bar.recompute(view, 40, 0..5);
        assert_eq!(bar.matches.len(), 3);
        assert_eq!(bar.status_label(), "3/3");

        bar.toggle_case();
        bar.recompute(view, 40, 0..5);
        assert_eq!(bar.matches.len(), 1);
        assert_eq!(bar.matches[0].start, CellPosition { row: 0, column: 6 });
        assert_eq!(bar.matches[0].end, CellPosition { row: 0, column: 11 });

        // Literal mode treats `.` as a literal dot rather than wildcard.
        bar.clear();
        bar.push("foo.bar");
        bar.recompute(view, 40, 0..5);
        assert_eq!(bar.matches.len(), 1);
        assert_eq!(bar.matches[0].start, CellPosition { row: 1, column: 0 });

        // Regex mode matches both `foo.bar` and `fooXbar`.
        bar.toggle_regex();
        bar.recompute(view, 40, 0..5);
        assert_eq!(bar.matches.len(), 2);
    }

    #[test]
    fn wrapped_lines_and_wide_characters_map_exact_columns() {
        let screen = feed_screen(6, 4, "ab中文cd");
        let view = screen.view();
        let mut bar = SearchBar::open(None, ScreenBuffer::Primary, view, 6, 0..4);

        bar.push("中文c");
        bar.recompute(view, 6, 0..4);
        assert_eq!(bar.matches.len(), 1);
        let matched = bar.matches[0];
        assert_eq!(matched.start, CellPosition { row: 0, column: 2 });
        assert_eq!(matched.end, CellPosition { row: 1, column: 1 });
        assert_eq!(matched.columns_on_row(0, 6), Some(2..6));
        assert_eq!(matched.columns_on_row(1, 6), Some(0..1));
        assert_eq!(matched.columns_on_row(2, 6), None);
    }

    #[test]
    fn invalid_regex_and_stepping_wrap_cleanly() {
        let screen = feed_screen(20, 4, "one\r\ntwo\r\none");
        let view = screen.view();
        let mut bar = SearchBar::open(None, ScreenBuffer::Primary, view, 20, 0..4);

        bar.toggle_regex();
        bar.push("[unclosed");
        bar.recompute(view, 20, 0..4);
        assert!(bar.invalid_regex);
        assert_eq!(bar.status_label(), "invalid");
        assert!(bar.matches.is_empty());

        bar.clear();
        bar.push("one");
        bar.recompute(view, 20, 0..4);
        assert_eq!(bar.matches.len(), 2);
        assert_eq!(bar.current, Some(1));
        assert_eq!(bar.step(true), Some(CellPosition { row: 0, column: 0 }));
        assert_eq!(bar.step(true), Some(CellPosition { row: 2, column: 0 }));
        assert_eq!(bar.step(false), Some(CellPosition { row: 0, column: 0 }));
    }

    #[test]
    fn vi_regex_search_crosses_soft_wraps_and_repeats_from_origin() {
        let screen = feed_screen(8, 4, "abcdef界ij\r\nnext 界ij");
        let view = screen.view();
        let origin = CellPosition { row: 0, column: 0 };

        let mut bar = SearchBar::open_vi(None, Direction::Forward, origin, ScreenBuffer::Primary);
        bar.push("f界i.");
        bar.recompute(view, 8, 0..4);
        let first = bar.current_match().unwrap();
        assert_eq!(first, CellPosition { row: 0, column: 5 });

        let mut bar = SearchBar::open_vi(None, Direction::Forward, first, ScreenBuffer::Primary);
        bar.push("界i.");
        bar.recompute(view, 8, 0..4);
        let second = bar.current_match().unwrap();
        assert_eq!(second, CellPosition { row: 0, column: 6 });

        let saved = bar.into_saved(None);
        assert_eq!(
            saved.find_from(view, 8, second, false),
            Some(CellPosition { row: 2, column: 5 })
        );
        assert_eq!(
            saved.find_from(view, 8, CellPosition { row: 2, column: 5 }, true),
            Some(CellPosition { row: 0, column: 6 })
        );
    }
}
