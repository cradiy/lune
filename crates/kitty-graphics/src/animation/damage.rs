use std::sync::Arc;

use super::{FrameBudget, FrameCharge, MIN_SPLIT_SAVINGS_PIXELS, PixelRevision, Region, Surface};

pub(super) struct DamageLink {
    edge: (u64, Region),
    older: Option<Arc<DamageLink>>,
    _charge: FrameCharge,
}

impl DamageLink {
    pub(super) fn from_surface(
        surface: &Surface,
        original_revision: u64,
        budget: &Arc<FrameBudget>,
    ) -> Option<Arc<Self>> {
        Some(Arc::new(Self {
            edge: surface.patch_edge(original_revision)?,
            older: surface.ancestors.clone(),
            _charge: budget
                .charge(size_of::<Self>() + 2 * size_of::<usize>())
                .ok()?,
        }))
    }
}

pub(super) fn damage_between(
    current: Option<&Surface>,
    previous: &PixelRevision,
    original: u64,
) -> Option<[Region; 2]> {
    let mut before = DamagePath {
        revision: previous.revision,
        edge: previous.patch,
        older: previous.ancestors.as_deref(),
    };
    let mut after = DamagePath {
        revision: current.map_or(original, |surface| surface.revision),
        edge: current.and_then(|surface| surface.patch_edge(original)),
        older: current.and_then(|surface| surface.ancestors.as_deref()),
    };
    let mut damage = Damage::default();
    // Each immutable patch is newer than its base.
    while before.revision != after.revision {
        let path = if before.revision > after.revision {
            &mut before
        } else {
            &mut after
        };
        damage.add(path.step()?);
    }
    Some(damage.regions)
}

struct DamagePath<'a> {
    revision: u64,
    edge: Option<(u64, Region)>,
    older: Option<&'a DamageLink>,
}

impl DamagePath<'_> {
    fn step(&mut self) -> Option<Region> {
        let (base, region) = self.edge.take()?;
        self.revision = base;
        self.edge = self.older.map(|link| link.edge);
        self.older = self.older.and_then(|link| link.older.as_deref());
        Some(region)
    }
}

#[derive(Default)]
struct Damage {
    regions: [Region; 2],
}

impl Damage {
    fn add(&mut self, region: Region) {
        if region.is_empty() {
            return;
        }
        let [first, second] = self.regions;
        if second.is_empty() {
            let union = first.union(region);
            if split_saves_pixels(first, region, union) {
                self.regions[1] = region;
            } else {
                self.regions[0] = union;
            }
        } else {
            let first_union = first.union(region);
            let second_union = second.union(region);
            if first_union.area() - first.area() <= second_union.area() - second.area() {
                self.regions[0] = first_union;
            } else {
                self.regions[1] = second_union;
            }
        }
    }
}

pub(super) fn split_saves_pixels(first: Region, second: Region, union: Region) -> bool {
    !first.is_empty()
        && !second.is_empty()
        && union.area().saturating_sub(first.area() + second.area()) >= MIN_SPLIT_SAVINGS_PIXELS
}
