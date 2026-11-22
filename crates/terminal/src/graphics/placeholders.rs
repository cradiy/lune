use std::collections::HashMap;

use crate::{
    Color, GRAPHICS_PLACEHOLDER, GraphicsPlaceholder, PixelSize, Row, RowRevision, ScreenView,
    StorageError, StyleTable, Viewport,
};

use super::GraphicsPlacement;

type PlaceholderIndex = HashMap<(u32, u32), usize>;
pub(super) type Origins = HashMap<u64, Option<[i64; 2]>>;

#[derive(Debug, Default)]
pub(super) struct VirtualParentOrigins {
    lookup: PlaceholderIndex,
    origins: Origins,
}

impl VirtualParentOrigins {
    pub(super) fn reserve(&mut self, virtual_count: usize) -> Result<(), StorageError> {
        self.lookup
            .try_reserve((virtual_count * 2).saturating_sub(self.lookup.len()))?;
        self.origins
            .try_reserve(virtual_count.saturating_sub(self.origins.len()))?;
        Ok(())
    }

    pub(super) fn collect(
        &mut self,
        placements: &[GraphicsPlacement],
        view: ScreenView<'_>,
        styles: &StyleTable,
        mut visit: impl FnMut(GraphicsPlaceholderRun),
    ) -> &Origins {
        self.origins.clear();
        for parent in placements
            .iter()
            .filter_map(GraphicsPlacement::virtual_parent)
        {
            self.origins.entry(parent).or_insert(None);
        }
        index_placeholders(placements, &mut self.lookup);
        visit_indexed_rows(
            placements,
            view.placeholder_rows(),
            styles,
            &self.lookup,
            |tile| {
                if let Some(origin) = self.origins.get_mut(&tile.placement) {
                    let minimum = origin.get_or_insert(tile.position);
                    minimum[0] = minimum[0].min(tile.position[0]);
                    minimum[1] = minimum[1].min(tile.position[1]);
                }
                visit(tile);
            },
        );
        &self.origins
    }
}

#[derive(Clone, Copy, Debug)]
pub struct GraphicsPlaceholderRun {
    pub placement: u64,
    pub position: [i64; 2],
    pub origin: [i64; 2],
    pub columns: u32,
}

#[derive(Default)]
pub struct GraphicsPlaceholderCache {
    sources: Option<ParentSources>,
    live_rows: Vec<Option<RowRevision>>,
    parents: VirtualParentOrigins,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ParentSources {
    placement_revision: u64,
    history: Option<[RowRevision; 2]>,
    cell: PixelSize,
}

impl GraphicsPlaceholderCache {
    /// Returns a virtual parent's origin in live-buffer coordinates.
    pub fn origin(&self, serial: u64) -> Option<[i64; 2]> {
        self.parents.origins.get(&serial).copied().flatten()
    }

    /// Resolves virtual parents for the supplied placement revision and optionally visits
    /// placeholder runs in viewport coordinates.
    pub fn visit(
        &mut self,
        placement_revision: u64,
        placements: &[GraphicsPlacement],
        viewport: Viewport<'_>,
        styles: &StyleTable,
        cell: PixelSize,
        mut visit: Option<impl FnMut(GraphicsPlaceholderRun)>,
    ) -> Result<(), StorageError> {
        let view = viewport.source();
        let sources = ParentSources {
            placement_revision,
            history: view.history_endpoints(),
            cell,
        };
        let revisions = || {
            view.iter_live_rows()
                .map(Row::graphics_placeholder_revision)
        };
        if self.sources == Some(sources) && revisions().eq(self.live_rows.iter().copied()) {
            if let Some(visit) = visit {
                visit_indexed_rows(
                    placements,
                    number_rows(viewport.iter_rows()),
                    styles,
                    &self.parents.lookup,
                    visit,
                );
            }
            return Ok(());
        }

        self.sources = None;
        self.parents.origins.clear();
        self.live_rows.clear();
        self.live_rows.try_reserve(revisions().len())?;
        let virtual_count = placements.iter().filter(|entry| entry.is_virtual()).count();
        self.parents.reserve(virtual_count)?;
        let offset = viewport.offset() as i64;
        let rows = 0..viewport.rows() as i64;
        self.parents.collect(placements, view, styles, |mut run| {
            if let Some(visit) = &mut visit {
                run.position[1] += offset;
                if rows.contains(&run.position[1]) {
                    run.origin[1] += offset;
                    visit(run);
                }
            }
        });
        self.live_rows.extend(revisions());
        self.sources = Some(sources);
        Ok(())
    }
}

/// Visits runs of adjacent placeholder cells covered by the image in view order.
/// Coordinates are local to the supplied rows.
pub fn visit_graphics_placeholders<'a>(
    placements: &[GraphicsPlacement],
    rows: impl IntoIterator<Item = &'a Row>,
    styles: &StyleTable,
    visit: impl FnMut(GraphicsPlaceholderRun),
) -> Result<(), StorageError> {
    visit_placeholder_rows(placements, number_rows(rows), styles, visit)
}

fn number_rows<'a>(
    rows: impl IntoIterator<Item = &'a Row>,
) -> impl Iterator<Item = (i64, &'a Row)> {
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| (index as i64, row))
}

fn visit_placeholder_rows<'a>(
    placements: &[GraphicsPlacement],
    rows: impl IntoIterator<Item = (i64, &'a Row)>,
    styles: &StyleTable,
    visit: impl FnMut(GraphicsPlaceholderRun),
) -> Result<(), StorageError> {
    let count = placements
        .iter()
        .filter(|placement| placement.is_virtual())
        .count();
    if count == 0 {
        return Ok(());
    }
    let mut lookup = HashMap::new();
    lookup.try_reserve(count * 2).map_err(StorageError::from)?;
    index_placeholders(placements, &mut lookup);
    visit_indexed_rows(placements, rows, styles, &lookup, visit);
    Ok(())
}

fn index_placeholders(placements: &[GraphicsPlacement], lookup: &mut PlaceholderIndex) {
    lookup.clear();
    for (index, placement) in placements
        .iter()
        .enumerate()
        .filter(|(_, item)| item.is_virtual())
    {
        let image = placement.placement.image.id();
        lookup
            .entry((image, 0))
            .and_modify(|current| {
                if placement.serial() < placements[*current].serial() {
                    *current = index;
                }
            })
            .or_insert(index);
        if placement.placement.id != 0 {
            lookup.insert((image, placement.placement.id), index);
        }
    }
}

fn visit_indexed_rows<'a>(
    placements: &[GraphicsPlacement],
    rows: impl IntoIterator<Item = (i64, &'a Row)>,
    styles: &StyleTable,
    lookup: &PlaceholderIndex,
    mut visit: impl FnMut(GraphicsPlaceholderRun),
) {
    let mut last_key = None;
    let mut last_placement = None;
    for (row_index, row) in rows {
        if !row.has_graphics_placeholder() {
            continue;
        }
        let mut previous: Option<(usize, [Color; 2], GraphicsPlaceholder)> = None;
        let mut pending: Option<GraphicsPlaceholderRun> = None;
        let mut next_extended = 0;
        for (column, cell) in row.cells().iter().enumerate() {
            if !cell.has_character(GRAPHICS_PLACEHOLDER) {
                continue;
            }
            let Some(style) = styles.get(cell.style()) else {
                continue;
            };
            let colors = [style.foreground, style.underline_color];
            let left = previous
                .filter(|(index, old_colors, _)| *index + 1 == column && *old_colors == colors)
                .map(|(_, _, placeholder)| placeholder);
            let glyph = if cell.has_extended_text() {
                let marks = row
                    .next_extended_text(column, &mut next_extended)
                    .strip_prefix(GRAPHICS_PLACEHOLDER)
                    .unwrap_or_default();
                GraphicsPlaceholder::decode(
                    marks.chars(),
                    color_id(colors[0]),
                    color_id(colors[1]),
                    left,
                )
            } else if let Some(previous) = left
                && let Some(column) = previous.column.checked_add(1)
            {
                Some(GraphicsPlaceholder { column, ..previous })
            } else {
                GraphicsPlaceholder::decode(
                    std::iter::empty(),
                    color_id(colors[0]),
                    color_id(colors[1]),
                    left,
                )
            };
            let Some(glyph) = glyph else {
                continue;
            };
            previous = Some((column, colors, glyph));
            let key = (glyph.image_id, glyph.placement_id);
            if last_key != Some(key) {
                last_placement = lookup.get(&key).map(|&index| &placements[index]);
                last_key = Some(key);
            }
            let Some(placement) = last_placement else {
                continue;
            };
            let [first_column, first_row] = placement.virtual_start;
            let column_in_content = u32::from(glyph.column).wrapping_sub(first_column) as usize;
            let row_in_content = u32::from(glyph.row).wrapping_sub(first_row) as usize;
            let [columns, rows] = placement.span;
            if column_in_content >= columns || row_in_content >= rows {
                continue;
            }
            let position = [column as i64, row_index];
            let origin = [
                position[0] - i64::from(glyph.column),
                position[1] - i64::from(glyph.row),
            ];
            if let Some(run) = &mut pending
                && run.placement == placement.serial()
                && run.origin == origin
                && run.position[0] + i64::from(run.columns) == position[0]
            {
                run.columns += 1;
                continue;
            }
            if let Some(run) = pending.replace(GraphicsPlaceholderRun {
                placement: placement.serial(),
                position,
                origin,
                columns: 1,
            }) {
                visit(run);
            }
        }
        if let Some(run) = pending {
            visit(run);
        }
    }
}

fn color_id(color: Color) -> u32 {
    match color {
        Color::Default => 0,
        Color::Indexed(id) => u32::from(id),
        Color::Rgb { red, green, blue } => u32::from_be_bytes([0, red, green, blue]),
    }
}
