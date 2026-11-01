use crate::{CellWidth, Grid, StorageError, StyleId};

/// A zero-based cursor snapshot. Columns may address wide-unit continuations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
    /// The next display unit starts on the next line.
    pub wrap_pending: bool,
}

/// Screen storage with cursor placement and delayed automatic wrapping.
/// Callers supply complete display units, their widths, and style keys.
#[derive(Debug)]
pub struct Screen {
    grid: Grid,
    cursor: Cursor,
}

impl Screen {
    pub fn new(columns: usize, rows: usize) -> Result<Self, StorageError> {
        Ok(Self {
            grid: Grid::new(columns, rows)?,
            cursor: Cursor::default(),
        })
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Sets an absolute cell position and cancels pending wrapping.
    /// Invalid coordinates preserve the entire screen state.
    pub fn set_cursor(&mut self, row: usize, column: usize) -> Result<(), StorageError> {
        if row >= self.grid.rows() || column >= self.grid.columns() {
            return Err(StorageError::OutOfBounds);
        }
        self.cursor = Cursor {
            row,
            column,
            wrap_pending: false,
        };
        Ok(())
    }

    /// Writes a complete display unit, then advances by its supplied width.
    /// Filling the right edge leaves the cursor in the last column with wrapping
    /// pending. A double-width unit that cannot fit wraps as a whole; the split
    /// unit at the old right edge is cleared using `style`.
    /// Units wider than the grid fail. Every error preserves contents and cursor.
    pub fn write(
        &mut self,
        text: &str,
        width: CellWidth,
        style: StyleId,
    ) -> Result<(), StorageError> {
        let columns = self.grid.columns();
        let width_columns = width.columns();
        if width_columns > columns {
            return Err(StorageError::OutOfBounds);
        }
        let gap = !self.cursor.wrap_pending && self.cursor.column + width_columns > columns;
        let wrap = self.cursor.wrap_pending || gap;
        let (row, column) = if wrap {
            let row = (self.cursor.row + 1).min(self.grid.rows() - 1);
            if self.cursor.row + 1 == self.grid.rows() {
                self.grid.write_scrolling(text, width, style, gap)?;
            } else {
                self.grid.write(row, 0, text, width, style)?;
                if gap {
                    self.grid
                        .clear_row(self.cursor.row, columns - 1..columns, style)
                        .expect("The last column is in bounds");
                }
            }
            (row, 0)
        } else {
            self.grid
                .write(self.cursor.row, self.cursor.column, text, width, style)?;
            (self.cursor.row, self.cursor.column)
        };
        self.advance(row, column + width_columns);
        Ok(())
    }

    /// Writes printable ASCII in row-sized batches. The entire input is validated
    /// before mutation. Empty input preserves pending wrapping. This does not allocate.
    pub fn write_ascii(&mut self, mut bytes: &[u8], style: StyleId) -> Result<(), StorageError> {
        if bytes.iter().any(|&byte| !(b' '..=b'~').contains(&byte)) {
            return Err(StorageError::InvalidAscii);
        }
        while !bytes.is_empty() {
            if self.cursor.wrap_pending {
                self.line_feed(style);
                self.cursor.column = 0;
            }
            let length = bytes.len().min(self.grid.columns() - self.cursor.column);
            self.grid.write_ascii_validated(
                self.cursor.row,
                self.cursor.column,
                &bytes[..length],
                style,
            );
            self.advance(self.cursor.row, self.cursor.column + length);
            bytes = &bytes[length..];
        }
        Ok(())
    }

    /// Moves to column zero and cancels pending wrapping.
    pub fn carriage_return(&mut self) {
        self.cursor.column = 0;
        self.cursor.wrap_pending = false;
    }

    /// Advances one row, scrolling the full grid at the bottom. The column is
    /// preserved and pending wrapping is canceled. Exposed cells use `style`.
    pub fn line_feed(&mut self, style: StyleId) {
        if self.cursor.row + 1 == self.grid.rows() {
            self.grid
                .scroll_up(0..self.grid.rows(), 1, style)
                .expect("The full grid is a valid row range");
        } else {
            self.cursor.row += 1;
        }
        self.cursor.wrap_pending = false;
    }

    /// Moves one column left, stopping at column zero, and cancels pending wrapping.
    pub fn backspace(&mut self) {
        self.cursor.column = self.cursor.column.saturating_sub(1);
        self.cursor.wrap_pending = false;
    }

    /// Preserves the upper-left grid intersection and clamps cursor coordinates.
    /// A size change cancels pending wrapping; an unchanged size preserves it.
    /// Every error preserves both contents and cursor.
    pub fn resize(
        &mut self,
        columns: usize,
        rows: usize,
        style: StyleId,
    ) -> Result<(), StorageError> {
        if columns == self.grid.columns() && rows == self.grid.rows() {
            return Ok(());
        }
        self.grid.resize(columns, rows, style)?;
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.column = self.cursor.column.min(columns - 1);
        self.cursor.wrap_pending = false;
        Ok(())
    }

    fn advance(&mut self, row: usize, end: usize) {
        self.cursor = Cursor {
            row,
            column: end.min(self.grid.columns() - 1),
            wrap_pending: end == self.grid.columns(),
        };
    }
}
