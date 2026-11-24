use std::collections::{HashMap, TryReserveError};

use crate::StyleError;

/// Maximum live hyperlinks in a [`HyperlinkTable`].
pub const MAX_HYPERLINKS: usize = 1 << 16;
/// Maximum UTF-8 byte length of an OSC 8 hyperlink URI.
pub const MAX_HYPERLINK_URI_BYTES: usize = 2048;
pub(crate) const MAX_HYPERLINK_PARAMS_BYTES: usize = 512;

const INDEX_MASK: u32 = (1 << 16) - 1;
const FREE_END: u32 = u32::MAX;

/// An opaque generational hyperlink handle. Zero means no hyperlink.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct HyperlinkId(pub u32);

/// An interned OSC 8 hyperlink target and optional grouping identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Hyperlink {
    uri: String,
    id: Option<String>,
}

impl Hyperlink {
    #[inline]
    pub fn uri(&self) -> &str {
        &self.uri
    }

    #[inline]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }
}

struct Slot {
    link: Option<Hyperlink>,
    generation: u16,
    next: u32,
    marked: bool,
}

/// Bounded OSC 8 hyperlink interning with generational handles and explicit GC.
///
/// Construction makes no allocation requests. Zero permanently represents the
/// absence of a hyperlink.
pub struct HyperlinkTable {
    index: HashMap<Hyperlink, HyperlinkId>,
    slots: Vec<Slot>,
    free: u32,
    limit: usize,
}

impl Default for HyperlinkTable {
    fn default() -> Self {
        Self::new(MAX_HYPERLINKS)
    }
}

impl HyperlinkTable {
    /// Creates an empty hyperlink table with a logical slot limit.
    /// Construction makes no allocation requests.
    pub fn new(limit: usize) -> Self {
        Self {
            index: HashMap::new(),
            slots: Vec::new(),
            free: FREE_END,
            limit: limit.clamp(1, MAX_HYPERLINKS),
        }
    }

    /// Number of live interned hyperlinks (excluding zero).
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Resolves a hyperlink handle in constant time.
    #[inline]
    pub fn get(&self, id: HyperlinkId) -> Option<&Hyperlink> {
        if id.0 == 0 {
            return None;
        }
        let slot_index = (id.0 & INDEX_MASK).checked_sub(1)? as usize;
        let slot = self.slots.get(slot_index)?;
        if u32::from(slot.generation) != id.0 >> 16 {
            return None;
        }
        slot.link.as_ref()
    }

    /// Reuses an equal live entry or interns a new hyperlink fallibly.
    pub fn intern(&mut self, uri: &str, id: Option<&str>) -> Result<HyperlinkId, StyleError> {
        if uri.is_empty() {
            return Ok(HyperlinkId(0));
        }
        if let Some((&existing_id, _)) = self
            .index
            .iter()
            .find(|(link, _)| link.uri == uri && link.id.as_deref() == id)
            .map(|(_, id)| (id, ()))
        {
            return Ok(existing_id);
        }
        let slot_index = if self.free != FREE_END {
            self.free as usize
        } else if self.slots.len() < self.limit - 1 {
            self.slots.len()
        } else {
            return Err(StyleError::CapacityExceeded);
        };
        let owned = clone_hyperlink(uri, id)?;
        self.index.try_reserve(1)?;
        if slot_index == self.slots.len() {
            self.slots.try_reserve(1)?;
            self.slots.push(Slot {
                link: None,
                generation: 0,
                next: FREE_END,
                marked: false,
            });
        } else {
            self.free = self.slots[slot_index].next;
        }
        let slot = &mut self.slots[slot_index];
        let handle = HyperlinkId((u32::from(slot.generation) << 16) | (slot_index as u32 + 1));
        slot.link = Some(owned.clone());
        self.index.insert(owned, handle);
        Ok(handle)
    }

    /// Reclaims hyperlinks absent from `roots` without allocating.
    pub fn collect(&mut self, roots: impl IntoIterator<Item = HyperlinkId>) -> usize {
        if self.slots.is_empty() {
            return 0;
        }
        for slot in &mut self.slots {
            slot.marked = false;
        }
        for id in roots {
            if id.0 == 0 {
                continue;
            }
            if let Some(slot_index) = (id.0 & INDEX_MASK).checked_sub(1)
                && let Some(slot) = self.slots.get_mut(slot_index as usize)
                && u32::from(slot.generation) == id.0 >> 16
                && slot.link.is_some()
            {
                slot.marked = true;
            }
        }
        let mut reclaimed = 0;
        for (slot_index, slot) in self.slots.iter_mut().enumerate() {
            if slot.marked {
                slot.marked = false;
                continue;
            }
            let Some(link) = slot.link.take() else {
                continue;
            };
            self.index.remove(&link);
            reclaimed += 1;
            if let Some(next_gen) = slot.generation.checked_add(1) {
                slot.generation = next_gen;
                slot.next = self.free;
                self.free = slot_index as u32;
            }
        }
        reclaimed
    }
}

fn clone_hyperlink(uri: &str, id: Option<&str>) -> Result<Hyperlink, TryReserveError> {
    let mut owned_uri = String::new();
    owned_uri.try_reserve_exact(uri.len())?;
    owned_uri.push_str(uri);
    let owned_id = match id {
        Some(id) if !id.is_empty() => {
            let mut owned = String::new();
            owned.try_reserve_exact(id.len())?;
            owned.push_str(id);
            Some(owned)
        }
        _ => None,
    };
    Ok(Hyperlink {
        uri: owned_uri,
        id: owned_id,
    })
}
