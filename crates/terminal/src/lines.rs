use std::{iter::FusedIterator, ops::Range};

use crate::{
    CellPosition, Cursor, ReflowAffinity, ReflowAnchor, ReflowRows, Row, ScreenView, StorageError,
    TextRange,
};

impl<'a> ScreenView<'a> {
    /// Groups retained rows connected by automatic wrapping. Hard row breaks
    /// separate groups. The oldest group may be a fragment after history eviction.
    /// Construction uses constant space; traversal visits each physical row once
    /// without allocating, including when alternating traversal directions.
    #[inline]
    pub fn logical_lines(&self) -> LogicalLines<'a> {
        LogicalLines {
            view: *self,
            remaining: 0..self.rows(),
        }
    }

    /// Finds the retained logical line containing a chronological row index.
    /// Returns `None` outside this view. Work is proportional to the number of
    /// physical rows in the returned line; auxiliary space is constant.
    pub fn logical_line(&self, row: usize) -> Option<LogicalLine<'a>> {
        (row < self.rows()).then(|| LogicalLine {
            view: *self,
            start: line_start(*self, 0, row),
            end: line_end(*self, row + 1, self.rows()),
        })
    }
}

/// A borrowed, nonempty group of retained rows connected by automatic wrapping.
/// Coordinates use the source [`ScreenView`]. View edges bound the group even
/// when history eviction has removed its original prefix. Captured row widths,
/// cell styles, erased state, and owned text remain available through its rows.
#[derive(Clone, Copy, Debug)]
pub struct LogicalLine<'a> {
    view: ScreenView<'a>,
    start: usize,
    end: usize,
}

impl<'a> LogicalLine<'a> {
    /// Validates a source position in this retained line and normalizes connected
    /// row ends and wrap gaps. The borrowed anchor can map into several layouts
    /// without repeating coordinate validation. Construction is constant-time,
    /// uses constant space, and makes no allocation requests.
    pub fn anchor(&self, source: CellPosition) -> Result<ReflowAnchor<'a>, StorageError> {
        if !(self.start..self.end).contains(&source.row) {
            return Err(StorageError::OutOfBounds);
        }
        Ok(ReflowAnchor {
            view: self.view,
            position: crate::reflow::normalize_position(self.view, source)?,
        })
    }

    /// Maps a chronological source position into a layout at `columns` width.
    /// Destination row indices are relative to this logical line. Source columns
    /// may address continuations or a row's exclusive end. Connected source row
    /// ends and excluded gaps join the next row's start. `affinity` chooses a side
    /// of a shared output boundary. Final ends can equal the target width.
    /// Positions outside this retained line fail. Width validation follows
    /// [`Self::reflow`]; lookup visits the layout prefix through the mapped row,
    /// with constant auxiliary space and no allocation requests.
    pub fn map_position(
        &self,
        columns: usize,
        source: CellPosition,
        affinity: ReflowAffinity,
    ) -> Result<CellPosition, StorageError> {
        let source = self.anchor(source)?.position;
        for (index, row) in self.reflow(columns)?.enumerate() {
            if let Some(column) = row.map_normalized_position(source) {
                if affinity == ReflowAffinity::After
                    && row.wrap_end().is_some()
                    && column == row.used_columns()
                {
                    continue;
                }
                return Ok(CellPosition { row: index, column });
            }
        }
        Err(StorageError::OutOfBounds)
    }

    /// Maps a cursor whose source row uses chronological view coordinates.
    /// Pending wrapping requires the source right-edge cell and maps the boundary
    /// after it with preceding-row affinity. Other cursors use following-row
    /// affinity. Destination rows are relative to this logical line. A destination
    /// right-edge boundary becomes its last cell with wrapping pending; other
    /// boundaries clear pending wrapping. Invalid source cursor coordinates fail.
    /// Complexity and allocation behavior follow [`Self::map_position`].
    pub fn map_cursor(&self, columns: usize, cursor: Cursor) -> Result<Cursor, StorageError> {
        let row = self.view.row(cursor.row).ok_or(StorageError::OutOfBounds)?;
        if cursor.column >= row.cells().len()
            || (cursor.wrap_pending && cursor.column + 1 != row.cells().len())
        {
            return Err(StorageError::OutOfBounds);
        }
        let source = CellPosition {
            row: cursor.row,
            column: cursor.column + usize::from(cursor.wrap_pending),
        };
        let affinity = if cursor.wrap_pending {
            ReflowAffinity::Before
        } else {
            ReflowAffinity::After
        };
        let destination = self.map_position(columns, source, affinity)?;
        let wrap_pending = destination.column == columns;
        Ok(Cursor {
            row: destination.row,
            column: destination.column - usize::from(wrap_pending),
            wrap_pending,
        })
    }

    /// Lays out retained units at a target width from 1 through [`crate::MAX_DIMENSION`].
    /// All spaces and cell metadata are retained; source soft-wrap gaps are omitted.
    /// A one-column target containing a double-width unit fails before traversal.
    /// Validation is constant-time for widths of at least two, and scans source
    /// cells for a one-column target. Layout work is proportional to source rows
    /// and output rows, with constant auxiliary space and no allocation requests.
    /// The immutable borrow fixes the source.
    pub fn reflow(&self, columns: usize) -> Result<ReflowRows<'a>, StorageError> {
        ReflowRows::new(self.view, self.range(), columns)
    }

    /// Exclusive physical row range in the source view.
    #[inline]
    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }

    pub fn iter_rows(
        &self,
    ) -> impl ExactSizeIterator<Item = &'a Row> + DoubleEndedIterator + FusedIterator + use<'a>
    {
        self.view
            .iter_rows()
            .skip(self.start)
            .take(self.end - self.start)
    }

    /// Borrows all retained text in this line. Formatting omits soft-wrap gaps,
    /// preserves spaces, and emits no final newline. Output sink failure and
    /// complete-unit rules follow [`ScreenView::text`].
    pub fn text(&self) -> TextRange<'a> {
        let last = self.end - 1;
        self.view
            .text(
                CellPosition {
                    row: self.start,
                    column: 0,
                }..CellPosition {
                    row: last,
                    column: self
                        .view
                        .row(last)
                        .expect("A line row exists")
                        .cells()
                        .len(),
                },
            )
            .expect("A logical line has valid text boundaries")
    }
}

/// Double-ended traversal of retained logical lines. Each physical row is
/// examined at most once. The upper size hint counts remaining physical rows;
/// the exact number of logical lines requires traversing their boundaries.
#[derive(Clone, Debug)]
pub struct LogicalLines<'a> {
    view: ScreenView<'a>,
    remaining: Range<usize>,
}

impl<'a> Iterator for LogicalLines<'a> {
    type Item = LogicalLine<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let start = self.remaining.start;
        // Keep single-row groups inline and scan longer groups separately.
        let end = if start + 1 == self.remaining.end
            || self
                .view
                .row(start)
                .expect("A line row exists")
                .wrap_end()
                .is_none()
        {
            start + 1
        } else {
            line_end(self.view, start + 2, self.remaining.end)
        };
        self.remaining.start = end;
        Some(LogicalLine {
            view: self.view,
            start,
            end,
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let rows = self.remaining.len();
        (usize::from(rows != 0), Some(rows))
    }
}

impl DoubleEndedIterator for LogicalLines<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let end = self.remaining.end;
        let start = if end - 1 == self.remaining.start
            || self
                .view
                .row(end - 2)
                .expect("A line row exists")
                .wrap_end()
                .is_none()
        {
            end - 1
        } else {
            line_start(self.view, self.remaining.start, end - 2)
        };
        self.remaining.end = start;
        Some(LogicalLine {
            view: self.view,
            start,
            end,
        })
    }
}

impl FusedIterator for LogicalLines<'_> {}

fn line_start(view: ScreenView<'_>, lower: usize, mut row: usize) -> usize {
    while row > lower
        && view
            .row(row - 1)
            .expect("A line row exists")
            .wrap_end()
            .is_some()
    {
        row -= 1;
    }
    row
}

fn line_end(view: ScreenView<'_>, mut end: usize, upper: usize) -> usize {
    while end < upper
        && view
            .row(end - 1)
            .expect("A line row exists")
            .wrap_end()
            .is_some()
    {
        end += 1;
    }
    end
}
