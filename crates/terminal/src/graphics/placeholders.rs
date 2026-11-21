use std::collections::HashMap;

use crate::{Color, GRAPHICS_PLACEHOLDER, GraphicsPlaceholder, Row, StorageError, StyleTable};

use super::GraphicsPlacement;

#[derive(Clone, Copy, Debug)]
pub struct GraphicsPlaceholderCell {
    pub placement: u64,
    pub position: [i64; 2],
    pub origin: [i64; 2],
}

/// Visits the valid placeholder tiles in view order. Coordinates are local to the supplied rows.
pub fn visit_graphics_placeholders<'a>(
    placements: &[GraphicsPlacement],
    rows: impl IntoIterator<Item = &'a Row>,
    styles: &StyleTable,
    mut visit: impl FnMut(GraphicsPlaceholderCell),
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
    for (row_index, row) in rows.into_iter().enumerate() {
        let mut previous: Option<(usize, [Color; 2], GraphicsPlaceholder)> = None;
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
            let Some(placement) = lookup.get(&(glyph.image_id, glyph.placement_id)) else {
                continue;
            };
            let [columns, rows] = placement.placement.cells;
            if u32::from(glyph.column) >= columns || u32::from(glyph.row) >= rows {
                continue;
            }
            let position = [column as i64, row_index as i64];
            visit(GraphicsPlaceholderCell {
                placement: placement.serial(),
                position,
                origin: [
                    position[0] - i64::from(glyph.column),
                    position[1] - i64::from(glyph.row),
                ],
            });
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
