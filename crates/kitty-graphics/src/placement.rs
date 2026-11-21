use std::sync::Arc;

use crate::{Controls, Image, ImageKey, Value, control_keys};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub image: ImageKey,
    pub id: u32,
    pub source_origin: [u32; 2],
    pub source_size: [u32; 2],
    pub cell_offset: [u32; 2],
    pub cells: [u32; 2],
    pub z: i32,
    pub keep_cursor: bool,
    pub position: PlacementPosition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementPosition {
    Cursor,
    Virtual,
    Relative(RelativePlacement),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelativePlacement {
    pub image_id: u32,
    pub placement_id: u32,
    pub offset: [i32; 2],
}

impl Placement {
    pub(crate) const CONTROL_NAMES: &[u8] = b"pxywhXYcrCUzPQHV";
    pub(crate) const KEYS: u64 = control_keys(Self::CONTROL_NAMES);

    pub(crate) fn parse(image: ImageKey, controls: &Controls) -> Option<Self> {
        let unsigned = |key| match controls.get(key) {
            Some(Value::Unsigned(value)) => Some(value),
            None => Some(0),
            _ => None,
        };
        let signed = |key| match controls.get(key) {
            Some(Value::Signed(value)) => Some(value),
            Some(Value::Unsigned(value)) => i32::try_from(value).ok(),
            None => Some(0),
            _ => None,
        };
        let cells = [unsigned(b'c')?, unsigned(b'r')?];
        let parent = unsigned(b'P')?;
        let parent_placement = unsigned(b'Q')?;
        let offset = [signed(b'H')?, signed(b'V')?];
        let position = match (unsigned(b'U')?, parent) {
            (0, 0) if parent_placement == 0 && offset == [0, 0] => PlacementPosition::Cursor,
            (0, image_id) if image_id != 0 => PlacementPosition::Relative(RelativePlacement {
                image_id,
                placement_id: parent_placement,
                offset,
            }),
            (1, 0) if parent_placement == 0 && offset == [0, 0] && image.id() != 0 => {
                PlacementPosition::Virtual
            }
            _ => return None,
        };
        Some(Self {
            image,
            id: if image.id() == 0 { 0 } else { unsigned(b'p')? },
            source_origin: [unsigned(b'x')?, unsigned(b'y')?],
            source_size: [unsigned(b'w')?, unsigned(b'h')?],
            cell_offset: [unsigned(b'X')?, unsigned(b'Y')?],
            cells,
            z: signed(b'z')?,
            keep_cursor: match unsigned(b'C')? {
                0 => false,
                1 => true,
                _ => return None,
            },
            position,
        })
    }
}

#[derive(Debug)]
pub enum ImageChange<'a> {
    /// Collects placement and dependency information before choosing evictions.
    RankEvictions {
        placement: Option<Placement>,
        ranking: &'a mut EvictionRanking,
    },
    Remove {
        replaced: Option<ImageKey>,
        evicted: &'a [ImageKey],
    },
    Place {
        placement: Placement,
        dimensions: (u32, u32),
        replaces_image: bool,
        anonymous: Option<Arc<Image>>,
        evicted: &'a [ImageKey],
    },
}

#[derive(Debug, Default)]
pub struct EvictionRanking {
    pub(crate) placed: Vec<ImageKey>,
    pub(crate) protected: Vec<ImageKey>,
}

impl EvictionRanking {
    pub fn mark_placed(&mut self, image: ImageKey) -> Result<(), PlacementError> {
        if image.id() == 0 {
            return Ok(());
        }
        self.placed
            .try_reserve(1)
            .map_err(|_| PlacementError::Quota)?;
        self.placed.push(image);
        Ok(())
    }

    pub fn protect(&mut self, image: ImageKey) -> Result<(), PlacementError> {
        self.protected
            .try_reserve(1)
            .map_err(|_| PlacementError::Quota)?;
        self.protected.push(image);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementError {
    InvalidRegion,
    Quota,
    MissingParent,
    Cycle,
    TooDeep,
}
