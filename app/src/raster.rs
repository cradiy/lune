use std::num::NonZeroU32;

use lune_terminal::{PixelSize, Rgb};

use crate::{BatchRun, CursorGlyph, PaintBatch, PaintCommand, PixelRect};

/// A clipped view of writable pixels. Row indices are relative to its rectangle.
pub struct PixelRegion<'a> {
    pixels: &'a mut [Rgb],
    stride: usize,
    rect: PixelRect,
}

impl PixelRegion<'_> {
    pub fn rect(&self) -> PixelRect {
        self.rect
    }

    pub fn row_mut(&mut self, row: u32) -> Option<&mut [Rgb]> {
        if row >= self.rect.size.height.get() {
            return None;
        }
        let start = row as usize * self.stride;
        Some(&mut self.pixels[start..start + self.rect.size.width.get() as usize])
    }

    fn fill(&mut self, color: Rgb) {
        let width = self.rect.size.width.get() as usize;
        for row in self.pixels.chunks_mut(self.stride) {
            row[..width].fill(color);
        }
    }
}

pub enum RasterText<'a> {
    Run(BatchRun<'a>),
    Cursor(CursorGlyph<'a>),
}

/// Composites terminal batches into caller-owned RGB storage.
pub struct RasterSurface<'a> {
    content: &'a mut [Rgb],
    output: &'a mut [Rgb],
    size: PixelSize,
    stride: usize,
    cursor: Option<PixelRect>,
}

impl<'a> RasterSurface<'a> {
    /// Both buffers use the same row stride, measured in pixels. Submit a full
    /// batch to initialize the visible area.
    pub fn new(
        size: PixelSize,
        stride: usize,
        content: &'a mut [Rgb],
        output: &'a mut [Rgb],
    ) -> Option<Self> {
        let width = size.width.get() as usize;
        if stride < width {
            return None;
        }
        let length = (size.height.get() as usize - 1)
            .checked_mul(stride)?
            .checked_add(width)?;
        Some(Self {
            content: content.get_mut(..length)?,
            output: output.get_mut(..length)?,
            size,
            stride,
            cursor: None,
        })
    }

    pub fn pixels(&self) -> &[Rgb] {
        self.output
    }

    /// Applies a batch in order. Text callbacks can modify pixels inside the
    /// run or cursor clip.
    pub fn apply(
        &mut self,
        batch: &PaintBatch,
        mut draw_text: impl FnMut(PixelRegion<'_>, RasterText<'_>),
    ) {
        let mut cursor = None;
        for command in batch.commands() {
            match command {
                PaintCommand::Background(background) => {
                    if let Some(rect) = clip_rect(background.rect, self.size) {
                        region(self.content, self.stride, rect).fill(background.color);
                    }
                }
                PaintCommand::Text { clip, run } => {
                    if let Some(rect) = clip_rect(clip, self.size) {
                        draw_text(
                            region(self.content, self.stride, rect),
                            RasterText::Run(run),
                        );
                    }
                }
                PaintCommand::Cursor(paint) => cursor = Some(paint),
            }
        }
        let mut band: Option<PixelRect> = None;
        for background in batch.backgrounds() {
            let rect = background.rect;
            if let Some(previous) = band.as_mut()
                && previous.origin.y == rect.origin.y
                && previous.size.height == rect.size.height
                && previous.origin.x + previous.size.width.get() == rect.origin.x
            {
                previous.size.width = previous
                    .size
                    .width
                    .checked_add(rect.size.width.get())
                    .expect("A row's rectangles fit within its pixel width");
            } else if let Some(previous) = band.replace(rect) {
                self.copy_content(previous);
            }
        }
        if let Some(band) = band {
            self.copy_content(band);
        }
        if let Some(cursor) = cursor {
            if let Some(previous) = self.cursor.take() {
                self.copy_content(previous);
            }
            if let Some(paint) = cursor
                && let Some(rect) = clip_rect(paint.rect, self.size)
            {
                region(self.output, self.stride, rect).fill(paint.color);
                if let Some(glyph) = paint.glyph {
                    draw_text(
                        region(self.output, self.stride, rect),
                        RasterText::Cursor(glyph),
                    );
                }
                self.cursor = Some(rect);
            }
        }
    }

    fn copy_content(&mut self, rect: PixelRect) {
        let Some(rect) = clip_rect(rect, self.size) else {
            return;
        };
        let width = rect.size.width.get() as usize;
        let height = rect.size.height.get() as usize;
        let first = rect.origin.y as usize * self.stride + rect.origin.x as usize;
        if width == self.stride {
            let range = first..first + width * height;
            self.output[range.clone()].copy_from_slice(&self.content[range]);
            return;
        }
        for offset in 0..height {
            let start = first + offset * self.stride;
            let row = start..start + width;
            self.output[row.clone()].copy_from_slice(&self.content[row]);
        }
    }
}

fn clip_rect(rect: PixelRect, size: PixelSize) -> Option<PixelRect> {
    let width = size.width.get().checked_sub(rect.origin.x)?;
    let height = size.height.get().checked_sub(rect.origin.y)?;
    Some(PixelRect {
        origin: rect.origin,
        size: PixelSize {
            width: rect.size.width.min(NonZeroU32::new(width)?),
            height: rect.size.height.min(NonZeroU32::new(height)?),
        },
    })
}

fn region(pixels: &mut [Rgb], stride: usize, rect: PixelRect) -> PixelRegion<'_> {
    let start = rect.origin.y as usize * stride + rect.origin.x as usize;
    let length = (rect.size.height.get() as usize - 1) * stride + rect.size.width.get() as usize;
    PixelRegion {
        pixels: &mut pixels[start..start + length],
        stride,
        rect,
    }
}
