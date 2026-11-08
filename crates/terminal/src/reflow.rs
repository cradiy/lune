use std::{iter::FusedIterator, ops::Range};

use crate::{CellPosition, MAX_DIMENSION, Row, ScreenView, StorageError, StyleId, TextRange};

/// A borrowed layout of one retained logical line at a new column width.
/// Rows preserve complete display units, printed spaces, erased cells and their
/// styles. Source soft-wrap gaps are omitted. Text stays in the source rows.
/// Traversal uses constant auxiliary space and makes no allocation requests.
/// Work is proportional to source rows and output rows. A cut inspects only its
/// boundary cell; source segments that fit are consumed together.
#[derive(Clone, Debug)]
pub struct ReflowRows<'a> {
    view: ScreenView<'a>,
    next: CellPosition,
    end: CellPosition,
    columns: usize,
}

impl<'a> ReflowRows<'a> {
    pub(crate) fn new(
        view: ScreenView<'a>,
        rows: Range<usize>,
        columns: usize,
    ) -> Result<Self, StorageError> {
        if columns == 0 {
            return Err(StorageError::InvalidDimensions);
        }
        if columns > MAX_DIMENSION {
            return Err(StorageError::CapacityExceeded);
        }
        // A one-column layout must reject wide units before exposing any rows.
        if columns == 1
            && view
                .iter_rows()
                .skip(rows.start)
                .take(rows.len())
                .any(|row| row.cells().iter().any(|cell| cell.width() == 2))
        {
            return Err(StorageError::OutOfBounds);
        }
        let last = rows.end - 1;
        Ok(Self {
            view,
            next: CellPosition {
                row: rows.start,
                column: 0,
            },
            end: CellPosition {
                row: last,
                column: view
                    .row(last)
                    .expect("A logical line row exists")
                    .cells()
                    .len(),
            },
            columns,
        })
    }
}

/// One output row of a borrowed logical-line layout.
/// Source coordinates refer to the complete chronological [`ScreenView`].
/// An early wide-unit wrap leaves one destination column available for padding.
#[derive(Clone, Copy, Debug)]
pub struct ReflowRow<'a> {
    view: ScreenView<'a>,
    start: CellPosition,
    end: CellPosition,
    used: usize,
    wrapped: bool,
    columns: usize,
}

impl<'a> ReflowRow<'a> {
    /// Builds an independently owned row at this layout's target width.
    /// Copies complete cells and owned text, fills unused columns with erased
    /// spaces in `padding_style`, and establishes this output row's soft wrap.
    /// Source rows remain borrowed and unchanged. Failure releases the partial
    /// destination. Row storage is bounded by the target width and per-unit text
    /// limit. Style IDs belong to their source table; include these cells as
    /// external roots when collecting styles while the row is retained separately.
    pub fn materialize(&self, padding_style: StyleId) -> Result<Row, StorageError> {
        let mut owned = Row::new(self.columns, padding_style)?;
        let mut column = 0;
        for index in self.start.row..=self.end.row {
            let source = self.view.row(index).expect("A reflow source row exists");
            let start = if index == self.start.row {
                self.start.column
            } else {
                0
            };
            let end = if index == self.end.row {
                self.end.column
            } else {
                source.wrap_end().unwrap_or(source.cells().len())
            };
            if start < end {
                owned.copy_segment(column, source, start..end)?;
                column += end - start;
            }
        }
        debug_assert_eq!(column, self.used);
        owned.set_wrap_end(self.wrap_end());
        Ok(owned)
    }

    /// Half-open source range of complete units. It can span source row edges
    /// and their excluded wrap gaps. Borrowing fixes all source coordinates.
    #[inline]
    pub fn source_range(&self) -> Range<CellPosition> {
        self.start..self.end
    }

    /// Columns occupied by retained units, including printed and erased spaces.
    #[inline]
    pub fn used_columns(&self) -> usize {
        self.used
    }

    /// Exclusive occupied column joining the next output row. The final output
    /// row returns `None`; an early wide-unit wrap excludes its padding column.
    #[inline]
    pub fn wrap_end(&self) -> Option<usize> {
        self.wrapped.then_some(self.used)
    }

    /// Streams source text with whole display units and source wrap gaps omitted.
    /// Destination padding and a final newline are supplied by the consumer.
    pub fn text(&self) -> TextRange<'a> {
        self.view
            .text(self.source_range())
            .expect("Reflow boundaries are valid")
    }
}

impl<'a> Iterator for ReflowRows<'a> {
    type Item = ReflowRow<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.end {
            return None;
        }
        let start = self.next;
        let mut used = 0;
        while self.next != self.end {
            let row = self
                .view
                .row(self.next.row)
                .expect("A logical line row exists");
            let end = row.wrap_end().unwrap_or(row.cells().len());
            let mut boundary = self.next.column + (end - self.next.column).min(self.columns - used);
            // A continuation at the cut moves its complete wide unit to the next row.
            if boundary < end && row.cells()[boundary].is_continuation() {
                boundary -= 1;
            }
            used += boundary - self.next.column;
            self.next.column = boundary;
            if boundary < end {
                return Some(ReflowRow {
                    view: self.view,
                    start,
                    end: self.next,
                    used,
                    wrapped: true,
                    columns: self.columns,
                });
            }
            if self.next.column == end && self.next.row != self.end.row {
                self.next.row += 1;
                self.next.column = 0;
            }
            if used == self.columns {
                break;
            }
        }
        Some(ReflowRow {
            view: self.view,
            start,
            end: self.next,
            used,
            wrapped: self.next != self.end,
            columns: self.columns,
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.next == self.end {
            (0, Some(0))
        } else {
            (1, None)
        }
    }
}

impl FusedIterator for ReflowRows<'_> {}
