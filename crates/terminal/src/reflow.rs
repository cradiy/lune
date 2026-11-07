use std::{iter::FusedIterator, ops::Range};

use crate::{CellPosition, MAX_DIMENSION, ScreenView, StorageError, TextRange};

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
}

impl<'a> ReflowRow<'a> {
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
