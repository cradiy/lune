use std::{
    fmt::{self, Write},
    ops::Range,
};

use crate::{Grid, StorageError};

/// A physical row and a column boundary. Column `grid.columns()` is the row end.
/// Positions are ordered by row, then column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellPosition {
    pub row: usize,
    pub column: usize,
}

/// A validated, read-only text range borrowed from a grid.
/// [`fmt::Display`] streams complete display units and propagates sink failures.
/// A sink may retain output written before its failure. Formatting can be repeated;
/// the selected text is fixed for the lifetime of the borrow.
#[derive(Clone, Copy, Debug)]
pub struct GridText<'a> {
    grid: &'a Grid,
    start: CellPosition,
    end: CellPosition,
}

impl<'a> GridText<'a> {
    pub(crate) fn new(grid: &'a Grid, range: Range<CellPosition>) -> Result<Self, StorageError> {
        if range.start > range.end
            || range.start.row >= grid.rows()
            || range.end.row >= grid.rows()
            || range.start.column > grid.columns()
            || range.end.column > grid.columns()
        {
            return Err(StorageError::OutOfBounds);
        }
        Ok(Self {
            grid,
            start: range.start,
            end: range.end,
        })
    }
}

impl fmt::Display for GridText<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (offset, row) in self
            .grid
            .iter_rows()
            .skip(self.start.row)
            .take(self.end.row - self.start.row + 1)
            .enumerate()
        {
            let index = self.start.row + offset;
            let start = if index == self.start.row {
                self.start.column
            } else {
                0
            };
            let end = if index == self.end.row {
                self.end.column
            } else {
                self.grid.columns()
            };
            let wrap = row.wrap_end();
            row.write_text(start..end.min(wrap.unwrap_or(self.grid.columns())), output)?;
            if index != self.end.row && wrap.is_none() {
                output.write_char('\n')?;
            }
        }
        Ok(())
    }
}
