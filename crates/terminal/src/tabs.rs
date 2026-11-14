use crate::StorageError;

const DEFAULT_STOPS: u64 = 0x0101_0101_0101_0101;

/// A bit per physical column. Padding bits stay clear, and capacity survives
/// shrinking so returning to a previously reserved width does not allocate.
#[derive(Debug)]
pub(crate) struct TabStops {
    words: Vec<u64>,
    // Exhausted requests can return before scanning sparse or empty words.
    total: usize,
}

impl TabStops {
    pub(crate) fn resized_copy(&self, old: usize, columns: usize) -> Result<Self, StorageError> {
        let count = columns.div_ceil(64);
        let mut words = Vec::new();
        words.try_reserve_exact(count)?;
        words.extend_from_slice(&self.words[..count.min(self.words.len())]);
        let mut copy = Self {
            words,
            total: self.total,
        };
        copy.resize_prepared(old, columns);
        Ok(copy)
    }

    pub(crate) fn new(columns: usize) -> Result<Self, StorageError> {
        let mut stops = Self {
            words: Vec::new(),
            total: 0,
        };
        stops.reserve_columns(columns)?;
        stops.resize_prepared(0, columns);
        Ok(stops)
    }

    pub(crate) fn reserve_columns(&mut self, columns: usize) -> Result<(), StorageError> {
        self.words
            .try_reserve_exact(columns.div_ceil(64).saturating_sub(self.words.len()))?;
        Ok(())
    }

    pub(crate) fn resize_prepared(&mut self, old: usize, columns: usize) {
        if old == columns {
            return;
        }
        self.words.resize(columns.div_ceil(64), DEFAULT_STOPS);
        if columns > old && !old.is_multiple_of(64) {
            self.words[old / 64] |= DEFAULT_STOPS & (u64::MAX << (old % 64));
        }
        if !columns.is_multiple_of(64) {
            *self.words.last_mut().unwrap() &= (1_u64 << (columns % 64)) - 1;
        }
        self.total = self
            .words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum();
    }

    pub(crate) fn contains(&self, column: usize) -> bool {
        self.words[column / 64] & (1_u64 << (column % 64)) != 0
    }

    pub(crate) fn set(&mut self, column: usize) {
        let word = &mut self.words[column / 64];
        let mask = 1_u64 << (column % 64);
        self.total += usize::from(*word & mask == 0);
        *word |= mask;
    }

    pub(crate) fn clear(&mut self, column: usize) {
        let word = &mut self.words[column / 64];
        let mask = 1_u64 << (column % 64);
        self.total -= usize::from(*word & mask != 0);
        *word &= !mask;
    }

    pub(crate) fn clear_all(&mut self) {
        self.words.fill(0);
        self.total = 0;
    }

    /// Restores default stops within the current logical width, retaining capacity.
    pub(crate) fn reset(&mut self, columns: usize) {
        self.words.fill(DEFAULT_STOPS);
        if !columns.is_multiple_of(64) {
            *self.words.last_mut().unwrap() &= (1_u64 << (columns % 64)) - 1;
        }
        self.total = columns.div_ceil(8);
    }

    pub(crate) fn forward(&self, column: usize, count: usize, columns: usize) -> usize {
        let last = columns - 1;
        if count > self.total || count >= last - column {
            return last;
        }
        let start = column + 1;
        let mut remaining = count;
        for (index, &word) in self.words.iter().enumerate().skip(start / 64) {
            let mut word = word;
            if index == start / 64 {
                word &= u64::MAX << (start % 64);
            }
            if word == 0 {
                continue;
            }
            if remaining == 1 {
                return index * 64 + word.trailing_zeros() as usize;
            }
            let available = word.count_ones() as usize;
            if remaining <= available {
                for _ in 1..remaining {
                    word &= word - 1;
                }
                return index * 64 + word.trailing_zeros() as usize;
            }
            remaining -= available;
        }
        last
    }

    pub(crate) fn backward(&self, column: usize, count: usize) -> usize {
        if count > self.total || count >= column {
            return 0;
        }
        let start = column - 1;
        let mut index = start / 64;
        let mut word = self.words[index] & (u64::MAX >> (63 - start % 64));
        let mut remaining = count;
        loop {
            if word != 0 {
                if remaining == 1 {
                    return index * 64 + 63 - word.leading_zeros() as usize;
                }
                let available = word.count_ones() as usize;
                if remaining <= available {
                    for _ in 1..remaining {
                        word &= !(1_u64 << (63 - word.leading_zeros()));
                    }
                    return index * 64 + 63 - word.leading_zeros() as usize;
                }
                remaining -= available;
            }
            if index == 0 {
                return 0;
            }
            index -= 1;
            word = self.words[index];
        }
    }
}
