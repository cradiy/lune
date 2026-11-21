use std::collections::TryReserveError;
use std::num::NonZeroU32;
use std::ops::Range;

use lune_terminal::{Cell, CellText, DisplayUnit, MAX_CELL_TEXT_BYTES, MAX_DIMENSION, Rgb, Style};

use super::{
    BackgroundRect, CursorGlyph, CursorPaint, PaintCommand, PaintError, PresentationDamage,
};
use crate::{PixelRect, PresentationFrame, PresentationRun, PresentationStyle};

/// Owns drawing commands and their text after the source frame is released.
#[derive(Default)]
pub struct PaintBatch {
    commands: Vec<StoredCommand>,
    units: Vec<StoredUnit>,
    text: String,
}

pub struct BatchRun<'a> {
    columns: Range<usize>,
    units: &'a [StoredUnit],
    text: &'a str,
    pub style: PresentationStyle<'a>,
}

impl<'a> BatchRun<'a> {
    #[inline]
    pub fn columns(&self) -> Range<usize> {
        self.columns.clone()
    }

    #[inline]
    pub fn units(&self) -> impl ExactSizeIterator<Item = DisplayUnit<'a>> + use<'a> {
        let text = self.text;
        self.units.iter().map(move |unit| unit.view(text))
    }
}

impl PaintBatch {
    /// Replaces the batch and reuses its buffers. An error leaves it empty.
    pub fn capture(
        &mut self,
        frame: &PresentationFrame<'_>,
        damage: PresentationDamage<'_>,
        cursor_thickness: NonZeroU32,
    ) -> Result<(), PaintError<TryReserveError>> {
        self.clear();
        let result =
            frame.visit_paint_commands(damage, cursor_thickness, |command| self.store(command));
        if result.is_err() {
            self.clear();
        }
        result
    }

    #[inline]
    pub fn commands(&self) -> impl ExactSizeIterator<Item = PaintCommand<'_, BatchRun<'_>>> {
        self.commands.iter().map(|command| match command {
            StoredCommand::Background(rect) => PaintCommand::Background(*rect),
            StoredCommand::Text {
                clip,
                columns,
                units,
                style,
            } => PaintCommand::Text {
                clip: *clip,
                run: BatchRun {
                    columns: columns.clone(),
                    units: &self.units[units.clone()],
                    text: &self.text,
                    style: style.view(),
                },
            },
            StoredCommand::Cursor(cursor) => {
                PaintCommand::Cursor(cursor.as_ref().map(|cursor| CursorPaint {
                    rect: cursor.rect,
                    color: cursor.color,
                    glyph: cursor.glyph.as_ref().map(|glyph| CursorGlyph {
                        unit: glyph.unit.view(&self.text),
                        color: glyph.color,
                        style: &glyph.style,
                    }),
                }))
            }
        })
    }

    pub(crate) fn backgrounds(&self) -> impl Iterator<Item = BackgroundRect> + '_ {
        self.commands.iter().filter_map(|command| match command {
            StoredCommand::Background(rect) => Some(*rect),
            _ => None,
        })
    }

    fn clear(&mut self) {
        self.commands.clear();
        self.units.clear();
        self.text.clear();
    }

    fn store(
        &mut self,
        command: PaintCommand<'_, PresentationRun<'_>>,
    ) -> Result<(), TryReserveError> {
        self.commands.try_reserve(1)?;
        let command = match command {
            PaintCommand::Background(rect) => StoredCommand::Background(rect),
            PaintCommand::Text { clip, run } => {
                let first = self.units.len();
                let columns = run.columns();
                self.units.try_reserve(columns.len())?;
                for unit in run.units() {
                    let unit = StoredUnit::store(unit, &mut self.text)?;
                    self.units.push(unit);
                }
                StoredCommand::Text {
                    clip,
                    columns,
                    units: first..self.units.len(),
                    style: StoredStyle {
                        foreground: run.style.foreground,
                        background: run.style.background,
                        underline_color: run.style.underline_color,
                        style: *run.style.style,
                    },
                }
            }
            PaintCommand::Cursor(cursor) => {
                let cursor = match cursor {
                    Some(cursor) => {
                        let glyph = match cursor.glyph {
                            Some(glyph) => Some(StoredCursorGlyph {
                                unit: StoredUnit::store(glyph.unit, &mut self.text)?,
                                color: glyph.color,
                                style: *glyph.style,
                            }),
                            None => None,
                        };
                        Some(StoredCursor {
                            rect: cursor.rect,
                            color: cursor.color,
                            glyph,
                        })
                    }
                    None => None,
                };
                StoredCommand::Cursor(cursor)
            }
        };
        self.commands.push(command);
        Ok(())
    }
}

enum StoredCommand {
    Background(BackgroundRect),
    Text {
        clip: PixelRect,
        columns: Range<usize>,
        units: Range<usize>,
        style: StoredStyle,
    },
    Cursor(Option<StoredCursor>),
}

struct StoredStyle {
    foreground: Rgb,
    background: Rgb,
    underline_color: Rgb,
    style: Style,
}

impl StoredStyle {
    #[inline]
    fn view(&self) -> PresentationStyle<'_> {
        PresentationStyle {
            foreground: self.foreground,
            background: self.background,
            underline_color: self.underline_color,
            style: &self.style,
        }
    }
}

struct StoredCursor {
    rect: PixelRect,
    color: Rgb,
    glyph: Option<StoredCursorGlyph>,
}

struct StoredCursorGlyph {
    unit: StoredUnit,
    color: Rgb,
    style: Style,
}

struct StoredUnit {
    cell: Cell,
    text_start: usize,
    column: u16,
    text_len: u16,
}

const _: () = assert!(MAX_CELL_TEXT_BYTES <= u16::MAX as usize);
const _: () = assert!(MAX_DIMENSION <= u16::MAX as usize);

impl StoredUnit {
    fn store(unit: DisplayUnit<'_>, arena: &mut String) -> Result<Self, TryReserveError> {
        let text_start = arena.len();
        let text_len = if let CellText::Extended(text) = unit.text {
            if arena.capacity() - text_start < text.len() {
                arena.try_reserve(text.len())?;
            }
            arena.push_str(text);
            text.len() as u16
        } else {
            0
        };
        Ok(Self {
            cell: unit.cell,
            text_start,
            column: unit.column as u16,
            text_len,
        })
    }

    #[inline]
    fn view<'a>(&self, arena: &'a str) -> DisplayUnit<'a> {
        let text = if self.text_len == 0 {
            CellText::Scalar(self.cell.character().expect("A leading cell has a scalar"))
        } else {
            let end = self.text_start + usize::from(self.text_len);
            CellText::Extended(&arena[self.text_start..end])
        };
        DisplayUnit {
            column: usize::from(self.column),
            cell: self.cell,
            text,
        }
    }
}
