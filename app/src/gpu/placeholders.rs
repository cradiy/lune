use std::collections::HashMap;

use lune_terminal::visit_graphics_placeholders;

use crate::PresentationFrame;

use super::ImageLocation;

pub(super) fn collect(frame: &PresentationFrame<'_>) -> HashMap<u64, Vec<ImageLocation>> {
    let mut pieces = HashMap::<_, Vec<ImageLocation>>::new();
    if !frame.has_virtual_graphics() {
        return pieces;
    }
    let _ = visit_graphics_placeholders(
        frame.graphics_placements(),
        frame.viewport.iter_rows(),
        frame.styles,
        |tile| {
            let [x, y] = tile.position;
            let runs = pieces.entry(tile.placement).or_default();
            if let Some(last) = runs.last_mut()
                && last.origin == tile.origin
                && last.clip[0].end == x
                && last.clip[1].start == y
            {
                last.clip[0].end += 1;
            } else {
                runs.push(ImageLocation {
                    origin: tile.origin,
                    clip: [x..x + 1, y..y + 1],
                });
            }
        },
    );
    pieces
}
