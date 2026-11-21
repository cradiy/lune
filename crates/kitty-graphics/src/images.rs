use std::collections::HashMap;
use std::sync::{
    Arc, OnceLock, Weak,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

use super::{
    Command, Controls, Delete, DeleteTarget, EvictionRanking, FeatureReply, INLINE_DECODED_BYTES,
    INLINE_ENCODED_BYTES, ImageChange, MAX_DIRECT_PAYLOAD_BYTES, Placement, PlacementError, Value,
    control_keys,
};
use crate::animation::{
    Animation, Canvas, FrameBudget, FrameComposition, FrameControl, FrameUpload, INDEXED,
    LOAD_FIELDS,
};
use crate::pixels::{INVALID, ImageFormat, PixelTransfer, Pixels, QUOTA, UNSUPPORTED};
use crate::source::{BAD_SOURCE, LocalSource};

mod quota;
mod wakes;
use wakes::AnimationWakes;

static NEXT_IMAGE_REVISION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageKey(u64);

impl ImageKey {
    pub fn id(self) -> u32 {
        u32::try_from(self.0).unwrap_or_default()
    }
}

#[derive(Debug)]
pub struct Image {
    width: u32,
    height: u32,
    revision: u64,
    number: u32,
    age: u64,
    transient: bool,
    format: ImageFormat,
    bytes: Vec<u8>,
    animation: Option<Box<Animation>>,
}

pub struct PixelUpdate<'a> {
    pub origin: [u32; 2],
    pub size: [u32; 2],
    pub bytes_per_row: u32,
    pub bytes: &'a [u8],
}

impl<'a> PixelUpdate<'a> {
    pub(crate) fn full(bytes: &'a [u8], (width, height): (u32, u32), format: ImageFormat) -> Self {
        Self {
            origin: [0, 0],
            size: [width, height],
            bytes_per_row: width * format.bytes_per_pixel(),
            bytes,
        }
    }
}

impl Image {
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn format(&self) -> ImageFormat {
        self.format
    }

    pub fn pixels<'a>(&'a self, scratch: &'a mut Vec<u8>) -> Option<&'a [u8]> {
        if let Some(animation) = &self.animation {
            return animation
                .pixels(&self.bytes, self.dimensions(), scratch)
                .ok();
        }
        Some(&self.bytes)
    }

    pub fn revision(&self) -> u64 {
        self.animation
            .as_ref()
            .and_then(|animation| animation.revision())
            .unwrap_or(self.revision)
    }

    pub fn source_revision(&self) -> u64 {
        self.revision
    }

    /// Returns a replacement region for a pixel buffer at the given revision.
    pub fn pixel_update<'a>(
        &'a self,
        previous: u64,
        scratch: &'a mut Vec<u8>,
    ) -> Option<PixelUpdate<'a>> {
        if let Some(animation) = &self.animation {
            return animation
                .pixel_update(
                    &self.bytes,
                    self.dimensions(),
                    self.revision,
                    previous,
                    scratch,
                )
                .ok();
        }
        Some(PixelUpdate::full(
            &self.bytes,
            self.dimensions(),
            self.format,
        ))
    }

    fn update_animation<T>(
        &mut self,
        operation: impl FnOnce(&mut Animation, &[u8]) -> Result<T, &'static [u8]>,
    ) -> Result<T, &'static [u8]> {
        if self.format != ImageFormat::Rgba {
            return Err(INDEXED);
        }
        if let Some(animation) = self.animation.as_mut() {
            return operation(animation, &self.bytes);
        }
        let mut animation = Animation::default();
        let result = operation(&mut animation, &self.bytes)?;
        self.animation = Some(Box::new(animation));
        Ok(result)
    }

    fn release_unused_original(&mut self) -> usize {
        if !self.bytes.is_empty()
            && self
                .animation
                .as_ref()
                .is_some_and(|animation| !animation.uses_original())
        {
            return std::mem::take(&mut self.bytes).capacity();
        }
        0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub id: u32,
    pub number: u32,
    pub placement_id: u32,
    pub frame_number: u32,
    pub message: &'static [u8],
    pub features: Option<Box<FeatureReply>>,
}

const MISSING: &[u8] = b"ENOENT:Image not found";

struct Transfer {
    key: ImageKey,
    number: u32,
    quiet: u32,
    transient: bool,
    action: u8,
    placement: Option<Placement>,
    frame: Option<FrameUpload>,
    pixels: PixelTransfer,
    local: Option<LocalSource>,
}

enum StoredImage {
    Named(Image),
    Anonymous {
        owner: Weak<Image>,
        bytes: usize,
        revision: u64,
        transient: bool,
    },
}

impl StoredImage {
    fn storage_bytes(&self) -> usize {
        match self {
            Self::Named(image) => image.bytes.capacity(),
            Self::Anonymous { bytes, .. } => *bytes,
        }
    }
}

/// Stores image data and resolves Kitty image IDs and numbers.
pub struct ImageStore {
    max_bytes: usize,
    max_images: usize,
    stored_bytes: usize,
    anonymous_images: usize,
    images: HashMap<ImageKey, StoredImage>,
    pending: Option<Transfer>,
    decode_scratch: Vec<u8>,
    next_id: u32,
    next_anonymous: u64,
    frame_budget: OnceLock<Arc<FrameBudget>>,
    visual_revision: u64,
    storage_revision: u64,
    animation_wakes: AnimationWakes,
}

impl ImageStore {
    pub fn new(max_bytes: usize, max_images: usize) -> Self {
        Self {
            max_bytes,
            max_images,
            stored_bytes: 0,
            anonymous_images: 0,
            images: HashMap::new(),
            pending: None,
            decode_scratch: Vec::new(),
            next_id: 1,
            next_anonymous: u64::from(u32::MAX) + 1,
            frame_budget: OnceLock::new(),
            visual_revision: 0,
            storage_revision: 0,
            animation_wakes: AnimationWakes::default(),
        }
    }

    pub fn get(&self, id: u32) -> Option<&Image> {
        match self.images.get(&ImageKey(u64::from(id)))? {
            StoredImage::Named(image) => Some(image),
            StoredImage::Anonymous { .. } => None,
        }
    }

    pub fn source_revision(&self, key: ImageKey) -> Option<u64> {
        match self.images.get(&key)? {
            StoredImage::Named(image) => Some(image.source_revision()),
            StoredImage::Anonymous {
                owner, revision, ..
            } => (owner.strong_count() != 0).then_some(*revision),
        }
    }

    pub fn visual_revision(&self) -> u64 {
        self.visual_revision
    }

    pub fn storage_revision(&self) -> u64 {
        self.storage_revision
    }

    pub fn animation_deadline(&self) -> Option<Instant> {
        self.animation_wakes.deadline()
    }

    /// Advances due animations using a monotonic clock and updates their next deadline.
    pub fn poll_animations(&mut self, clock: impl FnOnce() -> Instant) {
        if self.animation_wakes.poll(clock, &mut self.images) {
            self.visual_revision = self.visual_revision.wrapping_add(1);
        }
    }

    pub fn newest_id(&self, number: u32) -> Option<u32> {
        self.images
            .iter()
            .filter_map(|(key, entry)| match entry {
                StoredImage::Named(image) if image.number == number && number != 0 => {
                    Some((image.revision, key.id()))
                }
                _ => None,
            })
            .max()
            .map(|(_, id)| id)
    }

    pub fn retain_ids(&mut self, mut keep: impl FnMut(u32) -> bool) {
        let before = self.images.len();
        self.images.retain(|&key, image| {
            let retain = match image {
                StoredImage::Named(_) => keep(key.id()),
                StoredImage::Anonymous { owner, .. } => owner.strong_count() != 0,
            };
            if !retain {
                self.stored_bytes -= image.storage_bytes();
                self.anonymous_images -= usize::from(key.id() == 0);
                if let StoredImage::Named(image) = image
                    && let Some(animation) = image.animation.as_mut()
                {
                    self.animation_wakes.remove(key, animation);
                }
            }
            retain
        });
        self.storage_revision = self
            .storage_revision
            .wrapping_add(u64::from(before != self.images.len()));
    }

    pub fn delete_frame(
        &mut self,
        delete: Delete,
        apply: impl FnOnce(ImageChange<'_>) -> Result<(), PlacementError>,
    ) {
        let DeleteTarget::Frame {
            id,
            image_number,
            number,
        } = delete.target
        else {
            return;
        };
        let Ok(key) = self.existing_key(id, image_number) else {
            return;
        };
        let Some(StoredImage::Named(image)) = self.images.get_mut(&key) else {
            return;
        };
        let previous = image.revision();
        let removed = image
            .animation
            .as_mut()
            .is_some_and(|animation| animation.delete_frame(number));
        if removed {
            self.stored_bytes -= image.release_unused_original();
            if image.revision() != previous {
                self.visual_revision = self.visual_revision.wrapping_add(1);
            }
            self.animation_wakes.update(key, image);
        } else if delete.free_data
            && apply(ImageChange::Remove {
                replaced: Some(key),
                evicted: &[],
            })
            .is_ok()
        {
            self.remove_stored(key);
        }
    }

    pub fn abort_transfer(&mut self) {
        self.pending = None;
    }

    pub fn clear(&mut self) {
        self.abort_transfer();
        self.storage_revision = self
            .storage_revision
            .wrapping_add(u64::from(!self.images.is_empty()));
        self.images.clear();
        self.stored_bytes = 0;
        self.anonymous_images = 0;
        self.animation_wakes = AnimationWakes::default();
    }

    pub fn receive(
        &mut self,
        command: Command<'_>,
        mut apply: impl FnMut(ImageChange<'_>) -> Result<(), PlacementError>,
    ) -> Option<Reply> {
        let controls = &command.controls;
        let quiet = controls.quiet().map_err(|_| INVALID);
        if controls.is_feature_query() {
            let id = number(controls, b'i').unwrap_or_default();
            let image_number = number(controls, b'I');
            let valid = id != 0
                && image_number == Some(0)
                && matches!(controls.get(b'e'), Some(Value::Letter(_)));
            let result = if valid { Ok(()) } else { Err(INVALID) };
            let mut response = reply(
                id,
                image_number.unwrap_or_default(),
                0,
                quiet.unwrap_or_default().unwrap_or_default(),
                quiet.and(result),
            )?;
            if valid {
                response.features = FeatureReply::resolve(controls).map(Box::new);
            }
            return Some(response);
        }
        let action = controls.get(b'a');
        let mut transfer = if let Some(mut pending) = self.pending.take() {
            let continuation = controls.only(const { control_keys(b"amq") })
                && action.is_none_or(|action| action == Value::Letter(pending.action));
            if continuation {
                if let Ok(Some(quiet @ 1..=2)) = quiet {
                    pending.quiet = quiet;
                }
                pending
            } else {
                let placement = pending.placement.map_or(0, |placement| placement.id);
                let id = if pending.number == 0 {
                    pending.key.id()
                } else {
                    0
                };
                return reply(id, pending.number, placement, pending.quiet, Err(INVALID));
            }
        } else {
            let id_field = number(controls, b'i');
            let number_field = number(controls, b'I');
            let id = id_field.unwrap_or_default();
            let image_number = number_field.unwrap_or_default();
            let quiet_value = quiet.ok().flatten().unwrap_or_default();
            let placement_id = number(controls, b'p').unwrap_or_default();
            if quiet.is_err()
                || controls.has_conflicting_image_selectors()
                || id_field.is_none()
                || number_field.is_none()
            {
                return reply(id, image_number, placement_id, quiet_value, Err(INVALID));
            }
            if controls.get(b'a') == Some(Value::Letter(b'p')) {
                let id = if image_number == 0 {
                    id
                } else {
                    self.newest_id(image_number).unwrap_or_default()
                };
                let result = self.place(&command, id, apply);
                return reply(id, image_number, placement_id, quiet_value, result);
            }
            if let Some(Value::Letter(action @ (b'a' | b'c'))) = action {
                let result = self.animation_action(&command, id, image_number, action);
                if action == b'a' && result.is_ok() {
                    return None;
                }
                return reply(id, image_number, 0, quiet_value, result);
            }
            match self.begin(controls, id, image_number, quiet_value) {
                Ok(transfer) => transfer,
                Err(message) => {
                    return reply(id, image_number, placement_id, quiet_value, Err(message));
                }
            }
        };

        let id = transfer.key.id();
        let image_number = transfer.number;
        let placement_id = transfer.placement.map_or(0, |placement| placement.id);
        let quiet_value = transfer.quiet;
        let result = (|| {
            quiet?;
            let local = transfer.local.is_some();
            let format_error = |error| if local { BAD_SOURCE } else { error };
            let length = command.encoded_payload.len();
            let mut inline = [0; INLINE_DECODED_BYTES];
            let decoded = if length <= INLINE_ENCODED_BYTES {
                &mut inline[..]
            } else {
                if local || length > MAX_DIRECT_PAYLOAD_BYTES {
                    return Err(format_error(INVALID));
                }
                let size = base64::decoded_len_estimate(length);
                self.decode_scratch
                    .try_reserve_exact(size.saturating_sub(self.decode_scratch.len()))
                    .map_err(|_| QUOTA)?;
                self.decode_scratch.resize(size, 0);
                &mut self.decode_scratch[..]
            };
            let written = command
                .decode_payload(decoded)
                .map_err(|_| format_error(INVALID))?;
            if let Some(source) = transfer.local.take() {
                if written > crate::MAX_PATH_BYTES {
                    return Err(BAD_SOURCE);
                }
                transfer.pixels.load(source, &decoded[..written])?;
            } else {
                transfer.pixels.append(&decoded[..written])?;
            }

            if controls.get(b'm') == Some(Value::Unsigned(1)) {
                self.pending = Some(transfer);
                return Ok(None);
            }
            let pixels = transfer
                .pixels
                .finish(self.max_bytes)
                .map_err(format_error)?;
            if let Some(frame) = transfer.frame {
                let frame = self.apply_frame(transfer.key, |animation, canvas| {
                    animation.upload(frame, pixels, canvas)
                })?;
                return Ok(Some(frame));
            }
            if transfer.action != b'q' && (id != 0 || transfer.placement.is_some()) {
                self.store(
                    transfer.key,
                    transfer.number,
                    transfer.placement,
                    transfer.transient,
                    pixels,
                    &mut apply,
                )?;
            }
            Ok(Some(0))
        })();
        match result {
            Ok(None) => None,
            Ok(Some(frame_number)) => {
                let mut reply = reply(id, image_number, placement_id, quiet_value, Ok(()))?;
                reply.frame_number = frame_number;
                Some(reply)
            }
            Err(message) => reply(
                if image_number == 0 { id } else { 0 },
                image_number,
                placement_id,
                quiet_value,
                Err(message),
            ),
        }
    }

    fn animation_action(
        &mut self,
        command: &Command<'_>,
        id: u32,
        image_number: u32,
        action: u8,
    ) -> Result<(), &'static [u8]> {
        if !command.encoded_payload.is_empty() {
            return Err(INVALID);
        }
        let key = self.existing_key(id, image_number)?;
        if action == b'a' {
            let control = FrameControl::parse(&command.controls)?;
            self.apply_frame(key, |animation, _| animation.control(control))
        } else {
            let composition = FrameComposition::parse(&command.controls)?;
            self.apply_frame(key, |animation, canvas| {
                animation.compose(composition, canvas)
            })
        }
    }

    fn apply_frame<T>(
        &mut self,
        key: ImageKey,
        apply: impl FnOnce(&mut Animation, Canvas<'_>) -> Result<T, &'static [u8]>,
    ) -> Result<T, &'static [u8]> {
        let Some(StoredImage::Named(image)) = self.images.get_mut(&key) else {
            return Err(MISSING);
        };
        let budget = self
            .frame_budget
            .get_or_init(|| Arc::new(FrameBudget::new(self.max_bytes.saturating_mul(5))));
        let previous = image.revision();
        let dimensions = image.dimensions();
        let revision = NEXT_IMAGE_REVISION.fetch_add(1, Ordering::Relaxed);
        let result = image.update_animation(|animation, original| {
            apply(
                animation,
                Canvas {
                    dimensions,
                    original,
                    budget,
                    revision,
                },
            )
        })?;
        self.stored_bytes -= image.release_unused_original();
        if image.revision() != previous {
            self.visual_revision = self.visual_revision.wrapping_add(1);
        }
        self.animation_wakes.update(key, image);
        Ok(result)
    }

    fn existing_key(&self, id: u32, image_number: u32) -> Result<ImageKey, &'static [u8]> {
        let id = if image_number != 0 {
            self.newest_id(image_number).ok_or(MISSING)?
        } else if id != 0 {
            id
        } else {
            return Err(INVALID);
        };
        Ok(ImageKey(u64::from(id)))
    }

    fn place(
        &mut self,
        command: &Command<'_>,
        id: u32,
        mut apply: impl FnMut(ImageChange<'_>) -> Result<(), PlacementError>,
    ) -> Result<(), &'static [u8]> {
        if !command
            .controls
            .only(const { control_keys(b"aIiq") | Placement::KEYS })
        {
            return Err(UNSUPPORTED);
        }
        if !command.encoded_payload.is_empty() {
            return Err(INVALID);
        }
        let key = ImageKey(u64::from(id));
        let Some(StoredImage::Named(image)) = self.images.get_mut(&key) else {
            return Err(MISSING);
        };
        let placement = Placement::parse(key, &command.controls).ok_or(INVALID)?;
        apply(ImageChange::Place {
            placement,
            dimensions: image.dimensions(),
            replaces_image: false,
            anonymous: None,
            evicted: &[],
        })
        .map_err(placement_error)?;
        image.age = NEXT_IMAGE_REVISION.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn begin(
        &mut self,
        controls: &Controls,
        id: u32,
        image_number: u32,
        quiet: u32,
    ) -> Result<Transfer, &'static [u8]> {
        let action = match controls.get(b'a') {
            None => b't',
            Some(Value::Letter(action @ (b't' | b'T' | b'f' | b'q'))) => action,
            _ => return Err(UNSUPPORTED),
        };
        let display = action == b'T';
        let frame = action == b'f';
        let extra = if display {
            Placement::KEYS
        } else if frame {
            const { control_keys(LOAD_FIELDS) }
        } else {
            0
        };
        if !controls.only(const { control_keys(b"aIfimqstovNSO") } | extra) {
            return Err(UNSUPPORTED);
        }
        if action == b'q' && id == 0 {
            return Err(INVALID);
        }
        let frame = frame.then(|| FrameUpload::parse(controls)).transpose()?;
        let frame_key = if frame.is_some() {
            let key = self.existing_key(id, image_number)?;
            if self.get(key.id()).ok_or(MISSING)?.format() != ImageFormat::Rgba
                || matches!(controls.get(b'f'), Some(Value::Unsigned(8 | 16)))
            {
                return Err(INDEXED);
            }
            Some(key)
        } else {
            None
        };
        let local = LocalSource::parse(controls)?;
        let transient = match controls.get(b'N') {
            None => false,
            Some(Value::Unsigned(hints)) => hints & 1 != 0,
            _ => return Err(INVALID),
        };
        let pixels = PixelTransfer::begin(controls, self.max_bytes, local.is_none())?;
        let key = if let Some(key) = frame_key {
            key
        } else if id != 0 {
            ImageKey(u64::from(id))
        } else if image_number != 0 {
            self.allocate_id().ok_or(QUOTA)?
        } else {
            let key = ImageKey(self.next_anonymous);
            self.next_anonymous = self.next_anonymous.checked_add(1).ok_or(QUOTA)?;
            key
        };
        let placement = if display {
            Some(Placement::parse(key, controls).ok_or(INVALID)?)
        } else {
            None
        };
        Ok(Transfer {
            key,
            number: image_number,
            quiet,
            transient,
            action,
            placement,
            frame,
            pixels,
            local,
        })
    }

    fn allocate_id(&mut self) -> Option<ImageKey> {
        for _ in 0..=self.images.len() {
            let key = ImageKey(u64::from(self.next_id));
            self.next_id = self.next_id.checked_add(1).unwrap_or(1);
            if !self.images.contains_key(&key) {
                return Some(key);
            }
        }
        None
    }

    fn store(
        &mut self,
        key: ImageKey,
        number: u32,
        placement: Option<Placement>,
        transient: bool,
        pixels: Pixels,
        mut apply: impl FnMut(ImageChange<'_>) -> Result<(), PlacementError>,
    ) -> Result<(), &'static [u8]> {
        let previous_bytes = self.images.get(&key).map(StoredImage::storage_bytes);
        let evicted = self.eviction_plan(
            key,
            placement,
            pixels.bytes.capacity(),
            previous_bytes,
            &mut apply,
        )?;
        let replaces_image = previous_bytes.is_some();
        if !replaces_image {
            self.images.try_reserve(1).map_err(|_| QUOTA)?;
        }
        let revision = NEXT_IMAGE_REVISION.fetch_add(1, Ordering::Relaxed);
        let image = Image {
            width: pixels.dimensions.0,
            height: pixels.dimensions.1,
            revision,
            number,
            age: revision,
            transient,
            format: pixels.format,
            bytes: pixels.bytes,
            animation: None,
        };
        let dimensions = image.dimensions();
        let entry = if key.id() == 0 {
            let image = Arc::new(image);
            let entry = StoredImage::Anonymous {
                owner: Arc::downgrade(&image),
                bytes: image.bytes.capacity(),
                revision: image.revision,
                transient,
            };
            let placement = placement.expect("An anonymous image has one placement");
            apply(ImageChange::Place {
                placement,
                dimensions,
                replaces_image: false,
                anonymous: Some(image),
                evicted: &evicted,
            })
            .map_err(placement_error)?;
            entry
        } else {
            if let Some(placement) = placement {
                apply(ImageChange::Place {
                    placement,
                    dimensions,
                    replaces_image,
                    anonymous: None,
                    evicted: &evicted,
                })
                .map_err(placement_error)?;
            } else if replaces_image || !evicted.is_empty() {
                apply(ImageChange::Remove {
                    replaced: replaces_image.then_some(key),
                    evicted: &evicted,
                })
                .map_err(placement_error)?;
            }
            StoredImage::Named(image)
        };
        for victim in evicted {
            self.remove_stored(victim);
        }
        let bytes = entry.storage_bytes();
        if let Some(previous) = self.images.insert(key, entry) {
            self.release_stored(key, previous);
        }
        self.stored_bytes += bytes;
        self.anonymous_images += usize::from(key.id() == 0);
        Ok(())
    }

    fn remove_stored(&mut self, key: ImageKey) {
        if let Some(entry) = self.images.remove(&key) {
            self.release_stored(key, entry);
        }
    }

    fn release_stored(&mut self, key: ImageKey, mut entry: StoredImage) {
        self.storage_revision = self.storage_revision.wrapping_add(1);
        self.stored_bytes -= entry.storage_bytes();
        self.anonymous_images -= usize::from(key.id() == 0);
        if let StoredImage::Named(image) = &mut entry
            && let Some(animation) = image.animation.as_mut()
        {
            self.animation_wakes.remove(key, animation);
        }
    }
}

fn number(controls: &Controls, key: u8) -> Option<u32> {
    match controls.get(key) {
        None => Some(0),
        Some(Value::Unsigned(value)) => Some(value),
        _ => None,
    }
}

fn placement_error(error: PlacementError) -> &'static [u8] {
    match error {
        PlacementError::InvalidRegion => b"EINVAL:Invalid placement region",
        PlacementError::Quota => b"ENOSPC:Placement quota exceeded",
        PlacementError::MissingParent => b"ENOPARENT:Parent placement missing",
        PlacementError::Cycle => b"ECYCLE:Placement cycle",
        PlacementError::TooDeep => b"ETOODEEP:Placement chain too deep",
    }
}

fn reply(
    id: u32,
    number: u32,
    placement_id: u32,
    quiet: u32,
    result: Result<(), &'static [u8]>,
) -> Option<Reply> {
    let message = match result {
        Ok(()) if quiet == 0 => b"OK",
        Err(message) if quiet != 2 => message,
        _ => return None,
    };
    (id != 0 || number != 0).then_some(Reply {
        id,
        number,
        placement_id,
        frame_number: 0,
        message,
        features: None,
    })
}
