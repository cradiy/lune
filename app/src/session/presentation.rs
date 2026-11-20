use std::collections::TryReserveError;
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use lune_terminal::{
    CellPosition, CellText, CursorAppearance, CursorShape, DisplayUnit, Palette, PaletteEntry, Rgb,
    Row, RowRevision, Style, StyleError,
};

use crate::{GridGeometry, PixelRect};

use super::{BackgroundRect, PresentationFrame, PresentationRun};

mod batch;

pub use batch::{BatchRun, PaintBatch};

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
    pub displayed_cursor: Option<PresentedCursor>,
}

pub enum PaintCommand<'a, Run> {
    Background(BackgroundRect),
    Text {
        clip: PixelRect,
        run: Run,
    },
    /// Replaces the cursor layer. An empty value clears it.
    Cursor(Option<CursorPaint<'a>>),
}

#[derive(Debug)]
pub enum PaintError<E> {
    GeometryUnavailable,
    InvalidRow(usize),
    Style(StyleError),
    Sink(E),
}

impl<'a> PresentationFrame<'a> {
    /// Visits the changed row content, then replaces the cursor layer if needed.
    /// An interrupted visit can replay the same frame and damage.
    pub fn visit_paint_commands<E>(
        &self,
        damage: PresentationDamage<'_>,
        cursor_thickness: NonZeroU32,
        mut emit: impl FnMut(PaintCommand<'a, PresentationRun<'a>>) -> Result<(), E>,
    ) -> Result<(), PaintError<E>> {
        if self.geometry.cell_pixels().is_none() {
            return Err(PaintError::GeometryUnavailable);
        }
        let size = self.geometry.pty_size();
        let width = usize::from(size.columns);
        let mut replace_cursor = damage.cursor.is_some();
        let mut paint_row = |row| {
            let source = self
                .viewport
                .row(row)
                .filter(|_| row < usize::from(size.rows))
                .ok_or(PaintError::InvalidRow(row))?;
            let backgrounds = self
                .row_backgrounds(row, 0..width)
                .expect("The row has pixel geometry");
            for background in backgrounds {
                let background = background.map_err(PaintError::Style)?;
                emit(PaintCommand::Background(background)).map_err(PaintError::Sink)?;
            }
            let runs = self
                .row_runs(row, 0..source.cells().len())
                .expect("The row is in the viewport");
            for run in runs {
                let run = run.map_err(PaintError::Style)?;
                if !run.style.style.hidden {
                    let clip = self
                        .geometry
                        .row_rect(row, run.columns())
                        .expect("The span clips to visible columns");
                    emit(PaintCommand::Text { clip, run }).map_err(PaintError::Sink)?;
                }
            }
            replace_cursor |= damage
                .displayed_cursor
                .is_some_and(|cursor| cursor.position.row == row);
            Ok(())
        };

        match damage.rows {
            RowDamage::All => {
                for row in 0..self.viewport.rows() {
                    paint_row(row)?;
                }
                replace_cursor = true;
            }
            RowDamage::Rows(rows) => {
                for &row in rows {
                    paint_row(row)?;
                }
            }
        }
        if replace_cursor {
            let cursor = damage
                .displayed_cursor
                .map(|cursor| self.cursor_paint(cursor, cursor_thickness))
                .transpose()
                .map_err(PaintError::Style)?
                .flatten();
            emit(PaintCommand::Cursor(cursor)).map_err(PaintError::Sink)?;
        }
        Ok(())
    }
}

/// Compares successive frames from one session in viewport coordinates.
#[derive(Default)]
pub struct PresentationTracker {
    committed_geometry: Option<GridGeometry>,
    revisions: Vec<RowRevision>,
    changed_rows: Vec<usize>,
    palette: Option<Palette>,
    reverse_video: bool,
    cursor: CursorOverlay,
}

/// A staged frame update. Dropping it before commit forces a full redraw.
#[must_use = "commit the presentation after submitting it"]
pub struct PreparedPresentation<'a> {
    tracker: &'a mut PresentationTracker,
    geometry: GridGeometry,
    all_rows: bool,
    cursor_change: Option<CursorChange>,
    cursor: Option<CursorOverlay>,
}

impl PreparedPresentation<'_> {
    #[inline]
    pub fn damage(&self) -> PresentationDamage<'_> {
        PresentationDamage {
            rows: if self.all_rows {
                RowDamage::All
            } else {
                RowDamage::Rows(&self.tracker.changed_rows)
            },
            cursor: self.cursor_change,
            displayed_cursor: self
                .cursor
                .as_ref()
                .unwrap_or(&self.tracker.cursor)
                .displayed(),
        }
    }

    /// Acknowledges a successful submission and retains its comparison baseline.
    #[inline]
    pub fn commit(self) {
        self.tracker.committed_geometry = Some(self.geometry);
        if let Some(cursor) = self.cursor {
            self.tracker.cursor = cursor;
        }
    }
}

impl PresentationTracker {
    /// Forces a full redraw and resets the cursor overlay after a surface reset.
    pub fn invalidate(&mut self) {
        self.committed_geometry = None;
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
        self.cursor.tick(now)
    }

    /// Stages damage and cursor state until the renderer confirms submission.
    pub fn prepare(
        &mut self,
        frame: &PresentationFrame<'_>,
        now: Instant,
    ) -> Result<PreparedPresentation<'_>, TryReserveError> {
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
        // Row cache entries can be reused only after the staged frame commits.
        let committed_geometry = self.committed_geometry.take();
        let all_rows = height_changed
            || committed_geometry != Some(frame.geometry)
            || colors_changed
            || self.reverse_video != frame.reverse_video;
        self.changed_rows.clear();
        if all_rows {
            self.revisions.clear();
            self.revisions
                .extend(frame.viewport.iter_rows().map(Row::revision));
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
        let source_changed = self.cursor.source != source;
        let (cursor, cursor_change) =
            if source_changed || self.cursor.deadline.is_some_and(|deadline| now >= deadline) {
                let mut cursor = self.cursor;
                let change = if source_changed {
                    let before = cursor.displayed();
                    cursor.source = source;
                    cursor.restart(now);
                    CursorChange::between(before, source)
                } else {
                    cursor.tick(now)
                };
                (Some(cursor), change)
            } else {
                (None, None)
            };
        Ok(PreparedPresentation {
            tracker: self,
            geometry: frame.geometry,
            all_rows,
            cursor_change,
            cursor,
        })
    }
}

#[derive(Clone, Copy, Default)]
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

    fn tick(&mut self, now: Instant) -> Option<CursorChange> {
        if !self.advance(now) {
            return None;
        }
        let (before, after) = if self.hidden {
            (self.source, None)
        } else {
            (None, self.source)
        };
        Some(CursorChange { before, after })
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
