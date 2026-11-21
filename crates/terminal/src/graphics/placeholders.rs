use std::collections::HashMap;

use crate::{
    Color, GRAPHICS_PLACEHOLDER, GraphicsPlaceholder, Row, ScreenView, StorageError, StyleTable,
};

use super::GraphicsPlacement;

#[derive(Clone, Copy, Debug)]
pub struct GraphicsPlaceholderRun {
    pub placement: u64,
    pub position: [i64; 2],
    pub origin: [i64; 2],
    pub columns: u32,
}

/// Visits placeholders and resolves virtual parents in live-buffer coordinates,
/// including retained history.
pub fn visit_graphics_placeholders_with_parents(
    placements: &[GraphicsPlacement],
    view: ScreenView<'_>,
    styles: &StyleTable,
    mut visit: impl FnMut(GraphicsPlaceholderRun),
) -> Result<HashMap<u64, Option<[i64; 2]>>, StorageError> {
    let mut origins = HashMap::new();
    for parent in placements
        .iter()
        .filter_map(GraphicsPlacement::virtual_parent)
    {
        if !origins.contains_key(&parent) {
            origins.try_reserve(1).map_err(StorageError::from)?;
            origins.insert(parent, None);
        }
    }
    visit_placeholder_rows(placements, view.placeholder_rows(), styles, |tile| {
        if let Some(origin) = origins.get_mut(&tile.placement) {
            let minimum = origin.get_or_insert(tile.position);
            minimum[0] = minimum[0].min(tile.position[0]);
            minimum[1] = minimum[1].min(tile.position[1]);
        }
        visit(tile);
    })?;
    Ok(origins)
}

/// Visits runs of adjacent placeholder cells covered by the image in view order.
/// Coordinates are local to the supplied rows.
pub fn visit_graphics_placeholders<'a>(
    placements: &[GraphicsPlacement],
    rows: impl IntoIterator<Item = &'a Row>,
    styles: &StyleTable,
    visit: impl FnMut(GraphicsPlaceholderRun),
) -> Result<(), StorageError> {
    visit_placeholder_rows(
        placements,
        rows.into_iter()
            .enumerate()
            .map(|(index, row)| (index as i64, row)),
        styles,
        visit,
    )
}

fn visit_placeholder_rows<'a>(
    placements: &[GraphicsPlacement],
    rows: impl IntoIterator<Item = (i64, &'a Row)>,
    styles: &StyleTable,
    mut visit: impl FnMut(GraphicsPlaceholderRun),
) -> Result<(), StorageError> {
    let virtual_placements = placements.iter().filter(|placement| placement.is_virtual());
    let count = virtual_placements.clone().count();
    if count == 0 {
        return Ok(());
    }
    let mut lookup = HashMap::new();
    lookup.try_reserve(count * 2).map_err(StorageError::from)?;
    for placement in virtual_placements {
        let image = placement.placement.image.id();
        lookup
            .entry((image, 0))
            .and_modify(|current: &mut &GraphicsPlacement| {
                if placement.serial() < current.serial() {
                    *current = placement;
                }
            })
            .or_insert(placement);
        if placement.placement.id != 0 {
            lookup.insert((image, placement.placement.id), placement);
        }
    }
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
            let marks = if cell.has_extended_text() {
                row.next_extended_text(column, &mut next_extended)
                    .strip_prefix(GRAPHICS_PLACEHOLDER)
                    .unwrap_or_default()
            } else {
                ""
            };
            let Some(glyph) = GraphicsPlaceholder::decode(
                marks.chars(),
                color_id(colors[0]),
                color_id(colors[1]),
                left,
            ) else {
                continue;
            };
            previous = Some((column, colors, glyph));
            let key = (glyph.image_id, glyph.placement_id);
            if last_key != Some(key) {
                last_placement = lookup.get(&key).copied();
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
    Ok(())
}

fn color_id(color: Color) -> u32 {
    match color {
        Color::Default => 0,
        Color::Indexed(id) => u32::from(id),
        Color::Rgb { red, green, blue } => u32::from_be_bytes([0, red, green, blue]),
    }
}
