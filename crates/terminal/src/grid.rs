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

    /// Scrolls an exclusive row range upward, clearing exposed rows with `style`.
    /// Empty ranges and zero lines leave contents unchanged. A count at least as
    /// large as the range clears it. Invalid ranges fail before any mutation.
    /// Row buffers are recycled without allocating.
    pub fn scroll_up(
        &mut self,
        region: Range<usize>,
        lines: usize,
        style: StyleId,
    ) -> Result<(), StorageError> {
        self.validate_region(&region)?;
        let height = region.len();
        if lines >= height {
            self.clear_rows(region, style);
        } else if region.start == 0 && region.end == self.rows.len() {
            for _ in 0..lines {
                let mut row = self.rows.pop_front().expect("The grid contains rows");
                row.clear(style);
                self.rows.push_back(row);
            }
        } else if lines != 0 {
            self.rotate_rows_left(region.clone(), lines);
            self.clear_rows(region.end - lines..region.end, style);
        }
        Ok(())
    }

    /// Scrolls an exclusive row range downward with the same boundary and
    /// allocation rules as [`Self::scroll_up`].
    pub fn scroll_down(
        &mut self,
        region: Range<usize>,
        lines: usize,
        style: StyleId,
    ) -> Result<(), StorageError> {
        self.validate_region(&region)?;
        let height = region.len();
        if lines >= height {
            self.clear_rows(region, style);
        } else if region.start == 0 && region.end == self.rows.len() {
            for _ in 0..lines {
                let mut row = self.rows.pop_back().expect("The grid contains rows");
                row.clear(style);
                self.rows.push_front(row);
            }
        } else if lines != 0 {
            self.rotate_rows_left(region.clone(), height - lines);
            self.clear_rows(region.start..region.start + lines, style);
        }
        Ok(())
    }

    fn validate_region(&self, region: &Range<usize>) -> Result<(), StorageError> {
        if region.start > region.end || region.end > self.rows.len() {
            return Err(StorageError::OutOfBounds);
        }
        Ok(())
    }

    fn clear_rows(&mut self, region: Range<usize>, style: StyleId) {
        for row in self.rows.range_mut(region) {
            row.clear(style);
        }
    }

    fn rotate_rows_left(&mut self, region: Range<usize>, lines: usize) {
        let (front, back) = self.rows.as_mut_slices();
        if region.end <= front.len() {
            front[region].rotate_left(lines);
            return;
        }
        if region.start >= front.len() {
            back[region.start - front.len()..region.end - front.len()].rotate_left(lines);
            return;
        }

        let split = front.len();
        let front = &mut front[region.start..];
        let back = &mut back[..region.end - split];
        let right = region.len() - lines;
        let short = lines.min(right);
        if short <= front.len().min(back.len()) {
            // Rotate each physical slice, then exchange the boundary blocks.
            if lines <= right {
                front.rotate_left(short);
                back.rotate_left(short);
                let front_start = front.len() - short;
                let back_start = back.len() - short;
                front[front_start..].swap_with_slice(&mut back[back_start..]);
            } else {
                front.rotate_right(short);
                back.rotate_right(short);
                front[..short].swap_with_slice(&mut back[..short]);
            }
        } else {
            // Three reversals handle a displacement across the shorter slice.
            self.reverse_rows(region.start..region.start + lines);
            self.reverse_rows(region.start + lines..region.end);
            self.reverse_rows(region);
        }
    }

    fn reverse_rows(&mut self, region: Range<usize>) {
        let (front, back) = self.rows.as_mut_slices();
        if region.end <= front.len() {
            front[region].reverse();
        } else if region.start >= front.len() {
            back[region.start - front.len()..region.end - front.len()].reverse();
        } else {
            let split = front.len();
            let front = &mut front[region.start..];
            let back = &mut back[..region.end - split];
            let pairs = front.len().min(back.len());
            for (left, right) in front.iter_mut().zip(back.iter_mut().rev()) {
                std::mem::swap(left, right);
            }
            front[pairs..].reverse();
            let remaining = back.len() - pairs;
            back[..remaining].reverse();
        }
    }

    /// Preserves the upper-left intersection and fills newly exposed cells with
    /// spaces using `style`. A wide unit cut by the right edge is cleared.
    /// Cell buffers and row-queue storage retain capacity for subsequent growth.
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
