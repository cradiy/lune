use std::collections::HashMap;

use lune_terminal::{
    GraphicsPlaceholderCache, GraphicsPlaceholderRun, PixelSize, visit_graphics_placeholders,
};

use crate::{PresentationFrame, session::GraphicsStamp};

use super::ImageLocation;

struct PlacedRun {
    run: GraphicsPlaceholderRun,
    piece: usize,
}

pub(super) fn collect(
    frame: &PresentationFrame<'_>,
    cell: PixelSize,
    stamp: Option<GraphicsStamp>,
    parents: &mut GraphicsPlaceholderCache,
) -> HashMap<u64, Vec<ImageLocation>> {
    let mut pieces = HashMap::<u64, Vec<ImageLocation>>::new();
    let mut previous = Vec::<PlacedRun>::new();
    let mut current = Vec::<PlacedRun>::new();
    let mut row = -1;
    let mut next_previous = 0;
    let append = |run: GraphicsPlaceholderRun| {
        let [x, y] = run.position;
        if y != row {
            std::mem::swap(&mut previous, &mut current);
            current.clear();
            if y != row + 1 {
                previous.clear();
            }
            next_previous = 0;
            row = y;
        }
        while previous
            .get(next_previous)
            .is_some_and(|prior| prior.run.position[0] < x)
        {
            next_previous += 1;
        }
        let pieces = pieces.entry(run.placement).or_default();
        let piece = if let Some(prior) = previous.get(next_previous).filter(|prior| {
            prior.run.position[0] == x
                && prior.run.placement == run.placement
                && prior.run.origin == run.origin
                && prior.run.columns == run.columns
        }) {
            pieces[prior.piece].clip[1].end += 1;
            prior.piece
        } else {
            let piece = pieces.len();
            pieces.push(ImageLocation {
                origin: run.origin,
                clip: [x..x + i64::from(run.columns), y..y + 1],
            });
            piece
        };
        current.push(PlacedRun { run, piece });
    };
    let placements = frame.graphics_placements();
    if let Some(stamp) = stamp.filter(|_| frame.has_virtual_parents()) {
        let _ = parents.visit(
            stamp.placement_revision(),
            placements,
            frame.viewport,
            frame.styles,
            cell,
            Some(append),
        );
    } else {
        let _ = visit_graphics_placeholders(
            placements,
            frame.viewport.iter_rows(),
            frame.styles,
            append,
        );
    }
    pieces
}
