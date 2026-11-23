use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lune_text::WidthPolicy;
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

use crate::StorageError;

pub(crate) const MAX_SIZED_TEXT_BYTES: usize = 4096;
pub(crate) const MAX_SIZING_METADATA_BYTES: usize = 128;
const MAX_RETAINED_BYTES: usize = 8 << 20;

pub(crate) fn joins_grapheme(text: &str, suffix: &str) -> bool {
    let mut cursor = GraphemeCursor::new(text.len(), text.len() + suffix.len(), true);
    loop {
        match cursor.is_boundary(suffix, text.len()) {
            Ok(boundary) => return !boundary,
            Err(GraphemeIncomplete::PreContext(end)) => cursor.provide_context(&text[..end], 0),
            Err(_) => unreachable!("The block and following text are available"),
        }
    }
}

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
    natural_width: bool,
    text: String,
    erased: AtomicBool,
    budget: Arc<SizedBudget>,
}

#[derive(Debug, Default)]
pub(crate) struct SizedBudget(AtomicUsize);

#[derive(Clone, Copy)]
pub(crate) enum SizedWidth {
    Explicit(usize),
    Natural(usize),
}

impl SizedWidth {
    pub(crate) fn columns(self, size: TextSize) -> usize {
        let (Self::Explicit(width) | Self::Natural(width)) = self;
        width * usize::from(size.scale)
    }
}

impl SizedBlock {
    pub(crate) fn new(
        size: TextSize,
        width: SizedWidth,
        text: &str,
        budget: &Arc<SizedBudget>,
    ) -> Result<Option<Arc<Self>>, StorageError> {
        Self::with_suffix(
            size,
            width.columns(size),
            matches!(width, SizedWidth::Natural(_)),
            text,
            "",
            budget,
        )
    }

    pub(crate) fn columns_with_suffix(&self, suffix: &str) -> usize {
        let scale = usize::from(self.size.scale);
        let Some(selector @ ('\u{fe0e}' | '\u{fe0f}')) = suffix.chars().next() else {
            return self.columns;
        };
        if !self.natural_width
            || (selector == '\u{fe0e}' && self.columns != scale * 2)
            || (selector == '\u{fe0f}' && (scale != 1 || self.columns != 1))
        {
            return self.columns;
        }
        self.text
            .chars()
            .next_back()
            .and_then(|base| variation_columns(base, selector))
            .map_or(self.columns, |columns| columns * scale)
    }

    pub(crate) fn placement_columns(&self) -> usize {
        let scale = usize::from(self.size.scale);
        if self.natural_width && self.columns == scale && self.text.len() >= 4 {
            let mut text = self.text.chars();
            let base = text.next().unwrap();
            if text.next() == Some('\u{fe0e}') && variation_columns(base, '\u{fe0e}') == Some(1) {
                // The original wide emoji may have already wrapped before VS15 narrows it.
                return 2 * scale;
            }
        }
        self.columns
    }

    pub(crate) fn append(
        &self,
        suffix: &str,
        columns: usize,
    ) -> Result<Option<Arc<Self>>, StorageError> {
        Self::with_suffix(
            self.size,
            columns,
            self.natural_width,
            &self.text,
            suffix,
            &self.budget,
        )
    }

    fn with_suffix(
        size: TextSize,
        columns: usize,
        natural_width: bool,
        text: &str,
        suffix: &str,
        budget: &Arc<SizedBudget>,
    ) -> Result<Option<Arc<Self>>, StorageError> {
        let length = text.len().saturating_add(suffix.len());
        if length > MAX_SIZED_TEXT_BYTES
            || budget.0.load(Ordering::Relaxed) + size_of::<Self>() + length > MAX_RETAINED_BYTES
        {
            return Ok(None);
        }
        let mut owned = String::new();
        owned.try_reserve_exact(length)?;
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
        owned.push_str(suffix);
        Ok(Some(Arc::new(Self {
            size,
            columns,
            natural_width,
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

    pub(crate) fn retained_bytes(&self) -> usize {
        size_of::<Self>() + self.text.capacity()
    }
}

fn variation_columns(base: char, selector: char) -> Option<usize> {
    let mut sequence = [0; 7];
    let end = base.encode_utf8(&mut sequence).len();
    let length = selector.encode_utf8(&mut sequence[end..]).len();
    let variant = std::str::from_utf8(&sequence[..end + length]).unwrap();
    let policy = WidthPolicy::Narrow;
    match (
        selector,
        policy.columns(&variant[..end]),
        policy.columns(variant),
    ) {
        ('\u{fe0e}', 2, 1) => Some(1),
        ('\u{fe0f}', 1, 2) => Some(2),
        _ => None,
    }
}

impl Drop for SizedBlock {
    fn drop(&mut self) {
        self.budget
            .0
            .fetch_sub(self.retained_bytes(), Ordering::Relaxed);
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
        self.block.retained_bytes()
    }
}
