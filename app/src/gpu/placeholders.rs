use std::collections::HashMap;

use lune_terminal::{GraphicsPlaceholderCache, GraphicsPlaceholderRun, PixelSize, StorageError};

use crate::{PresentationFrame, session::GraphicsStamp};

use super::ImageLocation;

struct PlacedRun {
    run: GraphicsPlaceholderRun,
    piece: usize,
}

#[derive(Default)]
pub(super) struct PlaceholderLocations {
    pieces: HashMap<u64, Vec<ImageLocation>>,
    previous: Vec<PlacedRun>,
    current: Vec<PlacedRun>,
}

impl PlaceholderLocations {
    pub fn get(&self, placement: u64) -> Option<&[ImageLocation]> {
        self.pieces.get(&placement).map(Vec::as_slice)
    }

    pub fn clear(&mut self) {
        self.pieces.clear();
        self.previous.clear();
        self.current.clear();
    }

    pub fn collect(
        &mut self,
        frame: &PresentationFrame<'_>,
        cell: PixelSize,
        stamp: GraphicsStamp,
        parents: &mut GraphicsPlaceholderCache,
    ) -> Result<(), StorageError> {
        let Self {
            pieces,
            previous,
            current,
        } = self;
        pieces.values_mut().for_each(Vec::clear);
        previous.clear();
        current.clear();
        let mut row = -1;
        let mut next_previous = 0;
        let mut allocation_error = None;
        let append = |run: GraphicsPlaceholderRun| {
            if allocation_error.is_some() {
                return;
            }
            let [x, y] = run.position;
            if y != row {
                std::mem::swap(previous, current);
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
            if current.len() == current.capacity()
                && let Err(error) = current.try_reserve(1)
            {
                allocation_error = Some(error);
                return;
            }
            if pieces.len() == pieces.capacity()
                && !pieces.contains_key(&run.placement)
                && let Err(error) = pieces.try_reserve(1)
            {
                allocation_error = Some(error);
                return;
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
                if piece == pieces.capacity()
                    && let Err(error) = pieces.try_reserve(1)
                {
                    allocation_error = Some(error);
                    return;
                }
                pieces.push(ImageLocation {
                    origin: run.origin,
                    clip: [x..x + i64::from(run.columns), y..y + 1],
                });
                piece
            };
            current.push(PlacedRun { run, piece });
        };
        let placements = frame.graphics_placements();
        let result = if frame.has_virtual_parents() {
            parents.visit(
                stamp.placement_revision(),
                placements,
                frame.viewport,
                frame.styles,
                cell,
                Some(append),
            )
        } else {
            parents.visit_viewport(
                stamp.placement_revision(),
                placements,
                frame.viewport,
                frame.styles,
                append,
            )
        };
        let result =
            result.and_then(|()| allocation_error.map_or(Ok(()), |error| Err(error.into())));
        if result.is_ok() {
            pieces.retain(|_, locations| !locations.is_empty());
        } else {
            self.clear();
        }
        result
    }
}
