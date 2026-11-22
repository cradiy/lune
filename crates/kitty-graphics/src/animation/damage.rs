use std::sync::Arc;

use super::{FrameBudget, FrameCharge, MIN_SPLIT_SAVINGS_PIXELS, PixelRevision, Region, Surface};

const MAX_RECENT_EDITS: usize = 8;

#[derive(Debug, Default)]
pub(super) struct RecentDamage {
    edits: Vec<RecentEdit>,
    next: usize,
}

#[derive(Debug)]
pub(super) struct RecentEdit {
    previous: u64,
    pub(super) current: u64,
    region: Region,
}

impl RecentDamage {
    pub(super) fn record(&mut self, previous: u64, current: u64, region: Region) {
        let edit = RecentEdit {
            previous,
            current,
            region,
        };
        if let Some(slot) = self.edits.get_mut(self.next) {
            *slot = edit;
        } else {
            if self.edits.capacity() == 0 && self.edits.try_reserve_exact(MAX_RECENT_EDITS).is_err()
            {
                return;
            }
            self.edits.push(edit);
        }
        self.next = (self.next + 1) % MAX_RECENT_EDITS;
    }

    pub(super) fn latest(&self) -> Option<&RecentEdit> {
        self.edits
            .get((self.next + MAX_RECENT_EDITS - 1) % MAX_RECENT_EDITS)
    }

    #[inline]
    pub(super) fn since(
        &self,
        latest: &RecentEdit,
        previous: u64,
        current: u64,
    ) -> Option<[Region; 2]> {
        if current > latest.current || current <= previous {
            return None;
        }
        if latest.current == current && latest.previous == previous {
            return Some([latest.region, Region::default()]);
        }
        self.merge_since(previous, current)
    }

    #[cold]
    fn merge_since(&self, previous: u64, mut revision: u64) -> Option<[Region; 2]> {
        let mut damage = Damage::default();
        for edit in self.newest_first() {
            if edit.current != revision {
                continue;
            }
            damage.add(edit.region);
            revision = edit.previous;
            if revision == previous {
                return Some(damage.regions);
            }
        }
        None
    }

    fn edge(&self, revision: u64) -> Option<(u64, Region)> {
        self.newest_first()
            .find(|edit| edit.current == revision)
            .map(|edit| (edit.previous, edit.region))
    }

    fn newest_first(&self) -> impl Iterator<Item = &RecentEdit> {
        // After wrapping, entries before the next write are newer than the suffix.
        let (newer, older) = self.edits.split_at(self.next);
        newer.iter().rev().chain(older.iter().rev())
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
    recent: &RecentDamage,
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
    // Edges point to older revisions.
    while before.revision != after.revision {
        let path = if before.revision > after.revision {
            &mut before
        } else {
            &mut after
        };
        damage.add(path.step(recent)?);
    }
    Some(damage.regions)
}

struct DamagePath<'a> {
    revision: u64,
    edge: Option<(u64, Region)>,
    older: Option<&'a DamageLink>,
}

impl DamagePath<'_> {
    fn step(&mut self, recent: &RecentDamage) -> Option<Region> {
        let (base, region) = self.edge.take().or_else(|| recent.edge(self.revision))?;
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
