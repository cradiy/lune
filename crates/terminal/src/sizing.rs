use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::StorageError;

pub(crate) const MAX_SIZED_TEXT_BYTES: usize = 4096;
pub(crate) const MAX_SIZING_METADATA_BYTES: usize = 128;
const MAX_RETAINED_BYTES: usize = 8 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TextSize {
    pub scale: u8,
    pub numerator: u8,
    pub denominator: u8,
    pub horizontal: u8,
    pub vertical: u8,
}

impl TextSize {
    pub fn fraction(self) -> f32 {
        if self.denominator == 0 {
            1.0
        } else {
            f32::from(self.numerator) / f32::from(self.denominator)
        }
    }
}

pub(crate) fn parse(bytes: &[u8]) -> Option<(TextSize, usize, &str)> {
    let input = std::str::from_utf8(bytes).ok()?;
    let (metadata, text) = input.split_once(';')?;
    if metadata.len() > MAX_SIZING_METADATA_BYTES
        || text.len() > MAX_SIZED_TEXT_BYTES
        || text.chars().any(char::is_control)
    {
        return None;
    }
    let mut size = TextSize {
        scale: 1,
        numerator: 0,
        denominator: 0,
        horizontal: 0,
        vertical: 0,
    };
    let mut width = 0;
    if !metadata.is_empty() {
        for pair in metadata.split(':') {
            let (key, value) = pair.split_once('=')?;
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let value = value.parse::<u8>().ok()?;
            match key {
                "s" if (1..=7).contains(&value) => size.scale = value,
                "w" if value <= 7 => width = usize::from(value),
                "n" if value <= 15 => size.numerator = value,
                "d" if value <= 15 => size.denominator = value,
                "h" if value <= 2 => size.horizontal = value,
                "v" if value <= 2 => size.vertical = value,
                _ => return None,
            }
        }
    }
    if size.denominator != 0 && size.numerator >= size.denominator {
        return None;
    }
    Some((size, width, text))
}

#[derive(Debug)]
pub(crate) struct SizedBlock {
    pub(crate) size: TextSize,
    pub(crate) columns: usize,
    text: String,
    erased: AtomicBool,
    budget: Arc<SizedBudget>,
}

#[derive(Debug, Default)]
pub(crate) struct SizedBudget(AtomicUsize);

impl SizedBlock {
    pub(crate) fn new(
        size: TextSize,
        columns: usize,
        text: &str,
        budget: &Arc<SizedBudget>,
    ) -> Result<Option<Arc<Self>>, StorageError> {
        if budget.0.load(Ordering::Relaxed) + size_of::<Self>() + text.len() > MAX_RETAINED_BYTES {
            return Ok(None);
        }
        let mut owned = String::new();
        owned.try_reserve_exact(text.len())?;
        let charge = size_of::<Self>() + owned.capacity();
        if budget
            .0
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |bytes| {
                let total = bytes + charge;
                (total <= MAX_RETAINED_BYTES).then_some(total)
            })
            .is_err()
        {
            return Ok(None);
        }
        owned.push_str(text);
        Ok(Some(Arc::new(Self {
            size,
            columns,
            text: owned,
            erased: AtomicBool::new(false),
            budget: Arc::clone(budget),
        })))
    }

    pub(crate) fn is_visible(&self) -> bool {
        !self.erased.load(Ordering::Relaxed)
    }

    pub(crate) fn erase(&self) {
        self.erased.store(true, Ordering::Relaxed);
    }
}

impl Drop for SizedBlock {
    fn drop(&mut self) {
        self.budget
            .0
            .fetch_sub(size_of::<Self>() + self.text.capacity(), Ordering::Relaxed);
    }
}

/// One row of an OSC 66 text block.
#[derive(Clone, Debug)]
pub struct SizedTextRow {
    pub column: usize,
    pub row_offset: usize,
    pub(crate) block: Arc<SizedBlock>,
}

impl SizedTextRow {
    pub fn size(&self) -> TextSize {
        self.block.size
    }

    pub fn columns(&self) -> Range<usize> {
        self.column..self.column + self.block.columns
    }

    pub fn text(&self) -> &str {
        &self.block.text
    }

    pub fn belongs_to(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.block, &other.block)
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        size_of::<SizedBlock>() + self.block.text.capacity()
    }
}
