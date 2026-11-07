use std::collections::VecDeque;

use crate::{Grid, Row, Screen, StorageError, StyleId};

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

/// A bounded chronological collection of immutable physical rows.
///
/// [`Self::scroll_up`] captures one row during an explicit primary-screen,
/// full-region scroll. It transfers row ownership, preserving cell and text
/// allocations, styles, erased cells, and outgoing soft-wrap metadata. History
/// rows retain their captured widths. Reads borrow their original storage.
///
/// Oldest rows are evicted to satisfy both limits. An oversized incoming row is
/// discarded and disconnects the preceding history row from the new screen top.
/// Evicted cell buffers are reused for the exposed row when their capacity fits.
/// Otherwise one blank replacement row is prepared before any logical mutation.
/// Queue growth and replacement allocation failures preserve logical history and
/// screen state. Preparation may retain additional reserved queue capacity.
///
/// The byte budget includes allocated capacities for cells, sparse text entries,
/// and UTF-8 payloads. Queue allocation and a temporary grid-width replacement
/// row are separate, bounded costs. Clear and limit reductions release row-owned
/// storage; the queue retains its capacity. Construction makes no allocations.
///
/// Pair this history with one screen. External changes to the primary screen's
/// top-row adjacency require [`Self::disconnect`]; pass [`Self::style_ids`] as
/// external roots to [`crate::Processor::collect_styles`]. Stateful text writers
/// require a boundary before this explicit screen mutation.
#[derive(Debug, Default)]
pub struct RowHistory {
    rows: VecDeque<Entry>,
    bytes: usize,
    limits: HistoryLimits,
}

impl RowHistory {
    pub fn new(limits: HistoryLimits) -> Result<Self, StorageError> {
        limits.validate()?;
        Ok(Self {
            limits,
            ..Self::default()
        })
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

    /// Style owners for collection while these rows remain retained.
    pub fn style_ids(&self) -> impl Iterator<Item = StyleId> + '_ {
        self.iter_rows()
            .flat_map(|row| row.cells().iter().map(|cell| cell.style()))
    }

    /// Changes limits, evicting oldest rows until both are satisfied. Validation
    /// fails before mutation. This makes no allocation requests.
    pub fn set_limits(&mut self, limits: HistoryLimits) -> Result<(), StorageError> {
        limits.validate()?;
        self.limits = limits;
        while self.rows.len() > limits.max_rows || self.bytes > limits.max_bytes {
            self.evict();
        }
        Ok(())
    }

    /// Releases all retained rows while preserving the queue allocation and limits.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.bytes = 0;
    }

    /// Ends the newest row's soft-wrap connection to the primary screen top.
    pub fn disconnect(&mut self) {
        if let Some(entry) = self.rows.back_mut() {
            entry.row.set_wrap_end(None);
        }
    }

    /// Scrolls one row of the screen's active region, preserving cursor placement,
    /// pending wrapping, modes, and cursor save slots. Primary full-region scrolling
    /// retains the outgoing row when it fits. Alternate and partial-region scrolling
    /// use the screen's regular recycling rules. Returns whether a row was retained.
    /// Every error preserves history and both display buffers.
    #[inline]
    pub fn scroll_up(&mut self, screen: &mut Screen, style: StyleId) -> Result<bool, StorageError> {
        if self.limits.max_rows == 0 || self.limits.max_bytes == 0 {
            screen.scroll_up(1, style);
            return Ok(false);
        }
        screen.scroll_history(self, style)
    }

    pub(crate) fn capture(
        &mut self,
        grid: &mut Grid,
        style: StyleId,
    ) -> Result<bool, StorageError> {
        let enabled = self.limits.max_rows != 0 && self.limits.max_bytes != 0;
        let bytes = if enabled {
            grid.row(0).expect("A grid contains a row").retained_bytes()
        } else {
            0
        };
        if !enabled || bytes > self.limits.max_bytes {
            self.disconnect();
            grid.scroll_up(0..grid.rows(), 1, style)
                .expect("The full grid is a valid region");
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
        if evicted == 0 {
            self.reserve_entry()?;
        }
        let columns = grid.columns();
        let reuse = (0..evicted).find(|&index| self.rows[index].row.fits_columns(columns));
        let mut replacement = if reuse.is_some() {
            None
        } else {
            Some(Row::new(columns, style)?)
        };
        for index in 0..evicted {
            let entry = self.evict();
            if reuse == Some(index) {
                let mut row = entry.row;
                row.clear(style);
                row.resize_prepared(columns, style);
                replacement = Some(row);
            }
        }
        if self
            .rows
            .back()
            .is_some_and(|entry| entry.row.cells().len() != columns)
        {
            self.disconnect();
        }
        let row = grid.exchange_top(replacement.expect("The replacement row is prepared"));
        self.rows.push_back(Entry { row, bytes });
        self.bytes += bytes;
        Ok(true)
    }

    fn evict(&mut self) -> Entry {
        let entry = self.rows.pop_front().expect("A retained row is present");
        self.bytes -= entry.bytes;
        entry
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
