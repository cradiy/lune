use std::collections::HashMap;

use lune_terminal::{
    GraphicsPlaceholderRun, visit_graphics_placeholders, visit_graphics_placeholders_with_parents,
};

use crate::PresentationFrame;

use super::ImageLocation;

#[derive(Default)]
pub(super) struct PlaceholderLayout {
    pub pieces: HashMap<u64, Vec<ImageLocation>>,
    pub parents: HashMap<u64, Option<[i64; 2]>>,
}

struct PlacedRun {
    run: GraphicsPlaceholderRun,
    piece: usize,
}

pub(super) fn collect(frame: &PresentationFrame<'_>) -> PlaceholderLayout {
    let mut layout = PlaceholderLayout::default();
    if !frame.has_virtual_graphics() {
        return layout;
    }
    let mut previous = Vec::<PlacedRun>::new();
    let mut current = Vec::<PlacedRun>::new();
    let mut row = -1;
    let mut next_previous = 0;
    let mut append = |run: GraphicsPlaceholderRun| {
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
        let pieces = layout.pieces.entry(run.placement).or_default();
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
    if frame.has_virtual_parents() {
        let offset = frame.viewport.offset() as i64;
        let rows = 0..frame.viewport.rows() as i64;
        layout.parents = visit_graphics_placeholders_with_parents(
            frame.graphics_placements(),
            frame.viewport.source(),
            frame.styles,
            |mut run| {
                run.position[1] += offset;
                if rows.contains(&run.position[1]) {
                    run.origin[1] += offset;
                    append(run);
                }
            },
        )
        .unwrap_or_default();
    } else {
        let _ = visit_graphics_placeholders(
            frame.graphics_placements(),
            frame.viewport.iter_rows(),
            frame.styles,
            append,
        );
    }
    layout
}
