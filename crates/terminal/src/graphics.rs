use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::{collections::HashMap, ops::Range};

use lune_kitty_graphics::{
    DecodeError, Decoder, Delete, DeleteTarget, Image, ImageKey, ImageStore, Placement,
    PlacementError, PlacementPosition, RelativePlacement, Reply,
};
use lune_vt::ApcEvent;

use crate::{CellPosition, Cursor, Grid, PixelSize, Screen, ScreenView, StorageError, StyleTable};

mod placeholders;
pub use placeholders::{
    GraphicsPlaceholderCell, graphics_parent_origins, visit_graphics_placeholders,
};

const MAX_PLACEMENTS: usize = 4096;
const _: () = assert!(MAX_PLACEMENTS - 1 <= u16::MAX as usize);
const MAX_PARENT_DEPTH: usize = 16;
const UNCROPPED: [f64; 2] = [f64::NEG_INFINITY, f64::INFINITY];
static NEXT_PLACEMENT_REVISION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct GraphicsPlacement {
    pub placement: Placement,
    pub row: i64,
    pub column: i64,
    // Retained vertical edges in units of the requested source rectangle height.
    vertical_crop: [f64; 2],
    span: [usize; 2],
    anonymous: Option<Arc<Image>>,
    serial: u64,
    parent: Option<u64>,
    root: Option<PlacementRoot>,
}

#[derive(Clone, Copy, Debug)]
struct PlacementRoot {
    serial: u64,
    offset: [i64; 2],
    index: u16,
    is_virtual: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraphicsLayout {
    pub offset: [f64; 2],
    pub size: [f64; 2],
    pub source_origin: [f64; 2],
    pub source_per_pixel: f64,
    pub sample_rows: [f64; 2],
}

impl GraphicsPlacement {
    pub fn serial(&self) -> u64 {
        self.serial
    }

    pub fn is_virtual(&self) -> bool {
        self.placement.position == PlacementPosition::Virtual
    }

    pub fn virtual_parent(&self) -> Option<u64> {
        self.root
            .filter(|root| root.is_virtual)
            .map(|root| root.serial)
    }

    pub fn origin_in_view(
        &self,
        history_offset: usize,
        virtual_origin: impl FnOnce(u64) -> Option<[i64; 2]>,
    ) -> Option<[i64; 2]> {
        if let Some(parent) = self.virtual_parent() {
            let [x, y] = virtual_origin(parent)?;
            return Some([
                x.saturating_add(self.column),
                y.saturating_add(self.row)
                    .saturating_add(history_offset as i64),
            ]);
        }
        Some([self.column, self.row.saturating_add(history_offset as i64)])
    }

    pub fn anonymous_image(&self) -> Option<&Image> {
        self.anonymous.as_deref()
    }

    fn rows(&self) -> Range<i64> {
        self.rows_at(self.row)
    }

    fn rows_at(&self, row: i64) -> Range<i64> {
        let height = i64::try_from(self.span[1]).unwrap_or(i64::MAX);
        row..row.saturating_add(height)
    }

    fn columns_at(&self, column: i64) -> Range<i64> {
        column..column.saturating_add(i64::try_from(self.span[0]).unwrap_or(i64::MAX))
    }

    fn intersects_display(&self, [x, y]: [i64; 2], grid: &Grid) -> bool {
        let rows = self.rows_at(y);
        x < grid.columns() as i64
            && self.columns_at(x).end > 0
            && rows.start < grid.rows() as i64
            && rows.end > 0
    }

    pub fn layout(&self, cell: PixelSize) -> GraphicsLayout {
        let (mut offset, mut size, scale) = self.uncropped_geometry(cell);
        let mut source_origin = self.placement.source_origin.map(f64::from);
        let mut sample_rows = UNCROPPED;
        if self.vertical_crop != UNCROPPED {
            let [top, bottom] = self.vertical_crop;
            let start = top.max(0.0);
            let fraction = bottom.min(1.0) - start;
            if top.is_finite() {
                offset[1] = (start - top) * size[1];
            }
            size[1] *= fraction;
            let source_height = f64::from(self.placement.source_size[1]);
            let first = source_origin[1] + start * source_height;
            if top > 0.0 {
                sample_rows[0] = snap_texel_edge(first).floor() + 0.5;
            }
            if bottom < 1.0 {
                let last = source_origin[1] + bottom * source_height;
                sample_rows[1] = (snap_texel_edge(last).ceil() - 0.5).max(sample_rows[0]);
            }
            source_origin[1] = first;
        }
        GraphicsLayout {
            offset,
            size,
            source_origin,
            source_per_pixel: scale.recip(),
            sample_rows,
        }
    }

    fn uncropped_geometry(&self, cell: PixelSize) -> ([f64; 2], [f64; 2], f64) {
        let cell = [f64::from(cell.width.get()), f64::from(cell.height.get())];
        let [width, height] = self.placement.source_size.map(f64::from);
        let [columns, rows] = self.placement.cells.map(f64::from);
        let [x, y] = self.placement.cell_offset.map(f64::from);
        let [x, y] = [x.min(cell[0] - 1.0), y.min(cell[1] - 1.0)];
        let available_width = columns * cell[0] - x;
        let available_height = rows * cell[1] - y;
        let (scale, left, top) = match (columns > 0.0, rows > 0.0) {
            (true, true) => {
                let scale = (available_width / width).min(available_height / height);
                (
                    scale,
                    (available_width - width * scale) * 0.5,
                    (available_height - height * scale) * 0.5,
                )
            }
            (true, false) => (available_width / width, 0.0, 0.0),
            (false, true) => (available_height / height, 0.0, 0.0),
            (false, false) => (1.0, 0.0, 0.0),
        };
        ([x + left, y + top], [width * scale, height * scale], scale)
    }

    fn set_cell_size(&mut self, cell: PixelSize) {
        let (offset, size, _) = self.uncropped_geometry(cell);
        let cell_size = [cell.width.get(), cell.height.get()].map(f64::from);
        self.span = std::array::from_fn(|axis| {
            let count = self.placement.cells[axis];
            if count == 0 {
                ((offset[axis] + size[axis]) / cell_size[axis]).ceil() as usize
            } else {
                count as usize
            }
        });
        if self.vertical_crop != UNCROPPED {
            let [top, bottom] = self.vertical_crop;
            let mut height = if bottom.is_finite() {
                bottom * size[1] + offset[1]
            } else if self.placement.cells[1] != 0 {
                f64::from(self.placement.cells[1]) * cell_size[1]
            } else {
                offset[1] + size[1]
            };
            if top.is_finite() {
                height -= top * size[1] + offset[1];
            }
            let rows = height / cell_size[1];
            // Allow round-off when projecting a crop edge onto a cell boundary.
            let tolerance = (size[1] / cell_size[1]).max(rows) * (8.0 * f64::EPSILON);
            self.span[1] = (rows - tolerance.min(1e-7)).ceil().max(1.0) as usize;
        }
    }

    fn crop_rows(&mut self, retained: Range<i64>, cell: PixelSize) -> bool {
        if retained.is_empty() {
            return false;
        }
        let (offset, size, _) = self.uncropped_geometry(cell);
        let anchor = if self.vertical_crop[0].is_finite() {
            self.vertical_crop[0]
        } else {
            -offset[1] / size[1]
        };
        let rows_to_source = f64::from(cell.height.get()) / size[1];
        let edge = |row: i64| anchor + row.saturating_sub(self.row) as f64 * rows_to_source;
        if retained.end < self.rows().end {
            self.vertical_crop[1] = self.vertical_crop[1].min(edge(retained.end));
        }
        if retained.start > self.row {
            self.vertical_crop[0] = edge(retained.start);
        }
        self.row = retained.start;
        self.span[1] = retained.end.saturating_sub(retained.start) as usize;
        self.vertical_crop[0].max(0.0) < self.vertical_crop[1].min(1.0)
    }

    pub(crate) fn cursor_travel(&self, measured_cell: bool) -> [usize; 2] {
        if measured_cell {
            self.span
        } else {
            self.placement.cells.map(|count| count as usize)
        }
    }

    pub(crate) fn at_cursor(
        cursor: Cursor,
        mut placement: Placement,
        dimensions: (u32, u32),
        cell: PixelSize,
        anonymous: Option<Arc<Image>>,
    ) -> Result<Self, PlacementError> {
        for (axis, dimension) in [dimensions.0, dimensions.1].into_iter().enumerate() {
            let remaining = dimension.saturating_sub(placement.source_origin[axis]);
            if remaining == 0 {
                return Err(PlacementError::InvalidRegion);
            }
            let requested = placement.source_size[axis];
            placement.source_size[axis] = if requested == 0 {
                remaining
            } else {
                requested.min(remaining)
            };
        }
        let cursor_anchored = match placement.position {
            PlacementPosition::Cursor => true,
            PlacementPosition::Virtual => {
                // Placeholder pixels are drawn below the cursor.
                placement.z = -1;
                false
            }
            PlacementPosition::Relative(_) => false,
        };
        let mut image = Self {
            placement,
            row: if cursor_anchored {
                cursor.row as i64
            } else {
                0
            },
            column: if cursor_anchored {
                cursor.column as i64
            } else {
                0
            },
            vertical_crop: UNCROPPED,
            span: [0; 2],
            anonymous,
            serial: 0,
            parent: None,
            root: None,
        };
        image.set_cell_size(cell);
        Ok(image)
    }
}

fn snap_texel_edge(pixel: f64) -> f64 {
    let rounded = pixel.round();
    if (pixel - rounded).abs() < 1e-9 {
        rounded
    } else {
        pixel
    }
}

#[derive(Debug, Default)]
pub(crate) struct GraphicsPlacements {
    images: Vec<GraphicsPlacement>,
    revision: u64,
    next_serial: u64,
    virtual_count: usize,
    virtual_parents: bool,
    anchors: HashMap<u64, usize>,
    orphaned: Vec<u32>,
}

pub(crate) struct GraphicsAnchor {
    pub(crate) source: CellPosition,
    pub(crate) destination: Option<CellPosition>,
    index: usize,
}

impl GraphicsPlacements {
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    pub(crate) fn as_slice(&self) -> &[GraphicsPlacement] {
        &self.images
    }

    pub(crate) fn has_virtual(&self) -> bool {
        self.virtual_count != 0
    }

    pub(crate) fn has_virtual_parents(&self) -> bool {
        self.virtual_parents
    }

    pub(crate) fn revision(&self) -> Option<u64> {
        (!self.images.is_empty()).then_some(self.revision)
    }

    fn changed(&mut self) {
        self.revision = NEXT_PLACEMENT_REVISION.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn clear(&mut self) {
        self.clear_buffer();
        self.orphaned.clear();
    }

    pub(crate) fn set_cell_size(&mut self, cell: PixelSize) {
        for image in &mut self.images {
            image.set_cell_size(cell);
        }
    }

    pub(crate) fn reflow_anchors(
        &self,
        view: ScreenView<'_>,
        columns: usize,
    ) -> Result<Vec<GraphicsAnchor>, StorageError> {
        let history = view.history_rows() as i64;
        let mut anchors = Vec::new();
        for (index, image) in self.images.iter().enumerate() {
            if image.placement.position != PlacementPosition::Cursor {
                continue;
            }
            let row = image.row.max(-history);
            let source = crate::reflow::normalize_position(
                view,
                CellPosition {
                    row: (row + history) as usize,
                    column: image.column.clamp(0, columns as i64 - 1) as usize,
                },
            )?;
            anchors.try_reserve(1).map_err(StorageError::from)?;
            anchors.push(GraphicsAnchor {
                source,
                destination: None,
                index,
            });
        }
        anchors.sort_unstable_by_key(|anchor| anchor.source);
        Ok(anchors)
    }

    pub(crate) fn commit_reflow(
        &mut self,
        mut anchors: Vec<GraphicsAnchor>,
        window: Range<usize>,
        history: usize,
        columns: usize,
        cell: PixelSize,
    ) {
        if anchors.is_empty() {
            return;
        }
        anchors.sort_unstable_by_key(|anchor| anchor.index);
        let mut anchors = anchors.into_iter();
        let count = self.images.len();
        self.images.retain_mut(|image| {
            if image.placement.position != PlacementPosition::Cursor {
                return true;
            }
            let anchor = anchors.next().expect("A physical root has a reflow anchor");
            let Some(destination) = anchor.destination.filter(|point| point.row < window.end)
            else {
                return false;
            };
            image.row = destination.row as i64 - window.start as i64;
            image.column = destination.column.min(columns - 1) as i64;
            let first = -(history as i64);
            image.row >= first || image.crop_rows(first..image.rows().end, cell)
        });
        self.sync_graph(self.images.len() != count);
        if !self.images.is_empty() {
            self.changed();
        }
    }

    pub(crate) fn crop_alternate(&mut self, columns: usize, rows: usize) {
        self.remove_matching(|image| {
            image.placement.position == PlacementPosition::Cursor
                && (image.column >= columns as i64 || image.row >= rows as i64)
        });
    }

    pub(crate) fn clear_buffer(&mut self) {
        self.images.clear();
        self.anchors.clear();
        self.virtual_count = 0;
        self.virtual_parents = false;
    }

    pub(crate) fn clear_live(&mut self, view: ScreenView<'_>, styles: &StyleTable) {
        let origins = if self.virtual_parents {
            graphics_parent_origins(&self.images, view, styles).unwrap_or_default()
        } else {
            HashMap::new()
        };
        self.remove_matching(|image| {
            if image.is_virtual() {
                return false;
            }
            if image.virtual_parent().is_some() {
                return image
                    .origin_in_view(0, |parent| origins.get(&parent).copied().flatten())
                    .is_some_and(|origin| image.intersects_display(origin, view.live_grid()));
            }
            image.rows().end > 0
        });
    }

    pub(crate) fn discard_history_before(&mut self, first_row: i64, cell: PixelSize) {
        let count = self.images.len();
        let mut changed = false;
        self.images.retain_mut(|image| {
            if image.is_virtual() || image.parent.is_some() || image.row >= first_row {
                return true;
            }
            changed = true;
            image.crop_rows(first_row..image.rows().end, cell)
        });
        if changed {
            self.sync_graph(self.images.len() != count);
            if !self.is_empty() {
                self.changed();
            }
        }
    }

    pub(crate) fn scroll(
        &mut self,
        source: Range<i64>,
        destination: Range<i64>,
        delta: i64,
        cell: PixelSize,
    ) {
        self.scroll_matching(destination, delta, cell, |rows| {
            rows.start >= source.start && rows.end <= source.end
        });
    }

    pub(crate) fn reverse_page(&mut self, first_retained_row: i64, delta: i64, cell: PixelSize) {
        // The live portion carries the whole placement. Placements entirely in
        // scrollback keep their position.
        self.scroll_matching(first_retained_row..i64::MAX, delta, cell, |rows| {
            rows.end > 0
        });
    }

    #[cold]
    fn scroll_matching(
        &mut self,
        destination: Range<i64>,
        delta: i64,
        cell: PixelSize,
        mut moves: impl FnMut(&Range<i64>) -> bool,
    ) {
        let count = self.images.len();
        let mut changed = false;
        self.images.retain_mut(|image| {
            if image.is_virtual() || image.parent.is_some() {
                return true;
            }
            let rows = image.rows();
            if !moves(&rows) {
                return true;
            }
            changed = true;
            let moved = rows.start.saturating_add(delta)..rows.end.saturating_add(delta);
            image.row = moved.start;
            if moved.start >= destination.start && moved.end <= destination.end {
                return true;
            }
            let retained = moved.start.max(destination.start)..moved.end.min(destination.end);
            image.crop_rows(retained, cell)
        });
        if changed {
            self.sync_graph(self.images.len() != count);
            if !self.is_empty() {
                self.changed();
            }
        }
    }

    pub(crate) fn remove_images(&mut self, replaced: Option<ImageKey>, evicted: &[ImageKey]) {
        self.remove_matching(|image| {
            let key = image.placement.image;
            Some(key) == replaced || evicted.binary_search(&key).is_ok()
        });
    }

    pub(crate) fn for_each_ancestor(
        &self,
        relative: RelativePlacement,
        mut mark: impl FnMut(ImageKey) -> Result<(), PlacementError>,
    ) -> Result<(), PlacementError> {
        let mut parent = self.parent(relative).ok_or(PlacementError::MissingParent)?;
        loop {
            mark(parent.placement.image)?;
            let Some(serial) = parent.parent else {
                return Ok(());
            };
            parent = &self.images[self.anchors[&serial]];
        }
    }

    fn parent(&self, parent: RelativePlacement) -> Option<&GraphicsPlacement> {
        self.images
            .iter()
            .filter(|item| {
                item.placement.image.id() == parent.image_id
                    && (parent.placement_id == 0 || item.placement.id == parent.placement_id)
            })
            .min_by_key(|item| item.serial)
    }

    pub(crate) fn delete(
        &mut self,
        target: DeleteTarget,
        cursor: Cursor,
        view: ScreenView<'_>,
        styles: &StyleTable,
        mut removed: impl FnMut(u32),
    ) {
        let coordinates = matches!(
            target,
            DeleteTarget::Visible
                | DeleteTarget::Cursor
                | DeleteTarget::Point { .. }
                | DeleteTarget::Column(_)
                | DeleteTarget::Row(_)
        );
        let origins = if coordinates && self.virtual_parents {
            let Ok(origins) = graphics_parent_origins(&self.images, view, styles) else {
                return;
            };
            origins
        } else {
            HashMap::new()
        };
        self.remove_matching(|image| {
            let placement = image.placement;
            let by_image = matches!(
                target,
                DeleteTarget::Image { .. } | DeleteTarget::IdRange { .. }
            );
            if !by_image && image.is_virtual() {
                return false;
            }
            let origin =
                || image.origin_in_view(0, |parent| origins.get(&parent).copied().flatten());
            let at_cell = |column, row| {
                origin().is_some_and(|[x, y]| {
                    image.columns_at(x).contains(&column) && image.rows_at(y).contains(&row)
                })
            };
            let matches = match target {
                DeleteTarget::Visible => origin()
                    .is_some_and(|origin| image.intersects_display(origin, view.live_grid())),
                DeleteTarget::Image { id, placement_id } => {
                    placement.image.id() == id
                        && (placement_id == 0 || placement.id == placement_id)
                }
                DeleteTarget::Number { .. } | DeleteTarget::Frame { .. } => false,
                DeleteTarget::IdRange { .. } => target.selects_image(placement.image.id()),
                DeleteTarget::Cursor => at_cell(cursor.column as i64, cursor.row as i64),
                DeleteTarget::Point { column, row, z } => {
                    z.is_none_or(|z| placement.z == z) && at_cell(i64::from(column), i64::from(row))
                }
                DeleteTarget::Column(column) => {
                    origin().is_some_and(|[x, _]| image.columns_at(x).contains(&i64::from(column)))
                }
                DeleteTarget::Row(row) => {
                    origin().is_some_and(|[_, y]| image.rows_at(y).contains(&i64::from(row)))
                }
                DeleteTarget::Layer(z) => placement.z == z,
            };
            if matches {
                removed(placement.image.id());
            }
            matches
        });
    }

    fn remove_matching(&mut self, mut matches: impl FnMut(&GraphicsPlacement) -> bool) {
        let count = self.images.len();
        let virtual_count = &mut self.virtual_count;
        self.images.retain(|image| {
            let removed = matches(image);
            if removed && image.is_virtual() {
                *virtual_count -= 1;
            }
            !removed
        });
        if self.images.len() != count {
            self.sync_graph(true);
            if !self.images.is_empty() {
                self.changed();
            }
        }
    }

    pub(crate) fn insert(
        &mut self,
        mut image: GraphicsPlacement,
        replaces_image: bool,
        evicted: &[ImageKey],
    ) -> Result<(), PlacementError> {
        let spec = image.placement;
        let remove = |existing: &GraphicsPlacement| {
            existing.placement.image == spec.image
                && (replaces_image || (spec.id != 0 && existing.placement.id == spec.id))
        };
        let removes_existing =
            (replaces_image || image.placement.id != 0) && self.images.iter().any(remove);
        let evicts =
            |existing: &GraphicsPlacement| evicted.binary_search(&existing.placement.image).is_ok();
        if !removes_existing && (evicted.is_empty() || !self.images.iter().any(evicts)) {
            if self.images.len() >= MAX_PLACEMENTS {
                return Err(PlacementError::Quota);
            }
            self.images
                .try_reserve(1)
                .map_err(|_| PlacementError::Quota)?;
        }
        let retained_serial = (!replaces_image && removes_existing)
            .then(|| {
                self.images
                    .iter()
                    .find(|entry| remove(entry))
                    .map(|entry| entry.serial)
            })
            .flatten();
        image.serial = retained_serial.unwrap_or(self.next_serial + 1);
        if let PlacementPosition::Relative(parent) = spec.position {
            let referenced = self.parent(parent).ok_or(PlacementError::MissingParent)?;
            image.parent = Some(referenced.serial);
        }
        if !self.anchors.is_empty() || image.parent.is_some() {
            self.anchors
                .try_reserve(self.images.len() + 1 - self.anchors.len())
                .map_err(|_| PlacementError::Quota)?;
            self.orphaned
                .try_reserve(MAX_PLACEMENTS - self.orphaned.len())
                .map_err(|_| PlacementError::Quota)?;
            if self.anchors.is_empty() {
                self.reindex();
            }
            let validation = self.validate(&image, replaces_image, retained_serial.is_some());
            if validation.is_err() && !self.images.iter().any(|entry| entry.parent.is_some()) {
                self.anchors.clear();
            }
            validation?;
        }
        if removes_existing || !evicted.is_empty() {
            let virtual_count = &mut self.virtual_count;
            self.images.retain(|existing| {
                let removed = remove(existing) || evicts(existing);
                if removed && existing.is_virtual() {
                    *virtual_count -= 1;
                }
                !removed
            });
        }
        self.next_serial = self.next_serial.max(image.serial);
        let graph = !self.anchors.is_empty() || image.parent.is_some();
        let sort_key = |entry: &GraphicsPlacement| (entry.placement.z, entry.placement.image.id());
        let position = self
            .images
            .partition_point(|entry| sort_key(entry) <= sort_key(&image));
        self.virtual_count += usize::from(image.is_virtual());
        self.images.insert(position, image);
        if graph {
            if removes_existing || !evicted.is_empty() {
                self.sync_graph(true);
            } else {
                self.index_inserted(position);
            }
        }
        self.changed();
        Ok(())
    }

    fn reindex(&mut self) {
        self.anchors.clear();
        self.anchors.extend(
            self.images
                .iter()
                .enumerate()
                .map(|(index, image)| (image.serial, index)),
        );
    }

    fn refresh_root_indices(&mut self) {
        for image in &mut self.images {
            if let Some(root) = &mut image.root {
                root.index = self.anchors[&root.serial] as u16;
            }
        }
    }

    fn index_inserted(&mut self, index: usize) {
        if index + 1 == self.images.len() {
            self.anchors.insert(self.images[index].serial, index);
        } else {
            self.reindex();
            for image in &mut self.images {
                if let Some(root) = &mut image.root
                    && usize::from(root.index) >= index
                {
                    root.index += 1;
                }
            }
        }
        if self.images[index].parent.is_some() {
            self.resolve_root(index)
                .expect("A newly added relative placement was validated");
        }
    }

    fn validate(
        &self,
        image: &GraphicsPlacement,
        replaces_image: bool,
        preserves_serial: bool,
    ) -> Result<(), PlacementError> {
        self.resolve(image, Some(image), replaces_image)?;
        if preserves_serial {
            for entry in &self.images {
                if entry.parent.is_some() && entry.serial != image.serial {
                    self.resolve(entry, Some(image), false)?;
                }
            }
        }
        Ok(())
    }

    fn resolve<'a>(
        &'a self,
        mut image: &'a GraphicsPlacement,
        replacement: Option<&'a GraphicsPlacement>,
        replaces_image: bool,
    ) -> Result<(&'a GraphicsPlacement, [i64; 2], Option<u16>), PlacementError> {
        let mut offset = [0i64; 2];
        let mut visited = [0; MAX_PARENT_DEPTH + 2];
        let mut depth = 0;
        let mut anchor_index = None;
        while let Some(parent) = image.parent {
            visited[depth] = image.serial;
            depth += 1;
            if visited[..depth].contains(&parent) {
                return Err(PlacementError::Cycle);
            }
            if depth > MAX_PARENT_DEPTH {
                return Err(PlacementError::TooDeep);
            }
            let PlacementPosition::Relative(link) = image.placement.position else {
                return Err(PlacementError::MissingParent);
            };
            offset[0] += i64::from(link.offset[0]);
            offset[1] += i64::from(link.offset[1]);
            image = match replacement.filter(|image| image.serial == parent) {
                Some(proposed) => {
                    anchor_index = None;
                    proposed
                }
                None => {
                    let index = *self
                        .anchors
                        .get(&parent)
                        .ok_or(PlacementError::MissingParent)?;
                    let referenced = &self.images[index];
                    if replaces_image
                        && replacement.is_some_and(|proposed| {
                            proposed.placement.image == referenced.placement.image
                        })
                    {
                        return Err(PlacementError::MissingParent);
                    }
                    anchor_index = Some(index as u16);
                    referenced
                }
            };
        }
        Ok((image, offset, anchor_index))
    }

    fn resolve_root(&mut self, index: usize) -> Result<(), PlacementError> {
        let (anchor, offset, anchor_index) = self.resolve(&self.images[index], None, false)?;
        let column = anchor.column + offset[0];
        let row = anchor.row + offset[1];
        let root = PlacementRoot {
            serial: anchor.serial,
            offset,
            index: anchor_index.expect("Relative placement has an existing anchor"),
            is_virtual: anchor.is_virtual(),
        };
        self.virtual_parents |= root.is_virtual;
        let image = &mut self.images[index];
        image.column = column;
        image.row = row;
        image.root = Some(root);
        Ok(())
    }

    #[cold]
    fn sync_graph(&mut self, membership_changed: bool) {
        if self.anchors.is_empty() {
            return;
        }
        if !membership_changed {
            for index in 0..self.images.len() {
                let Some(root) = self.images[index].root else {
                    continue;
                };
                let anchor = &self.images[usize::from(root.index)];
                let column = anchor.column + root.offset[0];
                let row = anchor.row + root.offset[1];
                let image = &mut self.images[index];
                image.column = column;
                image.row = row;
            }
            return;
        }
        self.reindex();
        self.virtual_parents = false;
        let mut invalid = false;
        let mut relative = false;
        for index in 0..self.images.len() {
            if self.images[index].parent.is_none() {
                continue;
            }
            if self.resolve_root(index).is_ok() {
                relative = true;
            } else {
                invalid = true;
                self.images[index].parent = Some(0);
            }
        }
        if invalid {
            let orphaned = &mut self.orphaned;
            self.images.retain(|image| {
                if image.parent != Some(0) {
                    return true;
                }
                let id = image.placement.image.id();
                if id != 0 {
                    orphaned.push(id);
                }
                false
            });
            if relative {
                self.reindex();
                self.refresh_root_indices();
            }
        }
        if !relative {
            self.anchors.clear();
        }
    }

    pub(crate) fn orphaned(&self) -> &[u32] {
        &self.orphaned
    }

    pub(crate) fn clear_orphaned(&mut self) {
        self.orphaned.clear();
    }
}

pub(crate) struct Graphics {
    decoder: Decoder,
    images: ImageStore,
    reply: Option<Reply>,
    active: bool,
    has_relative: bool,
}

impl Default for Graphics {
    fn default() -> Self {
        Self {
            decoder: Decoder::default(),
            images: ImageStore::new(64 * 1024 * 1024, 1024),
            reply: None,
            active: false,
            has_relative: false,
        }
    }
}

impl Graphics {
    pub(crate) fn get(&self, id: u32) -> Option<&Image> {
        self.images.get(id)
    }

    pub(crate) fn image_source_revision(&self, key: ImageKey) -> Option<u64> {
        self.images.source_revision(key)
    }

    pub(crate) fn visual_revision(&self) -> u64 {
        self.images.visual_revision()
    }

    pub(crate) fn storage_revision(&self) -> u64 {
        self.images.storage_revision()
    }

    pub(crate) fn animation_deadline(&self) -> Option<std::time::Instant> {
        self.images.animation_deadline()
    }

    pub(crate) fn poll_animations(&mut self, clock: impl FnOnce() -> std::time::Instant) {
        self.images.poll_animations(clock);
    }

    pub(crate) fn reset(&mut self) {
        self.decoder.cancel();
        self.images.clear();
        self.reply = None;
        self.active = false;
        self.has_relative = false;
    }

    pub(crate) fn finish(&mut self) {
        self.decoder.cancel();
        self.images.abort_transfer();
        self.active = false;
    }

    pub(crate) fn needs_data(&self) -> bool {
        self.active
    }

    pub(crate) fn data(&mut self, bytes: &[u8]) {
        self.decoder.push(bytes);
    }

    pub(crate) fn take_reply(&mut self) -> Option<Reply> {
        self.reply.take()
    }

    #[cold]
    pub(crate) fn event(
        &mut self,
        event: ApcEvent<'_>,
        screen: &mut Screen,
        styles: &StyleTable,
    ) -> bool {
        match event {
            ApcEvent::Start => {
                self.decoder.start();
                self.active = true;
            }
            ApcEvent::Data(bytes) => self.data(bytes),
            ApcEvent::End => {
                self.active = false;
                self.reap_orphans(screen);
                match self.decoder.finish() {
                    Some(Ok(command)) => {
                        if let Some(delete) = command.decode_delete() {
                            self.images.abort_transfer();
                            if let Ok(delete) = delete {
                                delete_images(delete, screen, &mut self.images, styles);
                                return false;
                            }
                        }
                        debug_assert!(self.reply.is_none());
                        self.reply = self.images.receive(command, |change| {
                            let relative = matches!(&change, lune_kitty_graphics::ImageChange::Place { placement, .. }
                                if matches!(placement.position, PlacementPosition::Relative(_)));
                            screen.apply_image(change)?;
                            self.has_relative |= relative;
                            Ok(())
                        });
                        return self.reply.is_some();
                    }
                    Some(Err(DecodeError::PayloadTooLong | DecodeError::PayloadAllocation)) => {
                        self.images.abort_transfer();
                    }
                    Some(Err(_)) | None => {}
                }
            }
            ApcEvent::Cancel => {
                self.active = false;
                if self.decoder.cancel() {
                    self.images.abort_transfer();
                }
            }
        }
        false
    }

    #[inline]
    pub(crate) fn reap_orphans(&mut self, screen: &mut Screen) {
        if self.has_relative && screen.has_graphics_orphans() {
            self.reap_orphans_inner(screen);
        }
    }

    #[cold]
    fn reap_orphans_inner(&mut self, screen: &mut Screen) {
        self.images.retain_ids(|id| {
            !screen.graphics_orphaned(id)
                || screen
                    .all_graphics_placements()
                    .any(|placement| placement.placement.image.id() == id)
        });
        screen.clear_graphics_orphans();
    }
}

fn delete_images(
    mut delete: Delete,
    screen: &mut Screen,
    images: &mut ImageStore,
    styles: &StyleTable,
) {
    if matches!(delete.target, DeleteTarget::Frame { .. }) {
        images.delete_frame(delete, |change| screen.apply_image(change));
        return;
    }
    if let DeleteTarget::Number {
        number,
        placement_id,
    } = delete.target
    {
        let Some(id) = images.newest_id(number) else {
            return;
        };
        delete.target = DeleteTarget::Image { id, placement_id };
    }
    if !delete.free_data {
        screen.delete_placements(delete.target, styles, |_| {});
        return;
    }
    let mut removed = Vec::new();
    let mut referenced = Vec::new();
    // Reserve both lists before changing placements or image storage.
    if removed
        .try_reserve_exact(screen.graphics_placements().len())
        .is_err()
        || referenced
            .try_reserve_exact(screen.all_graphics_placements().count())
            .is_err()
    {
        return;
    }
    screen.delete_placements(delete.target, styles, |id| removed.push(id));
    referenced.extend(
        screen
            .all_graphics_placements()
            .map(|image| image.placement.image.id()),
    );
    removed.sort_unstable();
    referenced.sort_unstable();
    images.retain_ids(|id| {
        referenced.binary_search(&id).is_ok()
            || !(delete.target.selects_image(id) || removed.binary_search(&id).is_ok())
    });
}

#[cfg(test)]
mod quota_tests {
    use super::*;
    use crate::ScreenBuffer;

    #[test]
    fn quota_eviction_ranks_both_buffers_and_protects_relative_ancestry() {
        let mut graphics = Graphics {
            images: ImageStore::new(12, 3),
            ..Graphics::default()
        };
        let mut screen = Screen::new(8, 4).unwrap();
        let styles = StyleTable::new(1).unwrap();
        let receive = |graphics: &mut Graphics, screen: &mut Screen, command: &[u8]| {
            graphics.event(ApcEvent::Start, screen, &styles);
            graphics.event(ApcEvent::Data(command), screen, &styles);
            graphics.event(ApcEvent::End, screen, &styles);
            graphics.take_reply().unwrap().message
        };

        assert_eq!(
            receive(
                &mut graphics,
                &mut screen,
                b"Ga=T,i=1,p=1,s=1,v=1,C=1;/wAA/w=="
            ),
            b"OK",
        );
        screen.select_buffer(ScreenBuffer::Alternate);
        assert_eq!(
            receive(&mut graphics, &mut screen, b"Gi=2,s=1,v=1;AAAA/w=="),
            b"OK"
        );
        assert_eq!(
            receive(&mut graphics, &mut screen, b"Gi=3,s=1,v=1,N=1;AAAA/w=="),
            b"OK"
        );
        assert_eq!(
            receive(&mut graphics, &mut screen, b"Ga=T,i=4,x=1,s=1,v=1;AAAA/w=="),
            b"EINVAL:Invalid placement region",
        );
        assert!((1..=3).all(|id| graphics.get(id).is_some()));
        assert_eq!(
            receive(&mut graphics, &mut screen, b"Gi=4,s=1,v=1;AAAA/w=="),
            b"OK"
        );
        assert!(graphics.get(3).is_none());
        assert_eq!(
            receive(&mut graphics, &mut screen, b"Gi=5,s=1,v=1;AAAA/w=="),
            b"OK"
        );
        assert!(graphics.get(2).is_none());

        screen.select_buffer(ScreenBuffer::Primary);
        assert_eq!(screen.graphics_placements()[0].placement.image.id(), 1);
        assert_eq!(
            receive(&mut graphics, &mut screen, b"Ga=p,i=4,p=1,P=1,Q=1"),
            b"OK"
        );
        assert_eq!(
            receive(&mut graphics, &mut screen, b"Ga=p,i=5,p=1,C=1"),
            b"OK"
        );
        assert_eq!(
            receive(
                &mut graphics,
                &mut screen,
                b"Ga=T,i=6,P=4,Q=1,s=1,v=1;AAAA/w=="
            ),
            b"OK",
        );
        assert!(graphics.get(5).is_none());
        assert_eq!(
            screen
                .graphics_placements()
                .iter()
                .map(|placement| placement.placement.image.id())
                .collect::<Vec<_>>(),
            [1, 4, 6],
        );
        assert_eq!(
            receive(&mut graphics, &mut screen, b"Ga=T,i=7,P=6,s=1,v=1;AAAA/w=="),
            b"ENOSPC:Image quota exceeded",
        );
        assert!([1, 4, 6].into_iter().all(|id| graphics.get(id).is_some()));
        assert_eq!(
            receive(&mut graphics, &mut screen, b"Gi=7,s=1,v=1;AAAA/w=="),
            b"OK"
        );
        assert!(screen.graphics_placements().is_empty());
        graphics.reap_orphans(&mut screen);
        assert!([1, 4, 6].into_iter().all(|id| graphics.get(id).is_none()));
        assert!(graphics.get(7).is_some());

        for command in [
            b"Gi=8,s=1,v=1;AAAA/w==".as_slice(),
            b"Gi=9,s=1,v=1;AAAA/w==",
            b"Gi=7,s=3,v=1;AAAAAAAAAAAAAAAA",
        ] {
            assert_eq!(receive(&mut graphics, &mut screen, command), b"OK");
        }
        assert_eq!(graphics.get(7).unwrap().dimensions(), (3, 1));
        assert!(graphics.get(8).is_none());
        assert!(graphics.get(9).is_none());
    }
}
