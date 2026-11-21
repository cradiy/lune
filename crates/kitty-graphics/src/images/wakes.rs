use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use super::{Animation, Image, ImageKey, StoredImage};

#[derive(Default)]
pub(super) struct AnimationWakes {
    pub(super) spare: Option<Box<Animation>>,
    animated: HashSet<ImageKey>,
    membership: u64,
    pending: HashSet<ImageKey>,
    scheduled: BTreeMap<Instant, Vec<ImageKey>>,
    visibility: Option<(HashSet<ImageKey>, HashSet<ImageKey>)>,
}

impl AnimationWakes {
    pub(super) fn membership(&self) -> Option<u64> {
        (!self.animated.is_empty()).then_some(self.membership)
    }

    pub(super) fn contains(&self, key: ImageKey) -> bool {
        self.animated.contains(&key)
    }

    pub(super) fn clear(&mut self) {
        self.spare = None;
        self.animated.clear();
        self.membership = self.membership.wrapping_add(1);
        self.pending.clear();
        self.scheduled.clear();
        if let Some((visible, scratch)) = &mut self.visibility {
            visible.clear();
            scratch.clear();
        }
    }

    pub(super) fn is_visible(&self, key: ImageKey) -> bool {
        self.visibility
            .as_ref()
            .is_none_or(|(visible, _)| visible.contains(&key))
    }

    pub(super) fn set_visibility(
        &mut self,
        images: &mut HashMap<ImageKey, StoredImage>,
        keys: impl IntoIterator<Item = ImageKey>,
    ) -> bool {
        let unrestricted = self.visibility.is_none();
        let (mut previous, mut visible) = self.visibility.take().unwrap_or_default();
        visible.extend(keys);
        let mut revealed = false;
        let mut prune = false;
        for &key in &self.animated {
            let was_visible = unrestricted || previous.contains(&key);
            let now_visible = visible.contains(&key);
            if was_visible == now_visible {
                continue;
            }
            let Some(StoredImage::Named(image)) = images.get_mut(&key) else {
                continue;
            };
            let animation = image
                .animation
                .as_mut()
                .expect("An animated image has frames");
            if now_visible {
                revealed = true;
                if animation.playing() {
                    self.pending.insert(key);
                }
            } else {
                self.pending.remove(&key);
                prune |= animation.scheduled_at.take().is_some();
                animation.suspend();
            }
        }
        if prune {
            self.scheduled.retain(|_, keys| {
                keys.retain(|key| visible.contains(key));
                !keys.is_empty()
            });
        }
        previous.clear();
        self.visibility = Some((visible, previous));
        revealed
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.scheduled.first_key_value().map(|(&at, _)| at)
    }

    pub(super) fn update(&mut self, key: ImageKey, image: &mut Image) {
        let Some(animation) = image.animation.as_mut() else {
            return;
        };
        if animation.is_redundant() {
            self.remove(key, animation);
            if self.spare.is_none() {
                animation.reset_for_reuse();
                self.spare = image.animation.take();
            } else {
                image.animation = None;
            }
            return;
        }
        self.membership = self
            .membership
            .wrapping_add(u64::from(self.animated.insert(key)));
        self.unschedule(key, animation);
        if animation.playing() && self.is_visible(key) {
            self.pending.insert(key);
        } else {
            self.pending.remove(&key);
        }
    }

    pub(super) fn remove(&mut self, key: ImageKey, animation: &mut Animation) {
        self.membership = self
            .membership
            .wrapping_add(u64::from(self.animated.remove(&key)));
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
    let original_revision = image.source_revision();
    let animation = image.animation.as_mut()?;
    let (next, frame_changed) = animation.advance(now, original_revision);
    animation.scheduled_at = next;
    *changed |= frame_changed;
    next
}
