use std::{
    fmt::{self, Write},
    iter::FusedIterator,
    ops::Range,
};

use crate::{Grid, Row, RowHistory, StorageError};

/// A row index and a column boundary in the addressed grid or screen view.
/// Column `row.cells().len()` is that row's end. Positions are ordered by row,
/// then column. View row indices start at the oldest retained row and remain
/// valid for the lifetime of the borrow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellPosition {
    pub row: usize,
    pub column: usize,
}

/// Borrowed chronological rows of the active screen buffer.
/// Primary views contain retained history followed by visible rows. Alternate
/// views contain visible rows only. Historical rows retain their captured widths.
/// The borrow fixes contents and row indices; construction, lookup, and range
/// validation take constant time and make no allocation requests.
#[derive(Clone, Copy, Debug)]
pub struct ScreenView<'a> {
    grid: &'a Grid,
    history: Option<&'a RowHistory>,
}

impl<'a> ScreenView<'a> {
    #[inline]
    pub(crate) fn same_source(&self, other: ScreenView<'_>) -> bool {
        std::ptr::eq(self.grid, other.grid)
            && match (self.history, other.history) {
                (Some(left), Some(right)) => std::ptr::eq(left, right),
                (None, None) => true,
                _ => false,
            }
    }

    pub(crate) fn new(grid: &'a Grid, history: Option<&'a RowHistory>) -> Self {
        Self { grid, history }
    }

    /// Number of history rows before the visible buffer. Add this to a physical
    /// cursor row to address it in this view.
    pub fn history_rows(&self) -> usize {
        self.history.map_or(0, RowHistory::len)
    }

    pub fn rows(&self) -> usize {
        self.history_rows() + self.grid.rows()
    }

    pub fn row(&self, index: usize) -> Option<&'a Row> {
        let history_rows = self.history_rows();
        if index < history_rows {
            self.history.and_then(|history| history.row(index))
        } else {
            self.grid.row(index - history_rows)
        }
    }

    pub(crate) fn live_grid(&self) -> &'a Grid {
        self.grid
    }

    pub fn iter_live_rows(&self) -> impl ExactSizeIterator<Item = &'a Row> + use<'a> {
        self.grid.iter_rows()
    }

    /// Row revisions of the oldest and newest retained history entries.
    pub fn history_endpoints(&self) -> Option<[crate::RowRevision; 2]> {
        let history = self.history?;
        let first = history.row(0)?.revision();
        let last = history.row(history.len() - 1)?.revision();
        Some([first, last])
    }

    pub(crate) fn placeholder_rows(&self) -> impl Iterator<Item = (i64, &'a Row)> + use<'a> {
        let history_rows = self.history_rows() as i64;
        self.history
            .into_iter()
            .flat_map(RowHistory::placeholder_rows)
            .map(move |(index, row)| (index as i64 - history_rows, row))
            .chain(
                self.grid
                    .iter_rows()
                    .enumerate()
                    .map(|(index, row)| (index as i64, row)),
            )
    }

    /// Borrows rows in chronological order. Skipping rows and reverse traversal
    /// preserve constant-time positioning.
    pub fn iter_rows(
        &self,
    ) -> impl ExactSizeIterator<Item = &'a Row> + DoubleEndedIterator + FusedIterator + use<'a>
    {
        self.rows_in(0..self.rows())
    }

    pub(crate) fn rows_in(
        &self,
        range: Range<usize>,
    ) -> impl ExactSizeIterator<Item = &'a Row> + DoubleEndedIterator + FusedIterator + use<'a>
    {
        let view = *self;
        range.map(move |index| view.row(index).expect("A view row is in bounds"))
    }

    /// Validates a row-major half-open range against each endpoint row's width.
    /// Endpoint rows must exist; columns may equal their row's width. Reversed
    /// ranges fail. Borrowed coordinates remain fixed until the borrow ends.
    pub fn text(&self, range: Range<CellPosition>) -> Result<TextRange<'a>, StorageError> {
        TextRange::new(*self, range)
    }
}

/// A validated, read-only text range borrowed from grid or screen rows.
/// Formatting streams complete display units, preserves selected spaces, joins
/// soft wraps, omits wrap gaps, and separates hard rows with newlines. Empty
/// ranges emit nothing. Traversal visits only selected rows with constant auxiliary
/// space. The output sink owns allocation and may retain a prefix on failure.
/// The range can be formatted repeatedly for the lifetime of its source borrow.
#[derive(Clone, Copy, Debug)]
pub struct TextRange<'a> {
    view: ScreenView<'a>,
    start: CellPosition,
    end: CellPosition,
}

impl<'a> TextRange<'a> {
    pub(crate) fn new(
        view: ScreenView<'a>,
        range: Range<CellPosition>,
    ) -> Result<Self, StorageError> {
        if range.start > range.end
            || view
                .row(range.start.row)
                .is_none_or(|row| range.start.column > row.cells().len())
            || view
                .row(range.end.row)
                .is_none_or(|row| range.end.column > row.cells().len())
        {
            return Err(StorageError::OutOfBounds);
        }
        Ok(Self {
            view,
            start: range.start,
            end: range.end,
        })
    }
}

impl fmt::Display for TextRange<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        for index in self.start.row..=self.end.row {
            let row = self.view.row(index).expect("Range endpoints are validated");
            let columns = row.cells().len();
            let start = if index == self.start.row {
                self.start.column
            } else {
                0
            };
            let end = if index == self.end.row {
                self.end.column
            } else {
                columns
            };
            let wrap = row.wrap_end();
            row.write_text(start..end.min(wrap.unwrap_or(columns)), output)?;
            if index != self.end.row && wrap.is_none() {
                output.write_char('\n')?;
            }
        }
        Ok(())
    }
}
