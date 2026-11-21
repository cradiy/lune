use std::ops::Range;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Instant;

use crate::{
    Controls, PixelUpdate, PixelUpdates, Value, control_keys,
    pixels::{INVALID, ImageFormat, Pixels, QUOTA},
};

mod playback;
use playback::{Playback, PlaybackMode};

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
    ) -> Result<Arc<Surface>, &'static [u8]> {
        // Include the surface allocation and allowance for the frame list's spare capacity.
        let overhead = size_of::<Surface>() + 2 * (size_of::<Frame>() + size_of::<usize>());
        let bytes = data.capacity().checked_add(overhead).ok_or(QUOTA)?;
        self.used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|&total| total <= self.limit)
            })
            .map_err(|_| QUOTA)?;
        Ok(Arc::new(Surface {
            revision,
            data,
            budget: Arc::clone(self),
            bytes,
        }))
    }
}

#[derive(Debug, Default)]
struct Frame {
    surface: Option<Arc<Surface>>,
    gap_ms: u32,
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
    current: usize,
    original_users: usize,
    total_gap_ms: u64,
    playback: Playback,
    pub(crate) scheduled_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRevision {
    revision: u64,
    patch: Option<(u64, Region)>,
}

impl PixelRevision {
    pub fn id(&self) -> u64 {
        self.revision
    }

    pub(crate) fn full(revision: u64) -> Self {
        Self {
            revision,
            patch: None,
        }
    }
}

impl Animation {
    pub(crate) fn uses_original(&self) -> bool {
        self.root.surface.is_none() || self.original_users != 0
    }

    pub(crate) fn revision(&self) -> Option<u64> {
        self.current_surface().map(|surface| surface.revision)
    }

    pub(crate) fn pixel_revision(&self, original_revision: u64) -> PixelRevision {
        let Some(surface) = self.current_surface() else {
            return PixelRevision::full(original_revision);
        };
        let mut revision = PixelRevision::full(surface.revision);
        if let SurfaceData::Patch(patch) = &surface.data {
            revision.patch = patch
                .base
                .revision(original_revision)
                .map(|base| (base, patch.region));
        }
        revision
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
            let mut region = None;
            if let Some(data @ SurfaceData::Patch(patch)) = data
                && let Some(base) = patch.base.revision(original_revision)
            {
                if base == previous.revision {
                    if !patch.blend {
                        return Ok(PixelUpdates::One(
                            patch.region.update(&patch.pixels, patch.stride),
                        ));
                    }
                    region = Some(patch.region);
                } else if let Some((previous_base, previous_region)) = previous.patch
                    && base == previous_base
                {
                    let union = patch.region.union(previous_region);
                    let separate_area = patch.region.area() + previous_region.area();
                    if !patch.region.is_empty()
                        && !previous_region.is_empty()
                        && union.area().saturating_sub(separate_area) >= MIN_SPLIT_SAVINGS_PIXELS
                    {
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
            if let Some(region) = region {
                let width = dimensions.0 as usize;
                if region.is_empty() {
                    return Ok(PixelUpdates::One(region.update(&[], 0)));
                }
                return match data {
                    None => Ok(PixelUpdates::One(region.update_from(original, width))),
                    Some(SurfaceData::Full(bytes)) => {
                        Ok(PixelUpdates::One(region.update_from(bytes, width)))
                    }
                    Some(data) => {
                        let stride = region.width * 4;
                        resize_scratch(scratch, stride * region.height)?;
                        data.render(original, width, region, scratch);
                        Ok(PixelUpdates::One(region.update(scratch, stride)))
                    }
                };
            }
        }
        let bytes = match data {
            Some(data) => data.pixels(original, dimensions, scratch)?,
            None => original,
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
            original,
            budget,
            revision,
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
        let base = if let Some(frame) = self.frame(index) {
            Base::from_frame(frame)
        } else if upload.base == 0 {
            Base::Color(upload.background)
        } else {
            Base::from_frame(
                self.frame(upload.base as usize - 1)
                    .ok_or(b"EINVAL:Animation base frame not found".as_slice())?,
            )
        };
        let (x, y) = upload.origin;
        let (patch_width, patch_height) = pixels.dimensions;
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
            let region = Region {
                x: x as usize,
                y: y as usize,
                width: patch_width.min(width.saturating_sub(x)) as usize,
                height: patch_height.min(height.saturating_sub(y)) as usize,
            };
            let base = if upload.overwrite {
                base.without_covered_patches(region)
            } else {
                base
            };
            let depth = base.depth() + 1;
            let uses_original = base.uses_original();
            let patch = SurfaceData::Patch(Patch {
                base,
                region,
                stride: patch_width as usize * 4,
                pixels: pixels.bytes,
                blend: !upload.overwrite,
                depth,
                uses_original,
            });
            if depth > MAX_PATCH_DEPTH {
                let mut scratch = Vec::new();
                patch.pixels(original, dimensions, &mut scratch)?;
                SurfaceData::Full(scratch)
            } else {
                patch
            }
        };
        let surface = budget.store(data, revision)?;
        let original_user = usize::from(surface.data.uses_original());
        if let Some(frame) = self.frame_mut(index) {
            let previous_user = usize::from(frame.uses_original_surface());
            frame.surface = Some(surface);
            self.original_users -= previous_user;
            if let Some(gap_ms) = upload.gap_ms {
                self.set_gap(index, gap_ms);
            }
            if index == self.current {
                self.playback.restart_frame();
            }
        } else {
            self.extra.try_reserve(1).map_err(|_| QUOTA)?;
            let gap_ms = upload.gap_ms.unwrap_or(DEFAULT_GAP_MS);
            self.extra.push(Frame {
                surface: Some(surface),
                gap_ms,
            });
            self.total_gap_ms += u64::from(gap_ms);
        }
        self.original_users += original_user;
        Ok(())
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
        self.original_users -= usize::from(removed.uses_original_surface());
        self.total_gap_ms -= u64::from(removed.gap_ms);
        if self.extra.capacity() > self.extra.len().saturating_mul(4).max(8) {
            self.extra.shrink_to_fit();
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
    budget: Arc<FrameBudget>,
    bytes: usize,
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Surface")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

enum SurfaceData {
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
        matches!(self, Self::Patch(patch) if patch.uses_original)
    }

    fn capacity(&self) -> usize {
        match self {
            Self::Full(pixels) => pixels.capacity(),
            Self::Patch(patch) => patch.pixels.capacity(),
        }
    }

    fn pixels<'a>(
        &'a self,
        original: &[u8],
        dimensions: (u32, u32),
        scratch: &'a mut Vec<u8>,
    ) -> Result<&'a [u8], &'static [u8]> {
        match self {
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
                SurfaceData::Full(_) => 0,
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
    pixels: Vec<u8>,
    blend: bool,
    depth: u8,
    uses_original: bool,
}

impl Patch {
    fn apply(&self, target: Region, output: &mut [u8]) {
        let Some(overlap) = self.region.intersection(target) else {
            return;
        };
        let rows = overlap.relative_to(target).rows(target.width * 4);
        let overlay_rows = overlap.relative_to(self.region).rows(self.stride);
        for (target, source) in rows.zip(overlay_rows) {
            let target = &mut output[target];
            let source = &self.pixels[source];
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Region {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl Region {
    fn area(self) -> usize {
        self.width * self.height
    }

    fn is_empty(self) -> bool {
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

    fn update_from(self, pixels: &[u8], width: usize) -> PixelUpdate<'_> {
        let stride = width * 4;
        let start = self.y * stride + self.x * 4;
        let len = (self.height - 1) * stride + self.width * 4;
        self.update(&pixels[start..start + len], stride)
    }

    fn checked(
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
    if source[3] == 0 {
        return;
    }
    if source[3] == 255 || target[3] == 0 {
        *target = *source;
        return;
    }
    let front = u32::from(source[3]) * 255;
    let back = u32::from(target[3]) * (255 - u32::from(source[3]));
    let total = front + back;
    for (dst, &src) in target[..3].iter_mut().zip(source) {
        let mixed = u32::from(src) * front + u32::from(*dst) * back;
        *dst = ((mixed + total / 2) / total) as u8;
    }
    target[3] = ((total + 127) / 255) as u8;
}
