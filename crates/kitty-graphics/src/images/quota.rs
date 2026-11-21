use super::{
    EvictionRanking, ImageChange, ImageKey, ImageStore, Placement, PlacementError, QUOTA,
    StoredImage, placement_error,
};

impl ImageStore {
    pub(super) fn eviction_plan(
        &mut self,
        key: ImageKey,
        placement: Option<Placement>,
        bytes: usize,
        previous: Option<usize>,
        apply: &mut impl FnMut(ImageChange<'_>) -> Result<(), PlacementError>,
    ) -> Result<Vec<ImageKey>, &'static [u8]> {
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
        if deficit(self) == (0, 0) {
            return Ok(Vec::new());
        }
        if self.anonymous_images != 0 {
            self.retain_ids(|_| true);
        }
        let (bytes_needed, count_needed) = deficit(self);
        if bytes_needed == 0 && count_needed == 0 {
            return Ok(Vec::new());
        }
        self.plan_under_pressure(key, placement, bytes_needed, count_needed, apply)
    }

    #[cold]
    fn plan_under_pressure(
        &self,
        key: ImageKey,
        placement: Option<Placement>,
        mut bytes_needed: usize,
        mut count_needed: usize,
        apply: &mut impl FnMut(ImageChange<'_>) -> Result<(), PlacementError>,
    ) -> Result<Vec<ImageKey>, &'static [u8]> {
        let mut ranking = EvictionRanking::default();
        apply(ImageChange::RankEvictions {
            placement,
            ranking: &mut ranking,
        })
        .map_err(placement_error)?;
        ranking.placed.sort_unstable();
        ranking.placed.dedup();
        ranking.protected.sort_unstable();

        let candidates = || {
            self.images.iter().filter_map(|(&image, entry)| {
                if image == key || ranking.protected.binary_search(&image).is_ok() {
                    return None;
                }
                let (placed, transient, age) = match entry {
                    StoredImage::Named(image_data) => (
                        ranking.placed.binary_search(&image).is_ok(),
                        image_data.transient,
                        image_data.age,
                    ),
                    StoredImage::Anonymous {
                        revision,
                        transient,
                        ..
                    } => (true, *transient, *revision),
                };
                Some(((placed, !transient, age), image, entry.storage_bytes()))
            })
        };

        let mut evicted = Vec::new();
        if count_needed <= 1
            && let Some((_, image, bytes)) = candidates().min_by_key(|&(priority, _, _)| priority)
            && bytes >= bytes_needed
        {
            evicted.try_reserve_exact(1).map_err(|_| QUOTA)?;
            evicted.push(image);
            return Ok(evicted);
        }

        let mut ordered = Vec::new();
        ordered
            .try_reserve_exact(self.images.len())
            .map_err(|_| QUOTA)?;
        ordered.extend(candidates());
        ordered.sort_unstable_by_key(|&(priority, _, _)| priority);
        for (_, image, bytes) in ordered {
            evicted.try_reserve(1).map_err(|_| QUOTA)?;
            evicted.push(image);
            bytes_needed = bytes_needed.saturating_sub(bytes);
            count_needed = count_needed.saturating_sub(1);
            if bytes_needed == 0 && count_needed == 0 {
                evicted.sort_unstable();
                return Ok(evicted);
            }
        }
        Err(QUOTA)
    }
}
