use super::{
    EvictionRanking, ImageChange, ImageKey, ImageStore, Placement, PlacementError, QUOTA,
    StoredImage, placement_error,
};

struct Candidate {
    priority: (bool, bool, u64),
    image: ImageKey,
    bytes: usize,
}

const SMALL_CANDIDATE_SET: usize = 32;

#[derive(Default)]
pub(super) struct EvictionScratch {
    ranking: EvictionRanking,
    eligible: Vec<Candidate>,
}

pub(super) enum Evictions {
    None,
    One(ImageKey),
    Many(Vec<ImageKey>),
}

impl Evictions {
    pub(super) fn as_slice(&self) -> &[ImageKey] {
        match self {
            Self::None => &[],
            Self::One(key) => std::slice::from_ref(key),
            Self::Many(keys) => keys,
        }
    }
}

impl ImageStore {
    pub(super) fn frame_evictions(
        &mut self,
        target: ImageKey,
        mut bytes_needed: usize,
        apply: &mut impl FnMut(ImageChange<'_>) -> Result<(), PlacementError>,
    ) -> Result<Evictions, &'static [u8]> {
        let ranking = &mut self.eviction_scratch.ranking;
        ranking.placed.clear();
        ranking.protected.clear();
        apply(ImageChange::RankEvictions {
            placement: None,
            ranking,
        })
        .map_err(placement_error)?;
        ranking.placed.sort_unstable();
        ranking.protected.sort_unstable();
        let eligible = &mut self.eviction_scratch.eligible;
        eligible.clear();
        eligible
            .try_reserve_exact(self.images.len())
            .map_err(|_| QUOTA)?;
        eligible.extend(self.images.iter().filter_map(|(&key, entry)| {
            let StoredImage::Named(image) = entry else {
                return None;
            };
            if key == target
                || image.animation.is_none()
                || ranking.placed.binary_search(&key).is_ok()
                || ranking.protected.binary_search(&key).is_ok()
            {
                return None;
            }
            Some(Candidate {
                priority: (false, !image.transient_for_eviction(), image.age),
                image: key,
                bytes: 0,
            })
        }));
        eligible.sort_unstable_by_key(|candidate| candidate.priority);
        let mut evicted = Vec::new();
        for candidate in eligible {
            let StoredImage::Named(image) = &self.images[&candidate.image] else {
                unreachable!("Only named images have animation frames");
            };
            let bytes = image
                .animation
                .as_ref()
                .unwrap()
                .reclaimable_bytes(bytes_needed)?;
            if bytes == 0 {
                continue;
            }
            bytes_needed = bytes_needed.saturating_sub(bytes);
            if evicted.is_empty() && bytes_needed == 0 {
                return Ok(Evictions::One(candidate.image));
            }
            evicted.try_reserve(1).map_err(|_| QUOTA)?;
            evicted.push(candidate.image);
            if bytes_needed == 0 {
                evicted.sort_unstable();
                return Ok(Evictions::Many(evicted));
            }
        }
        Err(QUOTA)
    }

    pub(super) fn eviction_plan(
        &mut self,
        key: ImageKey,
        placement: Option<Placement>,
        bytes: usize,
        previous: Option<usize>,
        apply: &mut impl FnMut(ImageChange<'_>) -> Result<(), PlacementError>,
    ) -> Result<Evictions, &'static [u8]> {
        if bytes > self.max_bytes || self.max_images == 0 {
            return Err(QUOTA);
        }
        let previous_bytes = previous.unwrap_or_default();
        let adds_image = usize::from(previous.is_none());
        let deficit = |store: &Self| {
            let bytes_needed =
                bytes.saturating_sub(store.max_bytes - (store.stored_bytes - previous_bytes));
            let count_needed = store
                .images
                .len()
                .saturating_sub(store.max_images - adds_image);
            (bytes_needed, count_needed)
        };
        let (mut bytes_needed, mut count_needed) = deficit(self);
        if bytes_needed == 0 && count_needed == 0 {
            return Ok(Evictions::None);
        }
        if self.anonymous_images != 0 {
            self.retain_ids(|_| true);
            (bytes_needed, count_needed) = deficit(self);
            if bytes_needed == 0 && count_needed == 0 {
                return Ok(Evictions::None);
            }
        }
        self.plan_under_pressure(key, placement, bytes_needed, count_needed, apply)
    }

    #[cold]
    fn plan_under_pressure(
        &mut self,
        key: ImageKey,
        placement: Option<Placement>,
        bytes_needed: usize,
        count_needed: usize,
        apply: &mut impl FnMut(ImageChange<'_>) -> Result<(), PlacementError>,
    ) -> Result<Evictions, &'static [u8]> {
        let ranking = &mut self.eviction_scratch.ranking;
        ranking.placed.clear();
        ranking.protected.clear();
        apply(ImageChange::RankEvictions { placement, ranking }).map_err(placement_error)?;
        ranking.placed.sort_unstable();
        ranking.placed.dedup();
        ranking.protected.sort_unstable();

        let candidates = || {
            self.images.iter().filter_map(|(&image, entry)| {
                if image == key || ranking.protected.binary_search(&image).is_ok() {
                    return None;
                }
                let (anonymous, transient, age) = match entry {
                    StoredImage::Named(image_data) => {
                        (false, image_data.transient_for_eviction(), image_data.age)
                    }
                    StoredImage::Anonymous {
                        revision,
                        transient,
                        ..
                    } => (true, *transient, *revision),
                };
                Some(Candidate {
                    priority: (anonymous, !transient, age),
                    image,
                    bytes: entry.storage_bytes(),
                })
            })
        };
        let rank = |mut candidate: Candidate| {
            candidate.priority.0 |= ranking.placed.binary_search(&candidate.image).is_ok();
            candidate
        };

        if count_needed <= 1 {
            let mut first: Option<Candidate> = None;
            for candidate in candidates() {
                // A placement only lowers a candidate's eviction priority.
                if first
                    .as_ref()
                    .is_some_and(|best| candidate.priority >= best.priority)
                {
                    continue;
                }
                let candidate = rank(candidate);
                if first
                    .as_ref()
                    .is_none_or(|best| candidate.priority < best.priority)
                {
                    first = Some(candidate);
                }
            }
            if let Some(candidate) = first
                && candidate.bytes >= bytes_needed
            {
                return Ok(Evictions::One(candidate.image));
            }
        }

        let eligible = &mut self.eviction_scratch.eligible;
        eligible.clear();
        eligible
            .try_reserve_exact(self.images.len())
            .map_err(|_| QUOTA)?;
        eligible.extend(candidates().map(rank));
        let count = eviction_prefix(eligible, bytes_needed, count_needed).ok_or(QUOTA)?;
        let mut evicted = Vec::new();
        evicted.try_reserve_exact(count).map_err(|_| QUOTA)?;
        evicted.extend(eligible[..count].iter().map(|candidate| candidate.image));
        evicted.sort_unstable();
        Ok(Evictions::Many(evicted))
    }
}

fn eviction_prefix(
    mut candidates: &mut [Candidate],
    mut bytes_needed: usize,
    mut count_needed: usize,
) -> Option<usize> {
    let mut selected = 0;
    while candidates.len() > SMALL_CANDIDATE_SET {
        let (earlier, pivot, later) =
            candidates.select_nth_unstable_by_key(candidates.len() / 2, |entry| entry.priority);
        let earlier_bytes = earlier.iter().map(|entry| entry.bytes).sum::<usize>();
        if earlier_bytes >= bytes_needed && earlier.len() >= count_needed {
            candidates = earlier;
        } else {
            // Any sufficient prefix includes the earlier entries and the pivot.
            let count = earlier.len() + 1;
            selected += count;
            bytes_needed = bytes_needed.saturating_sub(earlier_bytes + pivot.bytes);
            count_needed = count_needed.saturating_sub(count);
            if bytes_needed == 0 && count_needed == 0 {
                return Some(selected);
            }
            candidates = later;
        }
    }
    candidates.sort_unstable_by_key(|entry| entry.priority);
    candidates
        .iter()
        .position(|entry| {
            bytes_needed = bytes_needed.saturating_sub(entry.bytes);
            count_needed = count_needed.saturating_sub(1);
            bytes_needed == 0 && count_needed == 0
        })
        .map(|index| selected + index + 1)
}
