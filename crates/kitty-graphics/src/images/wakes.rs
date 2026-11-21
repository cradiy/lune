use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use super::{Animation, Image, ImageKey, StoredImage};

#[derive(Default)]
pub(super) struct AnimationWakes {
    pending: HashSet<ImageKey>,
    scheduled: BTreeMap<Instant, Vec<ImageKey>>,
}

impl AnimationWakes {
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.scheduled.first_key_value().map(|(&at, _)| at)
    }

    pub(super) fn update(&mut self, key: ImageKey, image: &mut Image) {
        let Some(animation) = image.animation.as_mut() else {
            return;
        };
        self.unschedule(key, animation);
        if animation.playing() {
            self.pending.insert(key);
        } else {
            self.pending.remove(&key);
        }
    }

    pub(super) fn remove(&mut self, key: ImageKey, animation: &mut Animation) {
        self.unschedule(key, animation);
        self.pending.remove(&key);
    }

    fn unschedule(&mut self, key: ImageKey, animation: &mut Animation) {
        if let Some(at) = animation.scheduled_at.take() {
            let keys = self
                .scheduled
                .get_mut(&at)
                .expect("A scheduled animation has a wake");
            let index = keys
                .iter()
                .position(|&candidate| candidate == key)
                .expect("A scheduled animation is in its wake");
            keys.swap_remove(index);
            if keys.is_empty() {
                self.scheduled.remove(&at);
            }
        }
    }

    pub(super) fn poll(
        &mut self,
        clock: impl FnOnce() -> Instant,
        images: &mut HashMap<ImageKey, StoredImage>,
    ) -> bool {
        if self.pending.is_empty() && self.scheduled.is_empty() {
            return false;
        }
        let now = clock();
        let mut changed = false;
        for key in self.pending.drain() {
            if let Some(at) = advance(images, key, now, &mut changed) {
                self.scheduled.entry(at).or_default().push(key);
            }
        }
        while let Some(wake) = self.scheduled.first_entry()
            && *wake.key() <= now
        {
            let mut keys = wake.remove();
            let mut shared_wake = None;
            keys.retain(|&key| {
                let Some(at) = advance(images, key, now, &mut changed) else {
                    return false;
                };
                if at == *shared_wake.get_or_insert(at) {
                    return true;
                }
                self.scheduled.entry(at).or_default().push(key);
                false
            });
            if let Some(at) = shared_wake {
                // Reuse the list when animations continue to share a deadline.
                match self.scheduled.entry(at) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(keys);
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        entry.get_mut().append(&mut keys);
                    }
                }
            }
        }
        changed
    }
}

fn advance(
    images: &mut HashMap<ImageKey, StoredImage>,
    key: ImageKey,
    now: Instant,
    changed: &mut bool,
) -> Option<Instant> {
    let Some(StoredImage::Named(image)) = images.get_mut(&key) else {
        return None;
    };
    let animation = image.animation.as_mut()?;
    let (next, frame_changed) = animation.advance(now);
    animation.scheduled_at = next;
    *changed |= frame_changed;
    next
}
