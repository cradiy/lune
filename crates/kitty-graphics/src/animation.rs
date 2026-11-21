use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Instant;
use std::{num::NonZeroU32, ops::Range};

use crate::{
    Controls, PixelUpdate, PixelUpdates, Value, control_keys,
    images::next_image_revision,
    pixels::{INVALID, ImageFormat, Pixels, QUOTA},
};

mod damage;
mod playback;
use damage::{DamageLink, InheritedDamage, damage_between, split_saves_pixels};
use playback::{Playback, PlaybackMode, TimedFrames};

const MAX_PATCH_DEPTH: u8 = 24;
const DEFAULT_GAP_MS: u32 = 40;
const MIN_SPLIT_SAVINGS_PIXELS: usize = 1024;
pub(crate) const LOAD_FIELDS: &[u8] = b"xycrzXY";
pub(crate) const CONTROL_FIELDS: &[u8] = b"svrcz";
pub(crate) const COMPOSE_FIELDS: &[u8] = b"rcxywhXYC";
pub(crate) const NO_FRAME: &[u8] = b"ENOENT:Animation frame not found";
pub(crate) const INVALID_REGION: &[u8] = b"EINVAL:Invalid animation frame region";
pub(crate) const INDEXED: &[u8] = b"EINVAL:Animation requires RGB or RGBA image data";

#[derive(Clone, Copy)]
pub(crate) struct FrameUpload {
    origin: (u32, u32),
    pub(crate) number: u32,
    base: u32,
    gap_ms: Option<u32>,
    background: [u8; 4],
    overwrite: bool,
}

impl FrameUpload {
    pub(crate) fn parse(controls: &Controls) -> Result<Self, &'static [u8]> {
        Ok(Self {
            origin: (unsigned(controls, b'x')?, unsigned(controls, b'y')?),
            number: unsigned(controls, b'r')?,
            base: unsigned(controls, b'c')?,
            gap_ms: gap(controls)?,
            background: unsigned(controls, b'Y')?.to_be_bytes(),
            overwrite: overwrite(controls, b'X')?,
        })
    }

    pub(crate) fn resolve_number(&mut self, animation: Option<&Animation>) {
        let extra = animation.map_or(0, |animation| animation.extra.len());
        let append = u32::try_from(extra.saturating_add(2)).unwrap_or(u32::MAX);
        if self.number == 0 || self.number > append {
            self.number = append;
        }
    }
}

pub(crate) struct FrameComposition {
    source: u32,
    target: u32,
    source_origin: (u32, u32),
    target_origin: (u32, u32),
    size: (u32, u32),
    overwrite: bool,
}

impl FrameComposition {
    pub(crate) fn parse(controls: &Controls) -> Result<Self, &'static [u8]> {
        if !controls.only(const { control_keys(b"aiIq") | control_keys(COMPOSE_FIELDS) }) {
            return Err(crate::pixels::UNSUPPORTED);
        }
        Ok(Self {
            source: unsigned(controls, b'r')?,
            target: unsigned(controls, b'c')?,
            source_origin: (unsigned(controls, b'X')?, unsigned(controls, b'Y')?),
            target_origin: (unsigned(controls, b'x')?, unsigned(controls, b'y')?),
            size: (unsigned(controls, b'w')?, unsigned(controls, b'h')?),
            overwrite: overwrite(controls, b'C')?,
        })
    }
}

fn overwrite(controls: &Controls, key: u8) -> Result<bool, &'static [u8]> {
    match unsigned(controls, key)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(INVALID),
    }
}

pub(crate) struct FrameControl {
    select: u32,
    gap_index: u32,
    gap_ms: Option<u32>,
    mode: Option<PlaybackMode>,
    loops: u32,
}

impl FrameControl {
    pub(crate) fn parse(controls: &Controls) -> Result<Self, &'static [u8]> {
        if !controls.only(const { control_keys(b"aiIq") | control_keys(CONTROL_FIELDS) }) {
            return Err(crate::pixels::UNSUPPORTED);
        }
        Ok(Self {
            select: unsigned(controls, b'c')?,
            gap_index: unsigned(controls, b'r')?,
            gap_ms: gap(controls)?,
            mode: match unsigned(controls, b's')? {
                0 => None,
                1 => Some(PlaybackMode::Stopped),
                2 => Some(PlaybackMode::Loading),
                3 => Some(PlaybackMode::Running),
                _ => return Err(INVALID),
            },
            loops: unsigned(controls, b'v')?,
        })
    }
}

fn unsigned(controls: &Controls, key: u8) -> Result<u32, &'static [u8]> {
    match controls.get(key) {
        None => Ok(0),
        Some(Value::Unsigned(value)) => Ok(value),
        _ => Err(INVALID),
    }
}

fn gap(controls: &Controls) -> Result<Option<u32>, &'static [u8]> {
    match controls.get(b'z') {
        None | Some(Value::Unsigned(0) | Value::Signed(0)) => Ok(None),
        Some(Value::Unsigned(value)) if i32::try_from(value).is_ok() => Ok(Some(value)),
        Some(Value::Signed(..=0)) => Ok(Some(0)),
        _ => Err(INVALID),
    }
}

pub(crate) struct FrameBudget {
    used: AtomicUsize,
    limit: usize,
}

pub(crate) struct Canvas<'a> {
    pub(crate) dimensions: (u32, u32),
    pub(crate) original: &'a [u8],
    pub(crate) original_revision: u64,
    pub(crate) budget: &'a Arc<FrameBudget>,
    pub(crate) revision: u64,
}

impl FrameBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            used: AtomicUsize::new(0),
            limit,
        }
    }

    fn store(
        self: &Arc<Self>,
        data: SurfaceData,
        revision: u64,
        original_revision: u64,
    ) -> Result<Arc<Surface>, &'static [u8]> {
        // Include the surface allocation and allowance for the frame list's spare capacity.
        let overhead = size_of::<Surface>() + 2 * (size_of::<Frame>() + size_of::<usize>());
        let bytes = data.capacity().checked_add(overhead).ok_or(QUOTA)?;
        let charge = self.charge(bytes)?;
        let ancestors = match &data {
            SurfaceData::Patch(Patch {
                base: Base::Frame(parent),
                ..
            }) => DamageLink::from_surface(parent, original_revision, self),
            _ => None,
        };
        Ok(Arc::new(Surface {
            revision,
            data,
            ancestors,
            _charge: charge,
        }))
    }

    fn charge(self: &Arc<Self>, bytes: usize) -> Result<FrameCharge, &'static [u8]> {
        self.used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|&total| total <= self.limit)
            })
            .map_err(|_| QUOTA)?;
        Ok(FrameCharge {
            budget: Arc::clone(self),
            bytes,
        })
    }
}

#[derive(Debug, Default)]
struct Frame {
    surface: Option<Arc<Surface>>,
    gap_ms: u32,
    parent: Option<NonZeroU32>,
}

struct FrameReplacement {
    index: usize,
    parent: Option<NonZeroU32>,
    surface: Arc<Surface>,
    flat_base: Option<Arc<Surface>>,
}

impl FrameReplacement {
    fn new(index: usize, parent: Option<NonZeroU32>, surface: Arc<Surface>) -> Self {
        Self {
            index,
            parent,
            surface,
            flat_base: None,
        }
    }

    fn rebase(&mut self, patch: &Patch, canvas: &Canvas<'_>) -> Result<SurfaceData, &'static [u8]> {
        let base = self.flat_base.as_ref().unwrap_or(&self.surface);
        let mut patch = Patch::new(
            Base::Frame(Arc::clone(base)),
            patch.region,
            patch.stride,
            patch.pixels.share(),
            patch.blend,
        );
        if patch.depth > MAX_PATCH_DEPTH {
            // Flatten the shared base so the dependent retains its live patch.
            let mut pixels = Vec::new();
            self.surface
                .data
                .pixels(canvas.original, canvas.dimensions, &mut pixels)?;
            let base = canvas.budget.store(
                SurfaceData::Full(pixels),
                self.surface.revision,
                canvas.original_revision,
            )?;
            if self.parent.is_none() {
                self.surface = Arc::clone(&base);
            } else {
                self.flat_base = Some(Arc::clone(&base));
            }
            patch.base = Base::Frame(base);
            patch.depth = 1;
            patch.uses_original = false;
        }
        Ok(SurfaceData::Patch(patch))
    }
}

impl Frame {
    fn uses_original_surface(&self) -> bool {
        self.surface
            .as_ref()
            .is_some_and(|surface| surface.data.uses_original())
    }
}

#[derive(Debug, Default)]
pub(crate) struct Animation {
    root: Frame,
    extra: Vec<Frame>,
    timed_extra: TimedFrames,
    current: usize,
    original_users: usize,
    linked_frames: usize,
    total_gap_ms: u64,
    playback: Playback,
    inherited_damage: InheritedDamage,
    pub(crate) scheduled_at: Option<Instant>,
}

#[derive(Clone)]
pub struct PixelRevision {
    revision: u64,
    patch: Option<(u64, Region)>,
    ancestors: Option<Arc<DamageLink>>,
}

impl std::fmt::Debug for PixelRevision {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("PixelRevision")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

impl PartialEq for PixelRevision {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision
    }
}

impl Eq for PixelRevision {}

impl PixelRevision {
    pub fn id(&self) -> u64 {
        self.revision
    }

    pub(crate) fn full(revision: u64) -> Self {
        Self {
            revision,
            patch: None,
            ancestors: None,
        }
    }
}

impl Animation {
    pub(crate) fn uses_original(&self) -> bool {
        self.root.surface.is_none() || self.original_users != 0
    }

    pub(crate) fn revision(&self) -> Option<u64> {
        self.frame_revision(self.current)
    }

    fn frame_revision(&self, index: usize) -> Option<u64> {
        self.frame(index)?
            .surface
            .as_ref()
            .map(|surface| surface.revision)
    }

    pub(crate) fn pixel_revision(&self, original_revision: u64) -> PixelRevision {
        let Some(surface) = self.current_surface() else {
            return PixelRevision::full(original_revision);
        };
        PixelRevision {
            revision: surface.revision,
            patch: surface.patch_edge(original_revision),
            ancestors: surface.ancestors.clone(),
        }
    }

    pub(crate) fn pixels<'a>(
        &'a self,
        original: &'a [u8],
        dimensions: (u32, u32),
        scratch: &'a mut Vec<u8>,
    ) -> Result<&'a [u8], &'static [u8]> {
        match self.current_surface() {
            Some(surface) => surface.data.pixels(original, dimensions, scratch),
            None => Ok(original),
        }
    }

    pub(crate) fn pixel_region<'a>(
        &'a self,
        original: &'a [u8],
        width: usize,
        region: Region,
        scratch: &'a mut Vec<u8>,
    ) -> Result<PixelUpdate<'a>, &'static [u8]> {
        render_region(
            self.current_surface().map(|surface| &surface.data),
            original,
            width,
            region,
            scratch,
        )
    }

    pub(crate) fn pixel_updates<'a>(
        &'a self,
        original: &'a [u8],
        dimensions: (u32, u32),
        original_revision: u64,
        previous: Option<&PixelRevision>,
        scratch: &'a mut Vec<u8>,
    ) -> Result<PixelUpdates<'a>, &'static [u8]> {
        let surface = self.current_surface();
        let data = surface.map(|surface| &surface.data);
        if let Some(previous) = previous {
            if let Some([first, second]) = surface.and_then(|surface| {
                self.inherited_damage
                    .since(previous.revision, surface.revision)
            }) {
                return if second.is_empty() {
                    render_region(data, original, dimensions.0 as usize, first, scratch)
                        .map(PixelUpdates::One)
                } else {
                    render_two_regions(
                        data,
                        original,
                        dimensions.0 as usize,
                        [first, second],
                        scratch,
                    )
                };
            }
            let mut region = None;
            if let Some(data @ SurfaceData::Patch(patch)) = data
                && let Some(base) = patch.base.revision(original_revision)
            {
                if base == previous.revision {
                    if !patch.blend {
                        return Ok(PixelUpdates::One(
                            patch.region.update(patch.pixels.bytes(), patch.stride),
                        ));
                    }
                    region = Some(patch.region);
                } else if let Some((previous_base, previous_region)) = previous.patch
                    && base == previous_base
                {
                    let union = patch.region.union(previous_region);
                    if split_saves_pixels(patch.region, previous_region, union) {
                        return data.update_two_regions(
                            original,
                            dimensions.0 as usize,
                            [previous_region, patch.region],
                            scratch,
                        );
                    }
                    region = Some(union);
                }
            }
            if let Some((base, previous_region)) = previous.patch
                && base == surface.map_or(original_revision, |surface| surface.revision)
            {
                region = Some(previous_region);
            }
            if region.is_none()
                && (previous.ancestors.is_some()
                    || surface.is_some_and(|surface| surface.ancestors.is_some()))
                && let Some([first, second]) = damage_between(surface, previous, original_revision)
            {
                if !second.is_empty() {
                    return render_two_regions(
                        data,
                        original,
                        dimensions.0 as usize,
                        [first, second],
                        scratch,
                    );
                }
                region = Some(first);
            }
            if let Some(region) = region {
                return render_region(data, original, dimensions.0 as usize, region, scratch)
                    .map(PixelUpdates::One);
            }
        }
        let bytes = match data {
            Some(SurfaceData::Full(bytes)) => bytes,
            Some(SurfaceData::Patch(_)) => return Ok(PixelUpdates::Rasterize),
            Some(SurfaceData::Original) | None => original,
        };
        Ok(PixelUpdates::One(PixelUpdate::full(
            bytes,
            dimensions,
            ImageFormat::Rgba,
        )))
    }

    fn current_surface(&self) -> Option<&Surface> {
        self.frame(self.current)?.surface.as_deref()
    }

    pub(crate) fn upload(
        &mut self,
        upload: FrameUpload,
        pixels: Pixels,
        canvas: Canvas<'_>,
    ) -> Result<(), &'static [u8]> {
        let Canvas {
            dimensions,
            budget,
            revision,
            ..
        } = canvas;
        if pixels.format != ImageFormat::Rgba {
            return Err(INDEXED);
        }
        let (width, height) = dimensions;
        if pixels.dimensions.0 > width || pixels.dimensions.1 > height {
            return Err(INVALID_REGION);
        }
        let index = upload.number.checked_sub(1).ok_or(NO_FRAME)? as usize;
        if index > self.extra.len() + 1 {
            return Err(NO_FRAME);
        }
        let existing = self.frame(index);
        let mut parent = NonZeroU32::new(upload.base).filter(|_| existing.is_none());
        let base = if let Some(frame) = existing {
            Base::from_frame(frame)
        } else if let Some(parent) = parent {
            Base::from_frame(
                self.frame(parent.get() as usize - 1)
                    .ok_or(b"EINVAL:Animation base frame not found".as_slice())?,
            )
        } else {
            Base::Color(upload.background)
        };
        let (x, y) = upload.origin;
        let (patch_width, patch_height) = pixels.dimensions;
        let region = Region {
            x: x as usize,
            y: y as usize,
            width: patch_width.min(width.saturating_sub(x)) as usize,
            height: patch_height.min(height.saturating_sub(y)) as usize,
        };
        let is_full = upload.origin == (0, 0) && pixels.dimensions == dimensions;
        let data = if is_full
            && (upload.overwrite
                || matches!(base, Base::Color([_, _, _, 0]))
                || pixels
                    .bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|pixel| pixel[3] == 255))
        {
            SurfaceData::Full(pixels.bytes)
        } else {
            let (bytes, stride) =
                region.retain_patch_pixels(pixels.bytes, patch_width as usize * 4)?;
            let pixels = if parent.is_some() {
                PatchPixels::shared(bytes, budget)?
            } else {
                PatchPixels::Owned(bytes)
            };
            Patch::new(base, region, stride, pixels, !upload.overwrite).into_data(&canvas)?
        };
        if matches!(data, SurfaceData::Full(_)) {
            parent = None;
        }
        let surface = budget.store(data, revision, canvas.original_revision)?;
        if existing.is_some() {
            let replacement = FrameReplacement::new(index, parent, surface);
            self.edit_frame(replacement, region, upload.gap_ms, &canvas)?;
        } else {
            self.extra.try_reserve(1).map_err(|_| QUOTA)?;
            let gap_ms = upload.gap_ms.unwrap_or(DEFAULT_GAP_MS);
            self.timed_extra.push(index - 1, gap_ms != 0)?;
            self.original_users += usize::from(surface.data.uses_original());
            self.linked_frames += usize::from(parent.is_some());
            self.extra.push(Frame {
                surface: Some(surface),
                gap_ms,
                parent,
            });
            self.total_gap_ms += u64::from(gap_ms);
        }
        Ok(())
    }

    fn edit_frame(
        &mut self,
        replacement: FrameReplacement,
        region: Region,
        gap_ms: Option<u32>,
        canvas: &Canvas<'_>,
    ) -> Result<(), &'static [u8]> {
        let index = replacement.index;
        if self.linked_frames == 0 {
            self.replace_surface(replacement);
        } else {
            self.replace_with_dependents(replacement, region, canvas)?;
        }
        if let Some(gap_ms) = gap_ms {
            self.set_gap(index, gap_ms);
        }
        if index == self.current {
            self.playback.restart_frame();
        }
        Ok(())
    }

    fn replace_with_dependents(
        &mut self,
        mut edited: FrameReplacement,
        region: Region,
        canvas: &Canvas<'_>,
    ) -> Result<(), &'static [u8]> {
        let mut replacements: Vec<FrameReplacement> = Vec::new();
        // A live base always precedes its dependents in frame order.
        for (position, frame) in self.extra.iter().enumerate().skip(edited.index) {
            let Some(parent) = frame.parent else { continue };
            let parent_index = parent.get() as usize - 1;
            let base = if parent_index == edited.index {
                &mut edited
            } else if let Ok(other) =
                replacements.binary_search_by_key(&parent_index, |entry| entry.index)
            {
                &mut replacements[other]
            } else {
                continue;
            };
            let Some(Surface {
                data: SurfaceData::Patch(patch),
                ..
            }) = frame.surface.as_deref()
            else {
                unreachable!("A dependent frame has a patch surface");
            };
            let data = base.rebase(patch, canvas)?;
            let surface =
                canvas
                    .budget
                    .store(data, next_image_revision(), canvas.original_revision)?;
            replacements.try_reserve(1).map_err(|_| QUOTA)?;
            replacements.push(FrameReplacement::new(position + 1, Some(parent), surface));
        }
        if let Ok(current) =
            replacements.binary_search_by_key(&self.current, |replacement| replacement.index)
        {
            self.inherited_damage.record(
                self.revision().expect("A dependent frame has a revision"),
                replacements[current].surface.revision,
                region,
            );
        }
        self.replace_surface(edited);
        for replacement in replacements {
            self.replace_surface(replacement);
        }
        Ok(())
    }

    fn replace_surface(&mut self, replacement: FrameReplacement) {
        let previous = self.frame(replacement.index).expect("Existing frame");
        let original_users = self.original_users - usize::from(previous.uses_original_surface())
            + usize::from(replacement.surface.data.uses_original());
        let linked_frames = self.linked_frames - usize::from(previous.parent.is_some())
            + usize::from(replacement.parent.is_some());
        let frame = self.frame_mut(replacement.index).expect("Existing frame");
        frame.parent = replacement.parent;
        frame.surface = Some(replacement.surface);
        self.original_users = original_users;
        self.linked_frames = linked_frames;
    }

    pub(crate) fn compose(
        &mut self,
        composition: FrameComposition,
        canvas: Canvas<'_>,
    ) -> Result<(), &'static [u8]> {
        let Canvas {
            dimensions,
            original,
            ..
        } = canvas;
        let source = composition.source.checked_sub(1).ok_or(NO_FRAME)? as usize;
        let target = composition.target.checked_sub(1).ok_or(NO_FRAME)? as usize;
        let frame = self.frame(source).ok_or(NO_FRAME)?;
        self.frame(target).ok_or(NO_FRAME)?;
        let size = (
            if composition.size.0 == 0 {
                dimensions.0
            } else {
                composition.size.0
            },
            if composition.size.1 == 0 {
                dimensions.1
            } else {
                composition.size.1
            },
        );
        let source_region = Region::checked(composition.source_origin, size, dimensions)?;
        let target_region = Region::checked(composition.target_origin, size, dimensions)?;
        if source == target && source_region.intersection(target_region).is_some() {
            return Err(INVALID_REGION);
        }
        if composition.overwrite && size == dimensions {
            let surface = match &frame.surface {
                Some(surface) => Arc::clone(surface),
                None => canvas.budget.store(
                    SurfaceData::Original,
                    canvas.original_revision,
                    canvas.original_revision,
                )?,
            };
            let replacement = FrameReplacement::new(target, None, surface);
            return self.edit_frame(replacement, target_region, None, &canvas);
        }
        let mut bytes = Vec::new();
        let len = source_region.width * source_region.height * 4;
        bytes.try_reserve_exact(len).map_err(|_| QUOTA)?;
        bytes.resize(len, 0);
        let width = dimensions.0 as usize;
        if let Some(surface) = &frame.surface {
            surface
                .data
                .render(original, width, source_region, &mut bytes);
        } else {
            source_region.copy_pixels(original, width, &mut bytes);
        }
        self.upload(
            FrameUpload {
                origin: composition.target_origin,
                number: composition.target,
                base: 0,
                gap_ms: None,
                background: [0; 4],
                overwrite: composition.overwrite,
            },
            Pixels {
                dimensions: size,
                format: ImageFormat::Rgba,
                bytes,
            },
            canvas,
        )
    }

    pub(crate) fn control(&mut self, command: FrameControl) {
        let select = command.select.checked_sub(1).map(|index| index as usize);
        let gap = command.gap_index.checked_sub(1).map(|index| index as usize);
        if let Some((index, gap_ms)) = gap.zip(command.gap_ms)
            && index <= self.extra.len()
        {
            self.set_gap(index, gap_ms);
        }
        if let Some(index) = select
            && index != self.current
            && index <= self.extra.len()
        {
            self.current = index;
            self.playback.restart_frame();
        }
        self.playback.configure(command.mode, command.loops);
    }

    pub(crate) fn delete_frame(&mut self, number: u32) -> bool {
        if self.extra.is_empty() {
            return false;
        }
        let index = (number.saturating_sub(1) as usize).min(self.extra.len());
        let removed = if index == 0 {
            std::mem::replace(&mut self.root, self.extra.remove(0))
        } else {
            self.extra.remove(index - 1)
        };
        self.timed_extra
            .remove(index.saturating_sub(1), self.extra.len());
        self.original_users -= usize::from(removed.uses_original_surface());
        self.linked_frames -= usize::from(removed.parent.is_some());
        if self.linked_frames != 0 {
            let number = index + 1;
            for frame in std::iter::once(&mut self.root).chain(&mut self.extra) {
                if let Some(parent) = frame.parent {
                    let parent_number = parent.get() as usize;
                    if parent_number == number {
                        frame.parent = None;
                        self.linked_frames -= 1;
                    } else if parent_number > number {
                        frame.parent = NonZeroU32::new(parent.get() - 1);
                    }
                }
            }
        }
        self.total_gap_ms -= u64::from(removed.gap_ms);
        if self.extra.capacity() > self.extra.len().saturating_mul(4).max(8) {
            self.extra.shrink_to_fit();
            self.timed_extra.shrink_to_fit();
        }
        if self.current > index {
            self.current -= 1;
        } else if self.current == index {
            self.current = self.current.min(self.extra.len());
            self.playback.restart_frame();
        }
        true
    }

    fn set_gap(&mut self, index: usize, gap_ms: u32) {
        let frame = self.frame_mut(index).expect("Frame index was validated");
        let old = std::mem::replace(&mut frame.gap_ms, gap_ms);
        if index != 0 && (old == 0) != (gap_ms == 0) {
            self.timed_extra.set(index - 1, gap_ms != 0);
        }
        self.total_gap_ms = self.total_gap_ms - u64::from(old) + u64::from(gap_ms);
    }

    fn frame(&self, index: usize) -> Option<&Frame> {
        if index == 0 {
            Some(&self.root)
        } else {
            self.extra.get(index - 1)
        }
    }

    fn frame_mut(&mut self, index: usize) -> Option<&mut Frame> {
        if index == 0 {
            Some(&mut self.root)
        } else {
            self.extra.get_mut(index - 1)
        }
    }
}

struct Surface {
    revision: u64,
    data: SurfaceData,
    ancestors: Option<Arc<DamageLink>>,
    _charge: FrameCharge,
}

impl Surface {
    fn patch_edge(&self, original_revision: u64) -> Option<(u64, Region)> {
        match &self.data {
            SurfaceData::Patch(patch) => patch
                .base
                .revision(original_revision)
                .map(|base| (base, patch.region)),
            SurfaceData::Full(_) | SurfaceData::Original => None,
        }
    }
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Surface")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

struct FrameCharge {
    budget: Arc<FrameBudget>,
    bytes: usize,
}

impl Drop for FrameCharge {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

enum SurfaceData {
    Original,
    Full(Vec<u8>),
    Patch(Patch),
}

impl SurfaceData {
    fn update_two_regions<'a>(
        &self,
        original: &[u8],
        width: usize,
        [first, second]: [Region; 2],
        scratch: &'a mut Vec<u8>,
    ) -> Result<PixelUpdates<'a>, &'static [u8]> {
        let first_len = first.area() * 4;
        resize_scratch(scratch, first_len + second.area() * 4)?;
        let (first_bytes, second_bytes) = scratch.split_at_mut(first_len);
        self.render(original, width, first, first_bytes);
        self.render(original, width, second, second_bytes);
        Ok(PixelUpdates::Two([
            first.update(first_bytes, first.width * 4),
            second.update(second_bytes, second.width * 4),
        ]))
    }

    fn uses_original(&self) -> bool {
        matches!(self, Self::Original) || matches!(self, Self::Patch(patch) if patch.uses_original)
    }

    fn capacity(&self) -> usize {
        match self {
            Self::Original => 0,
            Self::Full(pixels) => pixels.capacity(),
            Self::Patch(patch) => patch.pixels.capacity(),
        }
    }

    fn pixels<'a>(
        &'a self,
        original: &'a [u8],
        dimensions: (u32, u32),
        scratch: &'a mut Vec<u8>,
    ) -> Result<&'a [u8], &'static [u8]> {
        match self {
            Self::Original => Ok(original),
            Self::Full(pixels) => Ok(pixels),
            Self::Patch(_) => {
                let (width, height) = (dimensions.0 as usize, dimensions.1 as usize);
                resize_scratch(scratch, width * height * 4)?;
                let region = Region {
                    x: 0,
                    y: 0,
                    width,
                    height,
                };
                self.render(original, width, region, scratch);
                Ok(scratch)
            }
        }
    }

    fn render(&self, original: &[u8], width: usize, region: Region, output: &mut [u8]) {
        match self {
            Self::Original => region.copy_pixels(original, width, output),
            Self::Full(pixels) => region.copy_pixels(pixels, width, output),
            Self::Patch(patch) => {
                match &patch.base {
                    Base::Color([0, 0, 0, 0]) => output.fill(0),
                    Base::Color(color) => {
                        for pixel in output.as_chunks_mut::<4>().0 {
                            *pixel = *color;
                        }
                    }
                    Base::Original => region.copy_pixels(original, width, output),
                    Base::Frame(surface) => surface.data.render(original, width, region, output),
                }
                patch.apply(region, output);
            }
        }
    }
}

fn render_region<'a>(
    data: Option<&'a SurfaceData>,
    original: &'a [u8],
    width: usize,
    region: Region,
    scratch: &'a mut Vec<u8>,
) -> Result<PixelUpdate<'a>, &'static [u8]> {
    if region.is_empty() {
        return Ok(region.update(&[], 0));
    }
    match data {
        Some(SurfaceData::Original) | None => Ok(region.update_from(original, width, 4)),
        Some(SurfaceData::Full(bytes)) => Ok(region.update_from(bytes, width, 4)),
        Some(data) => {
            let stride = region.width * 4;
            resize_scratch(scratch, stride * region.height)?;
            data.render(original, width, region, scratch);
            Ok(region.update(scratch, stride))
        }
    }
}

fn render_two_regions<'a>(
    data: Option<&'a SurfaceData>,
    original: &'a [u8],
    width: usize,
    regions: [Region; 2],
    scratch: &'a mut Vec<u8>,
) -> Result<PixelUpdates<'a>, &'static [u8]> {
    match data {
        Some(SurfaceData::Original) | None => Ok(PixelUpdates::Two(
            regions.map(|region| region.update_from(original, width, 4)),
        )),
        Some(SurfaceData::Full(pixels)) => Ok(PixelUpdates::Two(
            regions.map(|region| region.update_from(pixels, width, 4)),
        )),
        Some(data) => data.update_two_regions(original, width, regions, scratch),
    }
}

fn resize_scratch(scratch: &mut Vec<u8>, len: usize) -> Result<(), &'static [u8]> {
    scratch
        .try_reserve_exact(len.saturating_sub(scratch.len()))
        .map_err(|_| QUOTA)?;
    scratch.resize(len, 0);
    Ok(())
}

#[derive(Clone)]
enum Base {
    Color([u8; 4]),
    Original,
    Frame(Arc<Surface>),
}

impl Base {
    fn uses_original(&self) -> bool {
        match self {
            Self::Color(_) => false,
            Self::Original => true,
            Self::Frame(surface) => surface.data.uses_original(),
        }
    }

    fn revision(&self, original: u64) -> Option<u64> {
        match self {
            Self::Color(_) => None,
            Self::Original => Some(original),
            Self::Frame(surface) => Some(surface.revision),
        }
    }

    fn from_frame(frame: &Frame) -> Self {
        match &frame.surface {
            Some(surface) => Self::Frame(Arc::clone(surface)),
            None => Self::Original,
        }
    }

    fn depth(&self) -> u8 {
        match self {
            Self::Frame(surface) => match &surface.data {
                SurfaceData::Patch(patch) => patch.depth,
                SurfaceData::Full(_) | SurfaceData::Original => 0,
            },
            _ => 0,
        }
    }

    fn without_covered_patches(mut self, region: Region) -> Self {
        while let Self::Frame(surface) = &self
            && let SurfaceData::Patch(patch) = &surface.data
            && region.contains(patch.region)
        {
            self = patch.base.clone();
        }
        self
    }
}

struct Patch {
    base: Base,
    region: Region,
    stride: usize,
    pixels: PatchPixels,
    blend: bool,
    depth: u8,
    uses_original: bool,
}

enum PatchPixels {
    Owned(Vec<u8>),
    Shared(Arc<SharedPatchPixels>),
}

struct SharedPatchPixels {
    bytes: Vec<u8>,
    _charge: FrameCharge,
}

impl PatchPixels {
    fn shared(bytes: Vec<u8>, budget: &Arc<FrameBudget>) -> Result<Self, &'static [u8]> {
        let capacity = bytes
            .capacity()
            .checked_add(size_of::<SharedPatchPixels>())
            .ok_or(QUOTA)?;
        Ok(Self::Shared(Arc::new(SharedPatchPixels {
            bytes,
            _charge: budget.charge(capacity)?,
        })))
    }

    fn share(&self) -> Self {
        let Self::Shared(pixels) = self else {
            unreachable!("A live dependency shares its pixels");
        };
        Self::Shared(Arc::clone(pixels))
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Shared(pixels) => &pixels.bytes,
        }
    }

    fn capacity(&self) -> usize {
        match self {
            Self::Owned(bytes) => bytes.capacity(),
            Self::Shared(_) => 0,
        }
    }
}

impl Patch {
    fn new(base: Base, region: Region, stride: usize, pixels: PatchPixels, blend: bool) -> Self {
        let base = if blend {
            base
        } else {
            base.without_covered_patches(region)
        };
        Self {
            depth: base.depth() + 1,
            uses_original: base.uses_original(),
            base,
            region,
            stride,
            pixels,
            blend,
        }
    }

    fn into_data(self, canvas: &Canvas<'_>) -> Result<SurfaceData, &'static [u8]> {
        let flatten = self.depth > MAX_PATCH_DEPTH;
        let patch = SurfaceData::Patch(self);
        if flatten {
            let mut scratch = Vec::new();
            patch.pixels(canvas.original, canvas.dimensions, &mut scratch)?;
            return Ok(SurfaceData::Full(scratch));
        }
        Ok(patch)
    }

    fn apply(&self, target: Region, output: &mut [u8]) {
        let Some(overlap) = self.region.intersection(target) else {
            return;
        };
        let rows = overlap.relative_to(target).rows(target.width * 4);
        let overlay_rows = overlap.relative_to(self.region).rows(self.stride);
        let pixels = self.pixels.bytes();
        for (target, source) in rows.zip(overlay_rows) {
            let target = &mut output[target];
            let source = &pixels[source];
            if self.blend {
                for (dst, src) in target
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(source.as_chunks::<4>().0)
                {
                    blend(dst, src);
                }
            } else {
                target.copy_from_slice(source);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Region {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl Region {
    fn area(self) -> usize {
        self.width * self.height
    }

    fn retain_patch_pixels(
        self,
        pixels: Vec<u8>,
        source_stride: usize,
    ) -> Result<(Vec<u8>, usize), &'static [u8]> {
        if self.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let stride = self.width * 4;
        let len = stride * self.height;
        // Keep the upload's stride when packing would save less than a quarter of its bytes.
        if pixels.len() - len < pixels.len().div_ceil(4) {
            return Ok((pixels, source_stride));
        }
        let mut retained = Vec::new();
        retained.try_reserve_exact(len).map_err(|_| QUOTA)?;
        if stride == source_stride {
            retained.extend_from_slice(&pixels[..len]);
        } else {
            for row in pixels.chunks_exact(source_stride).take(self.height) {
                retained.extend_from_slice(&row[..stride]);
            }
        }
        Ok((retained, stride))
    }

    pub(crate) fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            width: (self.x + self.width).max(other.x + other.width) - x,
            height: (self.y + self.height).max(other.y + other.height) - y,
        }
    }

    fn update(self, bytes: &[u8], stride: usize) -> PixelUpdate<'_> {
        PixelUpdate {
            origin: [self.x as u32, self.y as u32],
            size: [self.width as u32, self.height as u32],
            bytes_per_row: stride as u32,
            bytes,
        }
    }

    pub(crate) fn update_from(
        self,
        pixels: &[u8],
        width: usize,
        bytes_per_pixel: usize,
    ) -> PixelUpdate<'_> {
        let stride = width * bytes_per_pixel;
        let start = self.y * stride + self.x * bytes_per_pixel;
        let len = (self.height - 1) * stride + self.width * bytes_per_pixel;
        self.update(&pixels[start..start + len], stride)
    }

    pub(crate) fn checked(
        (x, y): (u32, u32),
        (width, height): (u32, u32),
        (canvas_width, canvas_height): (u32, u32),
    ) -> Result<Self, &'static [u8]> {
        if x.checked_add(width)
            .is_none_or(|right| right > canvas_width)
            || y.checked_add(height)
                .is_none_or(|bottom| bottom > canvas_height)
        {
            return Err(INVALID_REGION);
        }
        Ok(Self {
            x: x as usize,
            y: y as usize,
            width: width as usize,
            height: height as usize,
        })
    }

    fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x + self.width).min(other.x + other.width);
        let bottom = (self.y + self.height).min(other.y + other.height);
        (right > x && bottom > y).then(|| Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        })
    }

    fn contains(self, other: Self) -> bool {
        self.x <= other.x
            && self.y <= other.y
            && other.x + other.width <= self.x + self.width
            && other.y + other.height <= self.y + self.height
    }

    fn relative_to(self, parent: Self) -> Self {
        Self {
            x: self.x - parent.x,
            y: self.y - parent.y,
            ..self
        }
    }

    fn rows(self, stride: usize) -> impl Iterator<Item = Range<usize>> {
        (self.y..self.y + self.height).map(move |row| {
            let start = row * stride + self.x * 4;
            start..start + self.width * 4
        })
    }

    fn copy_pixels(self, pixels: &[u8], width: usize, output: &mut [u8]) {
        let stride = width * 4;
        if self.width == width {
            let start = self.y * stride;
            output.copy_from_slice(&pixels[start..start + output.len()]);
            return;
        }
        for (out, row) in output
            .chunks_exact_mut(self.width * 4)
            .zip(self.rows(stride))
        {
            out.copy_from_slice(&pixels[row]);
        }
    }
}

fn blend(target: &mut [u8; 4], source: &[u8; 4]) {
    if source[3] == 255 {
        *target = *source;
        return;
    }
    if source[3] == 0 {
        return;
    }
    if target[3] == 0 {
        *target = *source;
        return;
    }
    let opacity = u32::from(source[3]);
    let inverse = 255 - opacity;
    if target[3] == 255 {
        for (dst, &src) in target[..3].iter_mut().zip(source) {
            let mixed = u32::from(src) * opacity + u32::from(*dst) * inverse;
            *dst = ((mixed + 127) / 255) as u8;
        }
        return;
    }
    let front = opacity * 255;
    let back = u32::from(target[3]) * inverse;
    let total = front + back;
    for (dst, &src) in target[..3].iter_mut().zip(source) {
        let mixed = u32::from(src) * front + u32::from(*dst) * back;
        *dst = ((mixed + total / 2) / total) as u8;
    }
    target[3] = ((total + 127) / 255) as u8;
}
