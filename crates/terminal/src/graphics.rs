use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::{collections::HashMap, ops::Range};

use lune_kitty_graphics::{
    Decoder, Delete, DeleteTarget, Image, ImageKey, ImageStore, Placement, PlacementError,
    PlacementPosition, RelativePlacement, Reply,
};
use lune_vt::ApcEvent;

use crate::{
    CellPosition, Cursor, Grid, PixelSize, Screen, ScreenBuffer, ScreenView, StorageError,
    StyleTable,
};

mod placeholders;
mod visibility;
pub use placeholders::{
    GraphicsPlaceholderCache, GraphicsPlaceholderRun, visit_graphics_placeholders,
};
use placeholders::{Origins, VirtualParentOrigins};
use visibility::AnimationVisibility;

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
    // Virtual spans count image-covered cells from virtual_start.
    span: [usize; 2],
    virtual_start: [u32; 2],
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
    /// Inclusive texel centers at the top-left and bottom-right of the source region.
    pub sample_bounds: [[f64; 2]; 2],
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

    fn draw_order(&self) -> (i32, u32) {
        (self.placement.z, self.placement.image.id())
    }

    fn shift_relative(&mut self, delta: [i64; 2]) {
        self.column += delta[0];
        self.row += delta[1];
        let root = self.root.as_mut().expect("A relative placement has a root");
        root.offset[0] += delta[0];
        root.offset[1] += delta[1];
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
        self.intersects_viewport([x, y], [grid.columns(), grid.rows()])
    }

    /// Checks the placement's cell span at its resolved viewport origin.
    pub fn intersects_viewport(&self, [x, y]: [i64; 2], [columns, rows]: [usize; 2]) -> bool {
        x < columns as i64
            && y < rows as i64
            && self.columns_at(x).end > 0
            && self.rows_at(y).end > 0
    }

    pub fn layout(&self, cell: PixelSize) -> GraphicsLayout {
        let (mut offset, mut size, scale) = self.uncropped_geometry(cell);
        let mut source_origin = self.placement.source_origin.map(f64::from);
        let source_size = self.placement.source_size.map(f64::from);
        let mut sample_bounds = [
            source_origin.map(|start| start + 0.5),
            std::array::from_fn(|axis| source_origin[axis] + source_size[axis] - 0.5),
        ];
        if self.vertical_crop != UNCROPPED {
            let [top, bottom] = self.vertical_crop;
            let start = top.max(0.0);
            let fraction = bottom.min(1.0) - start;
            if top.is_finite() {
                offset[1] = (start - top) * size[1];
            }
            size[1] *= fraction;
            let first = source_origin[1] + start * source_size[1];
            if top > 0.0 {
                sample_bounds[0][1] = snap_integer_edge(first).floor() + 0.5;
            }
            if bottom < 1.0 {
                let last = source_origin[1] + bottom * source_size[1];
                sample_bounds[1][1] =
                    (snap_integer_edge(last).ceil() - 0.5).max(sample_bounds[0][1]);
            }
            source_origin[1] = first;
        }
        GraphicsLayout {
            offset,
            size,
            source_origin,
            source_per_pixel: scale.recip(),
            sample_bounds,
        }
    }

    fn uncropped_geometry(&self, cell: PixelSize) -> ([f64; 2], [f64; 2], f64) {
        let cells = self.placement.cells;
        let cells = if self.is_virtual() && cells.contains(&0) {
            self.virtual_cells(cell)
        } else {
            cells
        };
        let cell = [cell.width.get(), cell.height.get()];
        let [x, y] = self.cell_offset(cell).map(f64::from);
        let cell = cell.map(f64::from);
        let [width, height] = self.placement.source_size.map(f64::from);
        let [columns, rows] = cells.map(f64::from);
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

    fn cell_offset(&self, cell: [u32; 2]) -> [u32; 2] {
        let [x, y] = self.placement.cell_offset;
        [x.min(cell[0] - 1), y.min(cell[1] - 1)]
    }

    fn uncropped_span(&self, cell: [u32; 2]) -> [usize; 2] {
        let requested = self.placement.cells;
        let source = self.placement.source_size;
        let offset = self.cell_offset(cell);
        std::array::from_fn(|axis| {
            if requested[axis] != 0 {
                return requested[axis] as usize;
            }
            let scaled_axis = 1 - axis;
            if requested[scaled_axis] == 0 {
                let extent = u64::from(source[axis]) + u64::from(offset[axis]);
                return extent.div_ceil(u64::from(cell[axis])) as usize;
            }

            // Round the exact ratio up once, at the cell boundary.
            let available = u64::from(requested[scaled_axis]) * u64::from(cell[scaled_axis])
                - u64::from(offset[scaled_axis]);
            let scale_divisor = u64::from(source[scaled_axis]);
            let extent = u128::from(available) * u128::from(source[axis])
                + u128::from(u64::from(offset[axis]) * scale_divisor);
            let cell_extent = u64::from(cell[axis]) * scale_divisor;
            usize::try_from(extent.div_ceil(u128::from(cell_extent))).unwrap_or(usize::MAX)
        })
    }

    fn virtual_cells(&self, cell: PixelSize) -> [u32; 2] {
        let cell_pixels = [cell.width.get(), cell.height.get()];
        std::array::from_fn(|axis| {
            let specified = self.placement.cells[axis];
            if specified == 0 {
                self.placement.source_size[axis].div_ceil(cell_pixels[axis])
            } else {
                specified
            }
        })
    }

    fn set_cell_size(&mut self, cell: PixelSize) {
        let cell_size = [cell.width.get(), cell.height.get()];
        if self.is_virtual() {
            let (offset, size, _) = self.uncropped_geometry(cell);
            let cell_size = cell_size.map(f64::from);
            self.virtual_start = std::array::from_fn(|axis| {
                snap_integer_edge(offset[axis] / cell_size[axis]).floor() as u32
            });
            self.span = std::array::from_fn(|axis| {
                let end =
                    snap_integer_edge((offset[axis] + size[axis]) / cell_size[axis]).ceil() as u32;
                end.saturating_sub(self.virtual_start[axis]) as usize
            });
            return;
        }
        self.span = self.uncropped_span(cell_size);
        if self.vertical_crop != UNCROPPED {
            let (offset, size, _) = self.uncropped_geometry(cell);
            let row_height = f64::from(cell_size[1]);
            let [top, bottom] = self.vertical_crop;
            let mut height = if bottom.is_finite() {
                bottom * size[1] + offset[1]
            } else if self.placement.cells[1] != 0 {
                f64::from(self.placement.cells[1]) * row_height
            } else {
                offset[1] + size[1]
            };
            if top.is_finite() {
                height -= top * size[1] + offset[1];
            }
            let rows = height / row_height;
            // Allow round-off when projecting a crop edge onto a cell boundary.
            let tolerance = (size[1] / row_height).max(rows) * (8.0 * f64::EPSILON);
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
            virtual_start: [0; 2],
            anonymous,
            serial: 0,
            parent: None,
            root: None,
        };
        image.set_cell_size(cell);
        Ok(image)
    }
}

fn snap_integer_edge(pixel: f64) -> f64 {
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
    parent_origins: VirtualParentOrigins,
    anchors: HashMap<u64, usize>,
    relative_indices: Vec<u16>,
    orphaned: Vec<OrphanedImage>,
}

#[derive(Debug)]
struct OrphanedImage {
    id: u32,
    referenced: bool,
}

impl OrphanedImage {
    fn new(id: u32) -> Self {
        Self {
            id,
            referenced: false,
        }
    }
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

    #[inline]
    fn has_cursor_placements(&self) -> bool {
        self.images.len() > self.virtual_count + self.relative_indices.len()
    }

    pub(crate) fn revision(&self) -> Option<u64> {
        (!self.images.is_empty()).then_some(self.revision)
    }

    fn changed(&mut self) {
        self.revision = NEXT_PLACEMENT_REVISION.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn clear(&mut self) {
        self.clear_placements();
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
        if !self.has_cursor_placements() {
            return Ok(Vec::new());
        }
        let history = view.history_rows() as i64;
        let live_rows = view.live_grid().rows() as i64;
        let end = CellPosition {
            row: history as usize + live_rows as usize,
            column: 0,
        };
        let mut anchors = Vec::new();
        for (index, image) in self.images.iter().enumerate() {
            if image.placement.position != PlacementPosition::Cursor {
                continue;
            }
            let (source, destination) = if image.row >= live_rows {
                // Resolve the gap below the live rows once the new layout is known.
                let offset = usize::try_from(image.row - live_rows).unwrap_or(usize::MAX);
                (
                    end,
                    Some(CellPosition {
                        row: offset,
                        column: image.column as usize,
                    }),
                )
            } else {
                (
                    crate::reflow::normalize_position(
                        view,
                        CellPosition {
                            row: (image.row.max(-history) + history) as usize,
                            column: image.column.clamp(0, columns as i64 - 1) as usize,
                        },
                    )?,
                    None,
                )
            };
            anchors.try_reserve(1).map_err(StorageError::from)?;
            anchors.push(GraphicsAnchor {
                source,
                destination,
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
            let Some(destination) = anchor.destination else {
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
        if !self.has_cursor_placements() {
            return;
        }
        self.remove_matching(|image| {
            image.placement.position == PlacementPosition::Cursor
                && (image.column >= columns as i64 || image.row >= rows as i64)
        });
    }

    pub(crate) fn clear_nonvirtual(&mut self) {
        self.remove_matching(|image| !image.is_virtual());
    }

    fn clear_placements(&mut self) {
        self.images.clear();
        self.anchors.clear();
        self.relative_indices.clear();
        self.virtual_count = 0;
        self.virtual_parents = false;
    }

    pub(crate) fn clear_live(&mut self, view: ScreenView<'_>, styles: &StyleTable) {
        self.remove_matching_with_origins(Some((view, styles)), |image, origins| {
            if image.is_virtual() {
                return false;
            }
            if image.virtual_parent().is_some() {
                return image
                    .origin_in_view(0, |parent| origins?.get(&parent).copied().flatten())
                    .is_some_and(|origin| image.intersects_display(origin, view.live_grid()));
            }
            image.rows().end > 0
        });
    }

    pub(crate) fn discard_history_before(&mut self, first_row: i64, cell: PixelSize) {
        if !self.has_cursor_placements() {
            return;
        }
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
        self.scroll_matching(delta, cell, |rows| {
            (rows.start >= source.start && rows.end <= source.end).then(|| destination.clone())
        });
    }

    pub(crate) fn scroll_with_history(
        &mut self,
        available: Range<i64>,
        delta: i64,
        cell: PixelSize,
    ) {
        let first = available.start;
        let end = available.end;
        if delta < 0 && end != i64::MAX {
            // Captured text shifts existing history while live images clip at the margin.
            self.scroll_matching(delta, cell, |rows| {
                if rows.end <= 0 {
                    Some(first..0)
                } else if rows.start >= 0 && rows.end <= end {
                    Some(0..end)
                } else {
                    None
                }
            });
        } else {
            self.scroll(i64::MIN..end, available, delta, cell);
        }
        if end != i64::MAX {
            self.discard_history_before(first, cell);
        }
    }

    pub(crate) fn reverse_page(&mut self, first_retained_row: i64, delta: i64, cell: PixelSize) {
        // The live portion carries the whole placement. Placements entirely in
        // scrollback keep their position.
        self.scroll_matching(delta, cell, |rows| {
            (rows.end > 0).then_some(first_retained_row..i64::MAX)
        });
    }

    #[cold]
    fn scroll_matching(
        &mut self,
        delta: i64,
        cell: PixelSize,
        mut destination: impl FnMut(&Range<i64>) -> Option<Range<i64>>,
    ) {
        // Virtual roots move with the text that contains their placeholders.
        if !self.has_cursor_placements() {
            return;
        }
        let count = self.images.len();
        let mut changed = false;
        self.images.retain_mut(|image| {
            if image.is_virtual() || image.parent.is_some() {
                return true;
            }
            let rows = image.rows();
            let Some(destination) = destination(&rows) else {
                return true;
            };
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
        self.remove_matching_with_origins(
            coordinates.then_some((view, styles)),
            |image, origins| {
                let placement = image.placement;
                let by_image = matches!(
                    target,
                    DeleteTarget::Image { .. } | DeleteTarget::IdRange { .. }
                );
                if !by_image && image.is_virtual() {
                    return false;
                }
                let origin =
                    || image.origin_in_view(0, |parent| origins?.get(&parent).copied().flatten());
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
                        z.is_none_or(|z| placement.z == z)
                            && at_cell(i64::from(column), i64::from(row))
                    }
                    DeleteTarget::Column(column) => origin()
                        .is_some_and(|[x, _]| image.columns_at(x).contains(&i64::from(column))),
                    DeleteTarget::Row(row) => {
                        origin().is_some_and(|[_, y]| image.rows_at(y).contains(&i64::from(row)))
                    }
                    DeleteTarget::Layer(z) => placement.z == z,
                };
                if matches {
                    removed(placement.image.id());
                }
                matches
            },
        );
    }

    fn remove_matching(&mut self, mut matches: impl FnMut(&GraphicsPlacement) -> bool) {
        let count = self.images.len();
        Self::retain_placements(&mut self.images, &mut self.virtual_count, &mut matches);
        self.finish_removal(count);
    }

    fn remove_matching_with_origins(
        &mut self,
        view: Option<(ScreenView<'_>, &StyleTable)>,
        mut matches: impl FnMut(&GraphicsPlacement, Option<&Origins>) -> bool,
    ) {
        let Some((view, styles)) = view.filter(|_| self.virtual_parents) else {
            self.remove_matching(|image| matches(image, None));
            return;
        };
        let origins = self
            .parent_origins
            .collect(&self.images, view, styles, |_| {});
        let count = self.images.len();
        Self::retain_placements(&mut self.images, &mut self.virtual_count, |image| {
            matches(image, Some(origins))
        });
        self.finish_removal(count);
    }

    fn retain_placements(
        images: &mut Vec<GraphicsPlacement>,
        virtual_count: &mut usize,
        mut matches: impl FnMut(&GraphicsPlacement) -> bool,
    ) {
        images.retain(|image| {
            let removed = matches(image);
            if removed && image.is_virtual() {
                *virtual_count -= 1;
            }
            !removed
        });
    }

    fn finish_removal(&mut self, count: usize) {
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
        let replacement = if replaces_image || spec.id != 0 {
            self.images.iter().position(remove)
        } else {
            None
        };
        if !replaces_image
            && evicted.is_empty()
            && let Some(index) = replacement
        {
            let previous = &mut self.images[index];
            if previous.placement.z == spec.z
                && let Some(relative_delta) =
                    placement_offset_delta(previous.placement.position, spec.position)
            {
                if previous.parent.is_some() {
                    image.row = previous.row;
                    image.column = previous.column;
                }
                let moved = image.row != previous.row || image.column != previous.column;
                let layout = Placement {
                    keep_cursor: spec.keep_cursor,
                    ..previous.placement
                };
                if !moved && image.vertical_crop == previous.vertical_crop && spec == layout {
                    previous.placement.keep_cursor = spec.keep_cursor;
                    return Ok(());
                }
                // Keeping the link and draw order preserves all cached graph indices.
                image.serial = previous.serial;
                image.parent = previous.parent;
                image.root = previous.root;
                let shifted = relative_delta != [0; 2];
                if shifted {
                    image.shift_relative(relative_delta);
                }
                self.images[index] = image;
                if shifted {
                    self.shift_relative_descendants(index, relative_delta);
                } else if moved {
                    self.sync_graph(false);
                }
                self.changed();
                return Ok(());
            }
            if !self.anchors.is_empty()
                && previous.placement.z != spec.z
                && let Some(delta) =
                    placement_offset_delta(previous.placement.position, spec.position)
            {
                self.reorder_linked(index, image, delta);
                return Ok(());
            }
        }
        let removes_existing = replacement.is_some();
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
        let retained_serial = replacement
            .filter(|_| !replaces_image)
            .map(|index| self.images[index].serial);
        image.serial = retained_serial.unwrap_or(self.next_serial + 1);
        let mut needs_virtual_origins =
            self.virtual_parents || (image.is_virtual() && !self.anchors.is_empty());
        if let PlacementPosition::Relative(parent) = spec.position {
            let referenced = self.parent(parent).ok_or(PlacementError::MissingParent)?;
            image.parent = Some(referenced.serial);
            needs_virtual_origins |=
                referenced.is_virtual() || referenced.virtual_parent().is_some();
        }
        if needs_virtual_origins {
            // Coordinate removal must fit both maps regardless of which placeholders are present.
            let virtual_count = self.virtual_count + usize::from(image.is_virtual());
            self.parent_origins
                .reserve(virtual_count)
                .map_err(|_| PlacementError::Quota)?;
        }
        if !self.anchors.is_empty() || image.parent.is_some() {
            self.anchors
                .try_reserve(self.images.len() + 1 - self.anchors.len())
                .map_err(|_| PlacementError::Quota)?;
            self.relative_indices
                .try_reserve(usize::from(image.parent.is_some()))
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
        let position = self.draw_position(&image);
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

    fn draw_position(&self, placement: &GraphicsPlacement) -> usize {
        let order = placement.draw_order();
        self.images
            .partition_point(|entry| entry.draw_order() <= order)
    }

    #[cold]
    fn reorder_linked(&mut self, index: usize, mut image: GraphicsPlacement, delta: [i64; 2]) {
        let previous = &self.images[index];
        image.serial = previous.serial;
        image.parent = previous.parent;
        image.root = previous.root;
        let moved = if image.parent.is_some() {
            image.row = previous.row;
            image.column = previous.column;
            false
        } else {
            image.row != previous.row || image.column != previous.column
        };
        if delta != [0; 2] {
            image.shift_relative(delta);
            self.shift_relative_descendants(index, delta);
        }

        self.images.remove(index);
        let position = self.draw_position(&image);
        self.images.insert(position, image);
        if index != position {
            self.reindex();
            self.refresh_root_indices();
        }
        if moved {
            self.sync_graph(false);
        }
        self.changed();
    }

    fn shift_relative_descendants(&mut self, index: usize, delta: [i64; 2]) {
        let image = &self.images[index];
        let serial = image.serial;
        let root = image.root.expect("A relative placement has a root").serial;
        for &entry_index in &self.relative_indices {
            let entry_index = usize::from(entry_index);
            let entry = &self.images[entry_index];
            if entry_index == index
                || entry
                    .root
                    .is_none_or(|entry_root| entry_root.serial != root)
            {
                continue;
            }
            let mut parent = entry.parent;
            while let Some(ancestor) = parent {
                if ancestor == serial {
                    self.images[entry_index].shift_relative(delta);
                    break;
                }
                if ancestor == root {
                    break;
                }
                parent = self.images[self.anchors[&ancestor]].parent;
            }
        }
    }

    fn reindex(&mut self) {
        self.anchors.clear();
        self.relative_indices.clear();
        for (index, image) in self.images.iter().enumerate() {
            self.anchors.insert(image.serial, index);
            if image.parent.is_some() {
                self.relative_indices.push(index as u16);
            }
        }
    }

    fn refresh_root_indices(&mut self) {
        for &index in &self.relative_indices {
            let root = self.images[usize::from(index)]
                .root
                .as_mut()
                .expect("A retained relative placement has a root");
            root.index = self.anchors[&root.serial] as u16;
        }
    }

    fn index_inserted(&mut self, index: usize) {
        if index + 1 == self.images.len() {
            self.anchors.insert(self.images[index].serial, index);
            if self.images[index].parent.is_some() {
                self.relative_indices.push(index as u16);
            }
        } else {
            self.reindex();
            for &relative in &self.relative_indices {
                if let Some(root) = &mut self.images[usize::from(relative)].root
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
            for &index in &self.relative_indices {
                let entry = &self.images[usize::from(index)];
                if entry.serial != image.serial {
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
            fn follow_root(images: &mut [GraphicsPlacement], index: usize, root: PlacementRoot) {
                let anchor = &images[usize::from(root.index)];
                let column = anchor.column + root.offset[0];
                let row = anchor.row + root.offset[1];
                let image = &mut images[index];
                image.column = column;
                image.row = row;
            }
            if self.relative_indices.len() * 2 <= self.images.len() {
                for &index in &self.relative_indices {
                    let index = usize::from(index);
                    let root = self.images[index]
                        .root
                        .expect("A relative placement has a root");
                    follow_root(&mut self.images, index, root);
                }
            } else {
                // Dense graphs benefit from traversing the placements in order.
                for index in 0..self.images.len() {
                    if let Some(root) = self.images[index].root {
                        follow_root(&mut self.images, index, root);
                    }
                }
            }
            return;
        }
        self.reindex();
        self.virtual_parents = false;
        let mut invalid = false;
        let mut relative = false;
        for relative_index in 0..self.relative_indices.len() {
            let index = usize::from(self.relative_indices[relative_index]);
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
                    orphaned.push(OrphanedImage::new(id));
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
            self.relative_indices.clear();
        }
    }

    pub(crate) fn has_orphans(&self) -> bool {
        !self.orphaned.is_empty()
    }

    pub(crate) fn reap_orphans(&mut self, images: &mut ImageStore) {
        if !self.has_orphans() {
            return;
        }
        self.orphaned.sort_unstable_by_key(|orphan| orphan.id);
        self.orphaned.dedup_by_key(|orphan| orphan.id);
        for placement in &self.images {
            let id = placement.placement.image.id();
            if let Ok(index) = self.orphaned.binary_search_by_key(&id, |orphan| orphan.id) {
                self.orphaned[index].referenced = true;
            }
        }
        self.orphaned.retain(|orphan| !orphan.referenced);
        if !self.orphaned.is_empty() {
            images.retain_ids(|id| {
                self.orphaned
                    .binary_search_by_key(&id, |orphan| orphan.id)
                    .is_err()
            });
        }
        self.orphaned.clear();
    }
}

fn placement_offset_delta(before: PlacementPosition, after: PlacementPosition) -> Option<[i64; 2]> {
    if before == after {
        return Some([0; 2]);
    }
    let (PlacementPosition::Relative(before), PlacementPosition::Relative(after)) = (before, after)
    else {
        return None;
    };
    if before.image_id != after.image_id || before.placement_id != after.placement_id {
        return None;
    }
    Some(std::array::from_fn(|axis| {
        i64::from(after.offset[axis]) - i64::from(before.offset[axis])
    }))
}

pub(crate) struct Graphics {
    decoder: Decoder,
    images: [ImageStore; 2],
    retained_ids: Vec<u32>,
    reply: Option<Reply>,
    active: bool,
    has_relative: bool,
    animation_buffer: Option<ScreenBuffer>,
    animation_visibility: AnimationVisibility,
}

impl Default for Graphics {
    fn default() -> Self {
        Self {
            decoder: Decoder::default(),
            images: std::array::from_fn(|_| ImageStore::new(64 * 1024 * 1024, 1024)),
            retained_ids: Vec::new(),
            reply: None,
            active: false,
            has_relative: false,
            animation_buffer: None,
            animation_visibility: AnimationVisibility::default(),
        }
    }
}

impl Graphics {
    pub(crate) fn images(&self, buffer: ScreenBuffer) -> &ImageStore {
        &self.images[buffer as usize]
    }

    pub(crate) fn clear_unplaced(&mut self, screen: &Screen) {
        let images = &mut self.images[screen.active_buffer() as usize];
        if images.is_empty() {
            images.abort_transfer();
            return;
        }
        let placements = screen.graphics_placements();
        if placements.is_empty() {
            images.clear();
            return;
        }

        images.abort_transfer();
        self.retained_ids.clear();
        let ids = placements.iter().map(|image| image.placement.image.id());
        if self.retained_ids.try_reserve(placements.len()).is_err() {
            images.retain_ids(|id| ids.clone().any(|placed| placed == id));
            return;
        }
        self.retained_ids.extend(ids.filter(|&id| id != 0));
        self.retained_ids.sort_unstable();
        self.retained_ids.dedup();
        images.retain_ids(|id| self.retained_ids.binary_search(&id).is_ok());
    }

    pub(crate) fn poll_animations(
        &mut self,
        screen: &Screen,
        styles: &StyleTable,
        clock: impl FnOnce() -> std::time::Instant,
    ) {
        let buffer = screen.active_buffer();
        if self.animation_buffer != Some(buffer) {
            if let Some(previous) = self.animation_buffer.replace(buffer)
                && self.images[previous as usize]
                    .animation_membership()
                    .is_some()
            {
                self.images[previous as usize].set_animation_visibility([]);
            }
            self.animation_visibility.invalidate();
        }
        let store = &mut self.images[buffer as usize];
        self.animation_visibility.update(screen, styles, store);
        store.poll_animations(clock);
    }

    pub(crate) fn reset(&mut self) {
        self.decoder.cancel();
        self.images.iter_mut().for_each(ImageStore::clear);
        self.reply = None;
        self.active = false;
        self.has_relative = false;
        self.animation_visibility.invalidate();
    }

    pub(crate) fn finish(&mut self) {
        self.decoder.cancel();
        self.images.iter_mut().for_each(ImageStore::abort_transfer);
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
                let images = &mut self.images[screen.active_buffer() as usize];
                match self.decoder.finish() {
                    Some(Ok(command)) => {
                        if let Some(Ok(delete)) = command.decode_delete() {
                            images.abort_transfer();
                            delete_images(delete, screen, images, styles);
                            return false;
                        }
                        debug_assert!(self.reply.is_none());
                        self.reply = images.receive(command, |change| {
                            let relative = matches!(&change, lune_kitty_graphics::ImageChange::Place { placement, .. }
                                if matches!(placement.position, PlacementPosition::Relative(_)));
                            screen.apply_image(change)?;
                            self.has_relative |= relative;
                            Ok(())
                        });
                        return self.reply.is_some();
                    }
                    Some(Err(error)) => {
                        debug_assert!(self.reply.is_none());
                        self.reply = images.reject_chunk(error);
                        return self.reply.is_some();
                    }
                    None => {}
                }
            }
            ApcEvent::Cancel => {
                self.active = false;
                if self.decoder.cancel() {
                    self.images[screen.active_buffer() as usize].abort_transfer();
                }
            }
        }
        false
    }

    #[inline]
    pub(crate) fn reap_orphans(&mut self, screen: &mut Screen) {
        if self.has_relative && screen.has_graphics_orphans() {
            screen.reap_graphics_orphans(&mut self.images);
        }
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
    match delete.target {
        DeleteTarget::Image { id, placement_id } => {
            let mut removed = false;
            screen.delete_placements(delete.target, styles, |_| removed = true);
            if placement_id == 0
                || (removed
                    && !screen
                        .graphics_placements()
                        .iter()
                        .any(|placement| placement.placement.image.id() == id))
            {
                images.remove_id(id);
            }
            if images.has_anonymous_images() {
                images.retain_ids(|_| true);
            }
            return;
        }
        target @ DeleteTarget::IdRange { .. } => {
            screen.delete_placements(target, styles, |_| {});
            images.retain_ids(|id| !target.selects_image(id));
            return;
        }
        _ => {}
    }
    const INLINE_IDS: usize = 32;
    if screen.graphics_placements().len() <= INLINE_IDS {
        delete_unreferenced::<INLINE_IDS>(delete.target, screen, images, styles);
    } else {
        delete_unreferenced::<MAX_PLACEMENTS>(delete.target, screen, images, styles);
    }
}

#[inline(never)]
fn delete_unreferenced<const N: usize>(
    target: DeleteTarget,
    screen: &mut Screen,
    images: &mut ImageStore,
    styles: &StyleTable,
) {
    let mut ids = [0; N];
    let mut count = 0;
    screen.delete_placements(target, styles, |id| {
        if id != 0 {
            ids[count] = id;
            count += 1;
        }
    });
    let (removed, available) = ids.split_at_mut(count);
    let placements = screen.graphics_placements();
    match removed {
        [] => {}
        [id] => {
            if !placements
                .iter()
                .any(|image| image.placement.image.id() == *id)
            {
                images.remove_id(*id);
            }
        }
        _ => {
            // Removed and surviving placement lists share the original placement bound.
            let referenced = &mut available[..placements.len()];
            for (id, placement) in referenced.iter_mut().zip(placements) {
                *id = placement.placement.image.id();
            }
            removed.sort_unstable();
            referenced.sort_unstable();
            images.retain_ids(|id| {
                removed.binary_search(&id).is_err() || referenced.binary_search(&id).is_ok()
            });
            return;
        }
    }
    if images.has_anonymous_images() {
        images.retain_ids(|_| true);
    }
}

#[cfg(test)]
mod quota_tests {
    use super::*;
    use crate::ScreenBuffer;

    #[test]
    fn quota_eviction_is_buffer_local_and_protects_relative_ancestry() {
        let mut graphics = Graphics {
            images: std::array::from_fn(|_| ImageStore::new(12, 3)),
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
        assert!(graphics.images(ScreenBuffer::Alternate).get(1).is_none());
        assert!((2..=3).all(|id| graphics.images(ScreenBuffer::Alternate).get(id).is_some()));
        assert_eq!(
            receive(&mut graphics, &mut screen, b"Gi=4,s=1,v=1;AAAA/w=="),
            b"OK"
        );
        assert_eq!(
            receive(&mut graphics, &mut screen, b"Gi=5,s=1,v=1;AAAA/w=="),
            b"OK"
        );
        assert!(graphics.images(ScreenBuffer::Alternate).get(3).is_none());
        assert!(graphics.images(ScreenBuffer::Alternate).get(2).is_some());

        screen.select_buffer(ScreenBuffer::Primary);
        assert_eq!(screen.graphics_placements()[0].placement.image.id(), 1);
        for id in [4, 5] {
            assert!(graphics.images(ScreenBuffer::Primary).get(id).is_none());
            let command = format!("Gi={id},s=1,v=1;/wAA/w==");
            assert_eq!(
                receive(&mut graphics, &mut screen, command.as_bytes()),
                b"OK"
            );
        }
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
        assert!(graphics.images(ScreenBuffer::Primary).get(5).is_none());
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
        assert!(
            [1, 4, 6]
                .into_iter()
                .all(|id| graphics.images(ScreenBuffer::Primary).get(id).is_some())
        );
        assert_eq!(
            receive(&mut graphics, &mut screen, b"Ga=p,i=6,p=1,C=1"),
            b"OK"
        );
        assert_eq!(
            receive(&mut graphics, &mut screen, b"Gi=7,s=1,v=1;AAAA/w=="),
            b"OK"
        );
        let [independent] = screen.graphics_placements() else {
            panic!("The independent placement survives eviction");
        };
        assert_eq!(independent.placement.image.id(), 6);
        graphics.reap_orphans(&mut screen);
        assert!(
            [1, 4]
                .into_iter()
                .all(|id| graphics.images(ScreenBuffer::Primary).get(id).is_none())
        );
        assert!(graphics.images(ScreenBuffer::Primary).get(6).is_some());
        assert!(graphics.images(ScreenBuffer::Primary).get(7).is_some());

        for command in [
            b"Gi=8,s=1,v=1;AAAA/w==".as_slice(),
            b"Gi=9,s=1,v=1;AAAA/w==",
            b"Gi=7,s=3,v=1;AAAAAAAAAAAAAAAA",
        ] {
            assert_eq!(receive(&mut graphics, &mut screen, command), b"OK");
        }
        assert_eq!(
            graphics
                .images(ScreenBuffer::Primary)
                .get(7)
                .unwrap()
                .dimensions(),
            (3, 1)
        );
        assert!(graphics.images(ScreenBuffer::Primary).get(8).is_none());
        assert!(graphics.images(ScreenBuffer::Primary).get(9).is_none());
        assert!(
            [2, 4, 5]
                .into_iter()
                .all(|id| graphics.images(ScreenBuffer::Alternate).get(id).is_some())
        );
    }
}
