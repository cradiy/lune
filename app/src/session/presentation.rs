use std::collections::TryReserveError;

use lune_terminal::{CellPosition, CursorAppearance, Palette, PaletteEntry, Rgb, Row, RowRevision};

use super::PresentationFrame;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowDamage<'a> {
    All,
    /// Changed viewport row indices in ascending order.
    Rows(&'a [usize]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentedCursor {
    pub position: CellPosition,
    pub appearance: CursorAppearance,
    pub color: Rgb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorChange {
    pub before: Option<PresentedCursor>,
    pub after: Option<PresentedCursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentationDamage<'a> {
    pub rows: RowDamage<'a>,
    /// Overlay changes. Repaint an unchanged cursor when repainting its row.
    pub cursor: Option<CursorChange>,
}

/// Compares successive frames from one session in viewport coordinates.
#[derive(Default)]
pub struct PresentationTracker {
    columns: usize,
    revisions: Vec<RowRevision>,
    changed_rows: Vec<usize>,
    palette: Option<Palette>,
    reverse_video: bool,
    cursor: Option<PresentedCursor>,
}

impl PresentationTracker {
    /// Forces a full redraw on the next update after a surface reset or failed draw.
    pub fn invalidate(&mut self) {
        self.palette = None;
        self.cursor = None;
    }

    /// Computes damage and records the frame as the next comparison baseline.
    /// Apply row damage before drawing the cursor overlay.
    pub fn update(
        &mut self,
        frame: &PresentationFrame<'_>,
    ) -> Result<PresentationDamage<'_>, TryReserveError> {
        let rows = frame.viewport.rows();
        let height_changed = self.revisions.len() != rows;
        if rows > self.revisions.len() {
            self.revisions
                .try_reserve_exact(rows - self.revisions.len())?;
            self.changed_rows
                .try_reserve_exact(rows - self.changed_rows.len())?;
        }

        let colors_changed = self
            .palette
            .as_ref()
            .is_none_or(|palette| !palette.same_text_colors(frame.palette));
        let all_rows = height_changed
            || self.columns != frame.columns
            || colors_changed
            || self.reverse_video != frame.reverse_video;
        self.changed_rows.clear();
        if all_rows {
            self.revisions.clear();
            self.revisions
                .extend(frame.viewport.iter_rows().map(Row::revision));
            self.columns = frame.columns;
            self.reverse_video = frame.reverse_video;
        } else {
            for (index, (previous, row)) in self
                .revisions
                .iter_mut()
                .zip(frame.viewport.iter_rows())
                .enumerate()
            {
                let revision = row.revision();
                if *previous != revision {
                    *previous = revision;
                    self.changed_rows.push(index);
                }
            }
        }
        if colors_changed {
            self.palette = Some(frame.palette.clone());
        }

        let before = self.cursor;
        let after = frame
            .viewport
            .cursor()
            .filter(|_| frame.cursor_appearance.visible)
            .map(|cursor| PresentedCursor {
                position: CellPosition {
                    row: cursor.row,
                    column: cursor.column,
                },
                appearance: frame.cursor_appearance,
                color: frame.palette.color(PaletteEntry::Cursor),
            });
        self.cursor = after;
        Ok(PresentationDamage {
            rows: if all_rows {
                RowDamage::All
            } else {
                RowDamage::Rows(&self.changed_rows)
            },
            cursor: (before != after).then_some(CursorChange { before, after }),
        })
    }
}
