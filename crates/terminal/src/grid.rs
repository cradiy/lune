use std::{collections::VecDeque, ops::Range};

use crate::{CellWidth, Row, StorageError, StyleId};

/// Maximum rows or columns in a grid.
pub const MAX_DIMENSION: usize = u16::MAX as usize;
/// Maximum logical base-cell count in a grid.
pub const MAX_CELLS: usize = 1 << 20;

/// Rectangular screen storage with reusable row buffers.
#[derive(Debug)]
pub struct Grid {
    columns: usize,
    rows: VecDeque<Row>,
}

impl Grid {
    pub fn new(columns: usize, rows: usize) -> Result<Self, StorageError> {
        validate_dimensions(columns, rows)?;
        let mut storage = VecDeque::new();
        storage.try_reserve_exact(rows)?;
        for _ in 0..rows {
            storage.push_back(Row::new(columns, StyleId::default())?);
        }
        Ok(Self {
            columns,
            rows: storage,
        })
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    pub fn row(&self, index: usize) -> Option<&Row> {
        self.rows.get(index)
    }

    pub fn iter_rows(&self) -> impl ExactSizeIterator<Item = &Row> + DoubleEndedIterator {
        self.rows.iter()
    }

    /// Writes one caller-defined display unit. Overlapping wide units are cleared
    /// completely, using `style` for any cleared cell outside the write range.
    pub fn write(
        &mut self,
        row: usize,
        column: usize,
        text: &str,
        width: CellWidth,
        style: StyleId,
    ) -> Result<(), StorageError> {
        self.row_mut(row)?.write(column, text, width, style)
    }

    /// Writes a printable ASCII run in one pass after validating the entire run.
    pub fn write_ascii(
        &mut self,
        row: usize,
        column: usize,
        bytes: &[u8],
        style: StyleId,
    ) -> Result<(), StorageError> {
        self.row_mut(row)?.write_ascii(column, bytes, style)
    }

    /// Appends text to a display unit while retaining its width and style.
    /// Addressing a continuation appends to its leading cell.
    pub fn append_text(
        &mut self,
        row: usize,
        column: usize,
        text: &str,
    ) -> Result<(), StorageError> {
        self.row_mut(row)?.append_text(column, text)
    }

    /// Clears a range, including complete wide units overlapping either boundary.
    pub fn clear_row(
        &mut self,
        row: usize,
        range: Range<usize>,
        style: StyleId,
    ) -> Result<(), StorageError> {
        self.row_mut(row)?.clear_range(range, style)
    }

    pub fn clear(&mut self, style: StyleId) {
        for row in &mut self.rows {
            row.clear(style);
        }
    }

    pub fn scroll_up(&mut self, lines: usize, style: StyleId) {
        if lines >= self.rows.len() {
            self.clear(style);
            return;
        }
        for _ in 0..lines {
            let mut row = self.rows.pop_front().expect("The grid contains rows");
            row.clear(style);
            self.rows.push_back(row);
        }
    }

    pub fn scroll_down(&mut self, lines: usize, style: StyleId) {
        if lines >= self.rows.len() {
            self.clear(style);
            return;
        }
        for _ in 0..lines {
            let mut row = self.rows.pop_back().expect("The grid contains rows");
            row.clear(style);
            self.rows.push_front(row);
        }
    }

    /// Preserves the upper-left intersection and fills newly exposed cells with
    /// spaces using `style`. A wide unit cut by the right edge is cleared.
    /// Row buffers retain capacity for subsequent growth.
    pub fn resize(
        &mut self,
        columns: usize,
        rows: usize,
        style: StyleId,
    ) -> Result<(), StorageError> {
        validate_dimensions(columns, rows)?;
        if columns == self.columns && rows == self.rows.len() {
            return Ok(());
        }

        let additional = rows.saturating_sub(self.rows.len());
        let mut added = Vec::new();
        added.try_reserve_exact(additional)?;
        for _ in 0..additional {
            added.push(Row::new(columns, style)?);
        }
        self.rows.try_reserve(additional)?;
        if columns > self.columns {
            for row in self.rows.iter_mut().take(rows) {
                row.reserve_columns(columns)?;
            }
        }

        self.rows.truncate(rows);
        for row in &mut self.rows {
            row.resize_prepared(columns, style);
        }
        self.rows.extend(added);
        self.columns = columns;
        Ok(())
    }

    fn row_mut(&mut self, row: usize) -> Result<&mut Row, StorageError> {
        self.rows.get_mut(row).ok_or(StorageError::OutOfBounds)
    }
}

fn validate_dimensions(columns: usize, rows: usize) -> Result<(), StorageError> {
    if columns == 0 || rows == 0 {
        return Err(StorageError::InvalidDimensions);
    }
    if columns > MAX_DIMENSION
        || rows > MAX_DIMENSION
        || columns
            .checked_mul(rows)
            .is_none_or(|cells| cells > MAX_CELLS)
    {
        return Err(StorageError::CapacityExceeded);
    }
    Ok(())
}
