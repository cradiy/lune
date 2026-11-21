use std::collections::VecDeque;
use std::sync::Arc;

use super::{FrameBudget, FrameCharge, MIN_SPLIT_SAVINGS_PIXELS, PixelRevision, Region, Surface};

const MAX_INHERITED_EDITS: usize = 8;

#[derive(Debug, Default)]
pub(super) struct InheritedDamage(VecDeque<InheritedEdit>);

#[derive(Debug)]
struct InheritedEdit {
    previous: u64,
    current: u64,
    region: Region,
}

impl InheritedDamage {
    pub(super) fn record(&mut self, previous: u64, current: u64, region: Region) {
        let edits = &mut self.0;
        if edits.capacity() == 0 && edits.try_reserve_exact(MAX_INHERITED_EDITS).is_err() {
            return;
        }
        if edits.back().is_some_and(|edit| edit.current != previous) {
            edits.clear();
        } else if edits.len() == MAX_INHERITED_EDITS {
            edits.pop_front();
        }
        edits.push_back(InheritedEdit {
            previous,
            current,
            region,
        });
    }

    #[inline]
    pub(super) fn since(&self, previous: u64, current: u64) -> Option<[Region; 2]> {
        let latest = self.0.back().filter(|edit| edit.current == current)?;
        if latest.previous == previous {
            return Some([latest.region, Region::default()]);
        }
        self.merge_since(previous)
    }

    #[cold]
    fn merge_since(&self, previous: u64) -> Option<[Region; 2]> {
        let mut damage = Damage::default();
        for edit in self.0.iter().rev() {
            damage.add(edit.region);
            if edit.previous == previous {
                return Some(damage.regions);
            }
        }
        None
    }
}

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
