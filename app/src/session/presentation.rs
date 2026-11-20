use std::collections::TryReserveError;
use std::time::{Duration, Instant};

use lune_terminal::{CellPosition, CursorAppearance, Palette, PaletteEntry, Rgb, Row, RowRevision};

use crate::GridGeometry;

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

impl CursorChange {
    fn between(before: Option<PresentedCursor>, after: Option<PresentedCursor>) -> Option<Self> {
        (before != after).then_some(Self { before, after })
    }
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
    geometry: Option<GridGeometry>,
    revisions: Vec<RowRevision>,
    changed_rows: Vec<usize>,
    palette: Option<Palette>,
    reverse_video: bool,
    cursor: CursorOverlay,
}

impl PresentationTracker {
    /// Forces a full redraw on the next update after a surface reset or failed draw.
    pub fn invalidate(&mut self) {
        self.geometry = None;
        self.cursor.source = None;
        self.cursor.deadline = None;
    }

    /// Sets the duration of each blink phase. Zero displays a steady cursor.
    pub fn set_cursor_blink_interval(
        &mut self,
        interval: Duration,
        now: Instant,
    ) -> Option<CursorChange> {
        if self.cursor.interval == interval {
            return None;
        }
        self.cursor.interval = interval;
        self.restart_cursor_blink(now)
    }

    /// Restores the visible phase and restarts its timer after user activity.
    pub fn restart_cursor_blink(&mut self, now: Instant) -> Option<CursorChange> {
        let before = self.cursor.displayed();
        self.cursor.restart(now);
        CursorChange::between(before, self.cursor.displayed())
    }

    pub fn cursor_deadline(&self) -> Option<Instant> {
        self.cursor.deadline
    }

    pub fn displayed_cursor(&self) -> Option<PresentedCursor> {
        self.cursor.displayed()
    }

    /// Advances the last presented cursor independently of terminal output.
    pub fn tick_cursor(&mut self, now: Instant) -> Option<CursorChange> {
        if !self.cursor.advance(now) {
            return None;
        }
        let source = self.cursor.source;
        let (before, after) = if self.cursor.hidden {
            (source, None)
        } else {
            (None, source)
        };
        Some(CursorChange { before, after })
    }

    /// Computes damage and records the frame as the next comparison baseline.
    /// Apply row damage before drawing the cursor overlay.
    pub fn update(
        &mut self,
        frame: &PresentationFrame<'_>,
        now: Instant,
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
            || self.geometry != Some(frame.geometry)
            || colors_changed
            || self.reverse_video != frame.reverse_video;
        self.changed_rows.clear();
        if all_rows {
            self.revisions.clear();
            self.revisions
                .extend(frame.viewport.iter_rows().map(Row::revision));
            self.geometry = Some(frame.geometry);
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

        let source = frame
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
        let cursor = if self.cursor.source == source {
            self.tick_cursor(now)
        } else {
            let before = self.cursor.displayed();
            self.cursor.source = source;
            self.cursor.restart(now);
            CursorChange::between(before, source)
        };
        Ok(PresentationDamage {
            rows: if all_rows {
                RowDamage::All
            } else {
                RowDamage::Rows(&self.changed_rows)
            },
            cursor,
        })
    }
}

#[derive(Default)]
struct CursorOverlay {
    source: Option<PresentedCursor>,
    interval: Duration,
    deadline: Option<Instant>,
    hidden: bool,
}

impl CursorOverlay {
    fn displayed(&self) -> Option<PresentedCursor> {
        if self.hidden { None } else { self.source }
    }

    fn restart(&mut self, now: Instant) {
        self.hidden = false;
        self.deadline = if !self.interval.is_zero()
            && self.source.is_some_and(|cursor| cursor.appearance.blinking)
        {
            now.checked_add(self.interval)
        } else {
            None
        };
    }

    fn advance(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.deadline.filter(|&deadline| now >= deadline) else {
            return false;
        };
        let overdue = now.duration_since(deadline);
        let (toggle, next) = if overdue < self.interval {
            (true, deadline.checked_add(self.interval))
        } else {
            let overdue_nanos = overdue.as_nanos();
            let interval_nanos = self.interval.as_nanos();
            let skipped = overdue_nanos / interval_nanos;
            let wait = Duration::from_nanos_u128(interval_nanos - overdue_nanos % interval_nanos);
            (skipped.is_multiple_of(2), now.checked_add(wait))
        };
        self.deadline = next;
        let was_hidden = self.hidden;
        self.hidden = next.is_some() && (was_hidden ^ toggle);
        self.hidden != was_hidden
    }
}
