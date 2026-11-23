use std::{collections::VecDeque, sync::Arc};

use crate::row::PreparedRow;
use crate::sizing::SizedBlock;
use crate::{Cell, CellWidth, Grid, Row, StorageError, StyleId};

/// Maximum retained physical rows.
pub const MAX_HISTORY_ROWS: usize = u16::MAX as usize;
/// Maximum owned cell, text-index, and text capacity retained in history.
pub const MAX_HISTORY_BYTES: usize = 64 << 20;

/// Independent bounds on retained row count and row-owned heap capacity.
/// Either zero disables capture. Queue storage follows the high-water row limit
/// and retains its allocation after limit reductions, eviction, or clearing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistoryLimits {
    pub max_rows: usize,
    pub max_bytes: usize,
}

impl HistoryLimits {
    fn validate(self) -> Result<(), StorageError> {
        if self.max_rows > MAX_HISTORY_ROWS || self.max_bytes > MAX_HISTORY_BYTES {
            Err(StorageError::CapacityExceeded)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
struct Entry {
    row: Row,
    bytes: usize,
}

/// Bounded chronological physical rows owned by a [`crate::Screen`].
///
/// Upward scrolling from the primary screen top transfers row ownership, including text,
/// styles, erased cells, and outgoing soft wraps. Rows retain their captured widths.
/// Configure capture with [`crate::Screen::set_history_limits`] and borrow rows
/// through [`crate::Screen::history`]. Capture starts disabled.
///
/// Oldest rows are evicted to satisfy both limits. An oversized incoming row is
/// discarded and disconnects the preceding row from the screen top. Fitting
/// evicted buffers supply replacement rows. Fallible preparation preserves logical
/// history and screen state; reserved queue capacity may grow during preparation.
///
/// The byte budget counts cell, sparse-index, and UTF-8 allocated capacities.
/// Queue and transaction metadata retain their high-water capacity. A text batch
/// can reserve up to `max_rows` blank buffers. Newly allocated cell capacity is
/// bounded by `max_bytes`; recycled buffers retain their existing capacities.
/// Spare rows are released when the batch ends.
/// Screen reset clears history while preserving configured limits and metadata.
#[derive(Debug, Default)]
pub struct RowHistory {
    rows: VecDeque<Entry>,
    revision: u64,
    bytes: usize,
    limits: HistoryLimits,
    // Preflight replacements for a whole atomic ASCII callback. Evicted buffers
    // can replenish this pool when byte limits evict several rows at once.
    spares: Vec<Row>,
    remaining: usize,
    // Cache fitting retained buffers so saturated preflight avoids a history scan.
    warm_columns: usize,
    warm_count: usize,
}

impl RowHistory {
    /// Appends to an unpublished contiguous reflow suffix. An unretainable row
    /// releases the preceding suffix. `bytes` includes reserved source text
    /// capacities that will move during commit. Discard the history on error.
    pub(crate) fn append_reflow(&mut self, row: Row, bytes: usize) -> Result<bool, StorageError> {
        if self.limits.max_rows == 0 || bytes > self.limits.max_bytes {
            self.clear();
            return Ok(false);
        }
        if self.warm_columns == 0 {
            self.warm_columns = row.cells().len();
            self.warm_count = self
                .rows
                .iter()
                .filter(|entry| entry.row.fits_columns(self.warm_columns))
                .count();
        }
        let warm = row.fits_columns(self.warm_columns);
        while self.rows.len() >= self.limits.max_rows || self.bytes > self.limits.max_bytes - bytes
        {
            self.evict();
        }
        self.reserve_entry()?;
        self.bytes += bytes;
        self.rows.push_back(Entry { row, bytes });
        self.warm_count += usize::from(warm);
        Ok(true)
    }

    pub(crate) fn row_mut(&mut self, index: usize) -> Option<&mut Row> {
        self.rows.get_mut(index).map(|entry| &mut entry.row)
    }

    pub(crate) fn replace_sized_block(
        &mut self,
        index: usize,
        column: usize,
        block: Arc<SizedBlock>,
    ) {
        let entry = &mut self.rows[index];
        let previous = entry
            .row
            .sized_text_at(column)
            .expect("The sized column exists")
            .retained_bytes();
        let replacement = block.retained_bytes();
        entry.row.replace_sized_block(column, block);
        entry.bytes = entry.bytes - previous + replacement;
        self.bytes = self.bytes - previous + replacement;
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn limits(&self) -> HistoryLimits {
        self.limits
    }
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(crate) fn revision(&self) -> Option<u64> {
        (!self.rows.is_empty()).then_some(self.revision)
    }

    pub(crate) fn commit_reflow(&mut self, mut history: Self) {
        history.revision = self.revision.wrapping_add(1);
        *self = history;
    }

    /// Allocated row-owned heap capacity, excluding the queue allocation.
    pub fn retained_bytes(&self) -> usize {
        self.bytes
    }

    /// Borrows a retained row, with zero indexing the oldest row.
    pub fn row(&self, index: usize) -> Option<&Row> {
        self.rows.get(index).map(|entry| &entry.row)
    }

    pub fn iter_rows(&self) -> impl ExactSizeIterator<Item = &Row> + DoubleEndedIterator {
        self.rows.iter().map(|entry| &entry.row)
    }

    pub(crate) fn placeholder_rows(&self) -> impl Iterator<Item = (usize, &Row)> {
        self.rows.iter().enumerate().filter_map(|(index, entry)| {
            entry
                .row
                .has_graphics_placeholder()
                .then_some((index, &entry.row))
        })
    }

    /// Style owners for collection while these rows remain retained.
    pub fn style_ids(&self) -> impl Iterator<Item = StyleId> + '_ {
        self.iter_rows()
            .flat_map(|row| row.cells().iter().map(|cell| cell.style()))
    }

    /// Changes limits, evicting oldest rows until both are satisfied. Validation
    /// fails before mutation. This makes no allocation requests.
    pub(crate) fn set_limits(&mut self, limits: HistoryLimits) -> Result<(), StorageError> {
        limits.validate()?;
        self.limits = limits;
        let before = self.rows.len();
        while self.rows.len() > limits.max_rows || self.bytes > limits.max_bytes {
            self.evict().erase_sized();
        }
        if self.rows.len() != before {
            self.revision = self.revision.wrapping_add(1);
        }
        Ok(())
    }

    /// Releases all retained rows while preserving the queue allocation and limits.
    pub(crate) fn clear(&mut self) {
        self.finish();
        self.warm_count = 0;
        if !self.rows.is_empty() {
            self.revision = self.revision.wrapping_add(1);
        }
        self.rows.clear();
        self.bytes = 0;
    }

    /// Ends the newest row's soft-wrap connection to the primary screen top.
    pub(crate) fn disconnect(&mut self) {
        if let Some(entry) = self.rows.back_mut() {
            entry.row.set_wrap_end(None);
        }
    }

    pub(crate) fn enabled_for(&self, columns: usize) -> bool {
        self.limits.max_rows != 0 && self.limits.max_bytes / size_of::<Cell>() >= columns
    }

    pub(crate) fn prepare(
        &mut self,
        columns: usize,
        scrolls: usize,
        style: StyleId,
    ) -> Result<bool, StorageError> {
        if scrolls == 0 || self.remaining != 0 {
            return Ok(false);
        }
        if self.warm_columns != columns {
            self.warm_columns = columns;
            self.warm_count = self
                .rows
                .iter()
                .filter(|entry| entry.row.fits_columns(columns))
                .count();
        }
        let maximum = self
            .limits
            .max_rows
            .min(self.limits.max_bytes / (columns * size_of::<Cell>()));
        let new_rows = scrolls.min(maximum.saturating_sub(self.warm_count));
        let cache = new_rows.max(scrolls.saturating_sub(1).min(maximum));
        let slots = self
            .rows
            .len()
            .saturating_add(scrolls)
            .min(self.limits.max_rows);
        self.rows
            .try_reserve_exact(slots.saturating_sub(self.rows.len()))?;
        self.spares
            .try_reserve_exact(cache.saturating_sub(self.spares.len()))?;
        for _ in 0..new_rows {
            match Row::new(columns, style) {
                Ok(row) => self.spares.push(row),
                Err(error) => {
                    self.spares.clear();
                    return Err(error);
                }
            }
        }
        self.remaining = scrolls;
        Ok(true)
    }

    pub(crate) fn finish(&mut self) {
        self.spares.clear();
        self.remaining = 0;
    }

    fn finish_scroll(&mut self) {
        if self.remaining != 0 {
            self.remaining -= 1;
            self.spares.truncate(self.remaining);
        }
    }

    pub(crate) fn capture(
        &mut self,
        grid: &mut Grid,
        end: usize,
        style: StyleId,
    ) -> Result<bool, StorageError> {
        self.capture_with(grid, end, style, None, None, false)
    }

    pub(crate) fn write_scrolling(
        &mut self,
        grid: &mut Grid,
        end: usize,
        text: &str,
        width: CellWidth,
        style: StyleId,
        gap: bool,
    ) -> Result<bool, StorageError> {
        self.capture_with(
            grid,
            end,
            style,
            Some((text, width)),
            Some(grid.columns() - usize::from(gap)),
            gap,
        )
    }

    fn capture_with(
        &mut self,
        grid: &mut Grid,
        end: usize,
        style: StyleId,
        unit: Option<(&str, CellWidth)>,
        wrap: Option<usize>,
        gap: bool,
    ) -> Result<bool, StorageError> {
        let columns = grid.columns();
        let top = grid.row(0).expect("A grid contains a row");
        let bytes = if gap && end == 1 {
            top.bytes_without_last_unit()
        } else {
            top.retained_bytes()
        };
        if !self.enabled_for(columns) || bytes > self.limits.max_bytes {
            if let Some((text, width)) = unit {
                grid.write_scrolling(0..end, text, width, style, gap)?;
            } else {
                grid.scroll_up(0..end, 1, style)
                    .expect("The top scrolling region is valid");
            }
            self.disconnect();
            self.finish_scroll();
            return Ok(false);
        }
        let mut remaining_bytes = self.bytes;
        let mut evicted = 0;
        while self.rows.len() - evicted >= self.limits.max_rows
            || remaining_bytes > self.limits.max_bytes - bytes
        {
            remaining_bytes -= self.rows[evicted].bytes;
            evicted += 1;
        }
        if evicted == 0 && self.remaining == 0 {
            self.reserve_entry()?;
        }
        let reuse = (0..evicted).find(|&index| self.rows[index].row.fits_columns(columns));
        let mut replacement = if let Some(index) = reuse {
            if let Some((text, width)) = unit {
                self.rows[index]
                    .row
                    .reset_write(columns, text, width, style)?;
            }
            None
        } else {
            let mut row = if self.remaining != 0 {
                self.spares
                    .pop()
                    .expect("Batch scrolling reserved a replacement row")
            } else {
                Row::new(columns, style)?
            };
            if let Some((text, width)) = unit {
                row.reset_write(columns, text, width, style)?;
            }
            Some(row)
        };
        let future = self.remaining.saturating_sub(1);
        for index in 0..evicted {
            let mut row = self.evict();
            row.erase_sized();
            if reuse == Some(index) {
                if unit.is_none() {
                    row.clear(style);
                    row.resize_prepared(columns, style);
                }
                replacement = Some(row);
            } else if self.remaining != 0 && self.spares.len() < future && row.fits_columns(columns)
            {
                row.clear(style);
                row.resize_prepared(columns, style);
                self.spares.push(row);
            }
        }
        if self
            .rows
            .back()
            .is_some_and(|entry| entry.row.cells().len() != columns)
        {
            self.disconnect();
        }
        if gap {
            grid.clear_row(end - 1, columns - 1..columns, style)
                .expect("The final cell is in bounds");
        }
        let mut row = grid.exchange_top(end, replacement.expect("A replacement row is prepared"));
        if end == 1 {
            row.set_wrap_end(wrap);
        }
        debug_assert_eq!(row.retained_bytes(), bytes);
        if self.warm_columns != 0 && row.fits_columns(self.warm_columns) {
            self.warm_count += 1;
        }
        self.rows.push_back(Entry { row, bytes });
        self.bytes += bytes;
        self.revision = self.revision.wrapping_add(1);
        if end > 1 && (wrap.is_some() || end != grid.rows()) {
            grid.set_wrap_end(end - 2, wrap);
        }
        self.finish_scroll();
        Ok(true)
    }

    pub(crate) fn restore(
        &mut self,
        grid: &mut Grid,
        end: usize,
        count: usize,
        blank: usize,
        style: StyleId,
    ) -> Result<(), StorageError> {
        let columns = grid.columns();
        let blank = blank.min(end);
        let retained = count.min(end - blank);
        let first = self.rows.len() - count;
        let mut widened = Vec::new();
        for (index, entry) in self.rows.range(first..first + retained).enumerate() {
            let source = &entry.row;
            if !source.fits_columns(columns) {
                let mut row = PreparedRow::new(columns, style)?;
                row.prepare_segment(0, source, 0..source.cells().len())?;
                widened.try_reserve(1)?;
                widened.push((index, row.finish_preparation().0));
            }
        }
        let resized_join = retained != 0 && self.rows[first].row.cells().len() != columns;

        let shift = blank + retained;
        grid.split_sized_text(end);
        if shift < end {
            grid.rotate_top_down(end, shift);
        }
        // Newer history rows beyond the final screen are scrolled out by this batch.
        for _ in retained..count {
            self.pop_recent().erase_sized();
        }
        for index in (0..retained).rev() {
            let mut source = self.pop_recent();
            let restored = if widened.last().is_some_and(|(target, _)| *target == index) {
                let (_, mut row) = widened.pop().expect("The widened row was prepared");
                let width = source.cells().len();
                row.transfer_segment(0, &mut source, 0..width);
                row
            } else {
                source.resize_prepared(columns, style);
                source
            };
            let target = grid.row_mut(blank + index).expect("A restored screen row");
            target.erase_sized();
            *target = restored;
        }
        grid.clear_rows(0..blank, style);
        grid.set_wrap_end(end - 1, None);
        if resized_join {
            self.disconnect();
        }
        if count != 0 {
            self.revision = self.revision.wrapping_add(1);
        }
        Ok(())
    }

    pub(crate) fn connect_latest(&mut self, end: usize) {
        if let Some(entry) = self.rows.back_mut() {
            entry.row.set_wrap_end(Some(end));
        }
    }

    fn evict(&mut self) -> Row {
        let entry = self.rows.pop_front().expect("A retained row is present");
        self.remove_entry(entry)
    }

    fn pop_recent(&mut self) -> Row {
        let entry = self.rows.pop_back().expect("A retained row is present");
        self.remove_entry(entry)
    }

    fn remove_entry(&mut self, entry: Entry) -> Row {
        self.bytes -= entry.bytes;
        if self.warm_columns != 0 && entry.row.fits_columns(self.warm_columns) {
            self.warm_count -= 1;
        }
        entry.row
    }

    fn reserve_entry(&mut self) -> Result<(), StorageError> {
        if self.rows.len() == self.rows.capacity() {
            let capacity = self
                .rows
                .capacity()
                .max(4)
                .saturating_mul(2)
                .min(self.limits.max_rows);
            self.rows.try_reserve_exact(capacity - self.rows.len())?;
        }
        Ok(())
    }
}
