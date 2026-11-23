use std::{
    fmt::{self, Write},
    iter::FusedIterator,
    ops::Range,
};

use crate::{Grid, Row, RowHistory, SizedTextRow, StorageError};

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

    /// Counts oldest history rows discarded by eviction or clearing. Reflow can
    /// change row indices independently. Alternate views report zero.
    pub fn discarded_history_rows(&self) -> u64 {
        self.history.map_or(0, RowHistory::discarded_rows)
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

    /// Revision of retained history rows within the screen.
    pub fn history_revision(&self) -> Option<u64> {
        self.history.and_then(RowHistory::revision)
    }

    pub(crate) fn placeholder_rows(&self) -> impl Iterator<Item = (i64, &'a Row)> + use<'a> {
        self.history_placeholder_rows().chain(
            self.grid
                .iter_rows()
                .enumerate()
                .map(|(index, row)| (index as i64, row)),
        )
    }

    pub(crate) fn history_placeholder_rows(
        &self,
    ) -> impl Iterator<Item = (i64, &'a Row)> + use<'a> {
        let history_rows = self.history_rows() as i64;
        self.history
            .into_iter()
            .flat_map(RowHistory::placeholder_rows)
            .map(move |(index, row)| (index as i64 - history_rows, row))
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

    /// Trims unwritten padding in a range addressed in the full screen view.
    pub fn selection_text(
        &self,
        range: Range<CellPosition>,
    ) -> Result<TextRange<'a, true>, StorageError> {
        self.text(range).map(TextRange::trim_erased)
    }

    /// Contracts a selection to its first and last occupied cells.
    pub fn selection_bounds(&self, range: Range<CellPosition>) -> Option<Range<CellPosition>> {
        self.selection_text(range).ok()?.content_bounds()
    }
}

/// A validated, read-only text range borrowed from grid or screen rows.
/// Formatting streams complete display units, preserves selected spaces, joins
/// soft wraps, omits wrap gaps, and separates hard rows with newlines. Empty
/// ranges emit nothing. Traversal visits only selected rows with constant auxiliary
/// space. The output sink owns allocation and may retain a prefix on failure.
/// The range can be formatted repeatedly for the lifetime of its source borrow.
#[derive(Clone, Copy, Debug)]
pub struct TextRange<'a, const TRIM_ERASED: bool = false> {
    view: ScreenView<'a>,
    start: CellPosition,
    end: CellPosition,
}

impl<'a, const TRIM_ERASED: bool> TextRange<'a, TRIM_ERASED> {
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

    pub(crate) fn trim_erased(self) -> TextRange<'a, true> {
        TextRange {
            view: self.view,
            start: self.start,
            end: self.end,
        }
    }

    pub(crate) fn content_bounds(&self) -> Option<Range<CellPosition>> {
        let (first, start) = (self.start.row..=self.end.row)
            .find_map(|row| self.columns_on_row(row).map(|columns| (row, columns)))?;
        let (last, end) = (first + 1..=self.end.row)
            .rev()
            .find_map(|row| self.columns_on_row(row).map(|columns| (row, columns.end)))
            .unwrap_or((first, start.end));
        Some(
            CellPosition {
                row: first,
                column: start.start,
            }..CellPosition {
                row: last,
                column: end,
            },
        )
    }

    /// Columns used by this text range at a row in the source view.
    pub fn columns_on_row(&self, index: usize) -> Option<Range<usize>> {
        if !(self.start.row..=self.end.row).contains(&index) {
            return None;
        }
        let row = self.view.row(index)?;
        let columns = self.text_columns(index, row, row.wrap_end().unwrap_or(row.cells().len()));
        (!columns.is_empty()).then_some(columns)
    }

    /// Whether a sized block intersects this range. The row index is in the source view.
    pub fn includes_sized_text(&self, row: usize, part: &SizedTextRow) -> bool {
        let Some(head) = row.checked_sub(part.row_offset) else {
            return false;
        };
        let end = head.saturating_add(usize::from(part.size().scale));
        self.overlaps_sized(head.max(self.start.row)..end.min(self.end.row + 1), part)
    }

    fn overlaps_sized(&self, mut rows: Range<usize>, part: &SizedTextRow) -> bool {
        let footprint = part.columns();
        rows.any(|index| {
            let Some(row) = self.view.row(index) else {
                return false;
            };
            let columns = self.columns(index, row.wrap_end().unwrap_or(row.cells().len()));
            !columns.is_empty()
                && columns.start < footprint.end
                && columns.end > footprint.start
                && row
                    .sized_text_at(part.column)
                    .is_some_and(|fragment| fragment.belongs_to(part))
        })
    }

    fn columns(&self, index: usize, limit: usize) -> Range<usize> {
        let start = if index == self.start.row {
            self.start.column
        } else {
            0
        };
        let end = if index == self.end.row {
            self.end.column.min(limit)
        } else {
            limit
        };
        start..end
    }

    fn text_columns(&self, index: usize, row: &Row, limit: usize) -> Range<usize> {
        let columns = self.columns(index, limit);
        if TRIM_ERASED {
            row.written_columns(columns).unwrap_or(0..0)
        } else {
            columns
        }
    }
}

impl<const TRIM_ERASED: bool> fmt::Display for TextRange<'_, TRIM_ERASED> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        for index in self.start.row..=self.end.row {
            let row = self.view.row(index).expect("Range endpoints are validated");
            let wrap = row.wrap_end();
            let columns = self.text_columns(index, row, wrap.unwrap_or(row.cells().len()));
            row.write_text(columns, output, |part| {
                let first = index.saturating_sub(part.row_offset).max(self.start.row);
                !self.overlaps_sized(first..index, part)
            })?;
            if index != self.end.row && wrap.is_none() {
                output.write_char('\n')?;
            }
        }
        Ok(())
    }
}
