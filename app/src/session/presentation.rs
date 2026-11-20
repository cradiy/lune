use std::collections::TryReserveError;
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use lune_terminal::{
    CellPosition, CellText, CursorAppearance, CursorShape, DisplayUnit, Palette, PaletteEntry, Rgb,
    Row, RowRevision, Style, StyleError,
};

use crate::{GridGeometry, PixelRect};

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
pub struct CursorPaint<'a> {
    pub rect: PixelRect,
    pub color: Rgb,
    pub glyph: Option<CursorGlyph<'a>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorGlyph<'a> {
    pub unit: DisplayUnit<'a>,
    pub color: Rgb,
    pub style: &'a Style,
}

impl<'a> PresentationFrame<'a> {
    /// Resolves a displayed cursor against this frame. Draw its glyph at the
    /// block origin and clip it to the rectangle.
    pub fn cursor_paint(
        &self,
        cursor: PresentedCursor,
        thickness: NonZeroU32,
    ) -> Result<Option<CursorPaint<'a>>, StyleError> {
        let Some(cell) = self.geometry.cell_pixels() else {
            return Ok(None);
        };
        let CellPosition { row, column } = cursor.position;
        if !cursor.appearance.visible || column >= usize::from(self.geometry.pty_size().columns) {
            return Ok(None);
        }
        let Some(source) = self.viewport.row(row) else {
            return Ok(None);
        };
        let cells = source.cells();
        let Some(covered) = cells.get(column) else {
            return Ok(None);
        };
        let leading = column - usize::from(covered.is_continuation());
        let covered = cells[leading];
        let Some(mut rect) = self
            .geometry
            .row_rect(row, leading..leading + covered.width())
        else {
            return Ok(None);
        };

        let mut glyph = None;
        match cursor.appearance.shape {
            CursorShape::Block if !covered.is_erased() => {
                let id = covered.style();
                let style = self.resolve_style(id).ok_or(StyleError::InvalidId(id))?;
                if !style.style.hidden {
                    let color = cursor_text_color(style.background, style.foreground, cursor.color);
                    let text = if covered.has_extended_text() {
                        source
                            .text(leading)
                            .expect("The leading column is in the row")
                    } else {
                        CellText::Scalar(covered.character().expect("A leading cell has a scalar"))
                    };
                    glyph = Some(CursorGlyph {
                        unit: DisplayUnit {
                            column: leading,
                            cell: covered,
                            text,
                        },
                        color,
                        style: style.style,
                    });
                }
            }
            CursorShape::Block => {}
            CursorShape::Bar => rect.size.width = thickness.min(cell.width),
            CursorShape::Underline => {
                let height = thickness.min(cell.height);
                rect.origin.y += cell.height.get() - height.get();
                rect.size.height = height;
            }
        }
        Ok(Some(CursorPaint {
            rect,
            color: cursor.color,
            glyph,
        }))
    }
}

fn cursor_text_color(background: Rgb, foreground: Rgb, cursor: Rgb) -> Rgb {
    if background != cursor {
        background
    } else if foreground != cursor {
        foreground
    } else {
        let brightness = 54 * u32::from(cursor.red)
            + 183 * u32::from(cursor.green)
            + 19 * u32::from(cursor.blue);
        let channel = if brightness >= 128 * 256 { 0 } else { 255 };
        Rgb {
            red: channel,
            green: channel,
            blue: channel,
        }
    }
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
