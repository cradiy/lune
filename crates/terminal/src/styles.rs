use std::{collections::HashMap, collections::TryReserveError, error::Error, fmt};

use crate::{Color, Style, StyleId, Underline};

/// Maximum live styles, including the permanent default entry.
pub const MAX_STYLES: usize = 1 << 16;
const INDEX_MASK: u32 = (1 << 16) - 1;
const FREE_END: u32 = u32::MAX;

#[inline]
fn color_key(color: Color) -> u64 {
    match color {
        Color::Default => 0,
        Color::Indexed(index) => u64::from(index) + 1,
        Color::Rgb { red, green, blue } => {
            (1 << 24) | (u64::from(red) << 16) | (u64::from(green) << 8) | u64::from(blue)
        }
    }
}

#[inline]
fn key(style: Style) -> u64 {
    let underline = match style.underline {
        Underline::None => 0,
        Underline::Single => 1,
        Underline::Double => 2,
    };
    let attributes = u64::from(style.bold)
        | (u64::from(style.faint) << 1)
        | (u64::from(style.italic) << 2)
        | (u64::from(style.blink) << 3)
        | (u64::from(style.inverse) << 4)
        | (u64::from(style.hidden) << 5)
        | (u64::from(style.strikethrough) << 6)
        | (underline << 7);
    // Each color occupies 25 bits; rendition occupies the next nine bits.
    color_key(style.foreground) | (color_key(style.background) << 25) | (attributes << 50)
}

#[derive(Debug)]
pub enum StyleError {
    InvalidLimit,
    CapacityExceeded,
    InvalidId(StyleId),
    Allocation(TryReserveError),
}

impl fmt::Display for StyleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimit => write!(f, "Style capacity must be between 1 and {MAX_STYLES}"),
            Self::CapacityExceeded => f.write_str("Style storage capacity is exhausted"),
            Self::InvalidId(id) => write!(f, "Style ID {} is not live in this table", id.0),
            Self::Allocation(error) => write!(f, "Style storage allocation failed: {error}"),
        }
    }
}

impl Error for StyleError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Allocation(error) => Some(error),
            _ => None,
        }
    }
}

impl From<TryReserveError> for StyleError {
    fn from(error: TryReserveError) -> Self {
        Self::Allocation(error)
    }
}

struct Slot {
    style: Option<Style>,
    generation: u16,
    next: u32,
    marked: bool,
}

/// Bounded style interning with explicit tracing of live IDs.
///
/// IDs belong to their originating table. Zero permanently resolves to default
/// rendition. Other IDs combine a slot index and generation; reclaimed IDs stay
/// invalid after reuse. A slot is retired when its generation is exhausted.
/// The limit includes default and retired slots, bounding retained slot storage.
///
/// The caller traces every cell and external owner during collection, including
/// current/saved rendition and any retained screen or rendering snapshot. IDs
/// omitted from those roots become invalid. Existing IDs survive collection and
/// storage growth unchanged. Operations reserve capacity before publishing a new
/// entry; failures preserve all existing mappings. Allocator capacity can grow on
/// error and is retained for subsequent operations.
pub struct StyleTable {
    default: Style,
    styles: HashMap<u64, StyleId>,
    slots: Vec<Slot>,
    free: u32,
    limit: usize,
}

impl StyleTable {
    /// Creates a table with a logical slot limit, initially containing default.
    /// Construction makes no allocation requests.
    pub fn new(limit: usize) -> Result<Self, StyleError> {
        if !(1..=MAX_STYLES).contains(&limit) {
            return Err(StyleError::InvalidLimit);
        }
        Ok(Self {
            default: Style::default(),
            styles: HashMap::new(),
            slots: Vec::new(),
            free: FREE_END,
            limit,
        })
    }

    /// Number of live entries, including default rendition.
    pub fn style_count(&self) -> usize {
        self.styles.len() + 1
    }

    /// Resolves an ID in constant time. Reclaimed or unknown IDs return `None`.
    #[inline]
    pub fn get(&self, id: StyleId) -> Option<&Style> {
        if id.0 == 0 {
            return Some(&self.default);
        }
        let index = (id.0 & INDEX_MASK).checked_sub(1)? as usize;
        let slot = self.slots.get(index)?;
        if u32::from(slot.generation) != id.0 >> 16 {
            return None;
        }
        slot.style.as_ref()
    }

    /// Reuses an equal live entry or interns a new value. Existing/default hits
    /// make no allocation requests, including when the table has reached its limit.
    #[inline]
    pub fn intern(&mut self, style: Style) -> Result<StyleId, StyleError> {
        let key = key(style);
        if key == 0 {
            return Ok(StyleId(0));
        }
        if let Some(&id) = self.styles.get(&key) {
            return Ok(id);
        }
        self.insert(style, key)
    }

    #[inline(never)]
    fn insert(&mut self, style: Style, key: u64) -> Result<StyleId, StyleError> {
        let index = if self.free != FREE_END {
            self.free as usize
        } else if self.slots.len() < self.limit - 1 {
            self.slots.len()
        } else {
            return Err(StyleError::CapacityExceeded);
        };
        self.styles.try_reserve(1)?;
        if index == self.slots.len() {
            self.slots.try_reserve(1)?;
            self.slots.push(Slot {
                style: None,
                generation: 0,
                next: FREE_END,
                marked: false,
            });
        } else {
            self.free = self.slots[index].next;
        }
        let slot = &mut self.slots[index];
        let id = StyleId((u32::from(slot.generation) << 16) | (index as u32 + 1));
        slot.style = Some(style);
        self.styles.insert(key, id);
        Ok(id)
    }

    /// Reclaims entries absent from the complete root set and returns their count.
    /// Repeated roots and default are accepted. Invalid roots reject collection
    /// atomically. Validation and collection use no auxiliary allocation.
    pub fn collect(
        &mut self,
        roots: impl IntoIterator<Item = StyleId>,
    ) -> Result<usize, StyleError> {
        // Clear marks left by rejected or interrupted root traversal.
        for slot in &mut self.slots {
            slot.marked = false;
        }
        for id in roots {
            if id.0 == 0 {
                continue;
            }
            if self.get(id).is_none() {
                return Err(StyleError::InvalidId(id));
            }
            self.slots[(id.0 & INDEX_MASK) as usize - 1].marked = true;
        }
        let mut reclaimed = 0;
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if slot.marked {
                slot.marked = false;
                continue;
            }
            if let Some(style) = slot.style.take() {
                self.styles.remove(&key(style));
                reclaimed += 1;
                if let Some(generation) = slot.generation.checked_add(1) {
                    slot.generation = generation;
                    slot.next = self.free;
                    self.free = index as u32;
                }
            }
        }
        Ok(reclaimed)
    }
}
