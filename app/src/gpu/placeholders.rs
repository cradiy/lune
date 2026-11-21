use std::collections::HashMap;

use lune_terminal::visit_graphics_placeholders;

use crate::PresentationFrame;

use super::ImageLocation;

pub(super) struct PlaceholderRuns {
    pub(super) top_left: [i64; 2],
    pub(super) locations: Vec<ImageLocation>,
}

pub(super) fn collect(frame: &PresentationFrame<'_>) -> HashMap<u64, PlaceholderRuns> {
    let mut pieces = HashMap::<_, PlaceholderRuns>::new();
    if !frame.has_virtual_graphics() {
        return pieces;
    }
    let _ = visit_graphics_placeholders(
        frame.graphics_placements(),
        frame.viewport.iter_rows(),
        frame.styles,
        |tile| {
            let [x, y] = tile.position;
            let runs = pieces
                .entry(tile.placement)
                .or_insert_with(|| PlaceholderRuns {
                    top_left: tile.position,
                    locations: Vec::new(),
                });
            runs.top_left[0] = runs.top_left[0].min(x);
            runs.top_left[1] = runs.top_left[1].min(y);
            if let Some(last) = runs.locations.last_mut()
                && last.origin == tile.origin
                && last.clip[0].end == x
                && last.clip[1].start == y
            {
                last.clip[0].end += 1;
            } else {
                runs.locations.push(ImageLocation {
                    origin: tile.origin,
                    clip: [x..x + 1, y..y + 1],
                });
            }
        },
    );
    pieces
}
