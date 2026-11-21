use std::num::NonZeroU32;

use lune_terminal::{PixelSize, Rgb};

use crate::{BatchRun, CursorGlyph, PaintBatch, PaintCommand, PixelRect};

/// An 8-bit coverage mask with a byte stride.
#[derive(Clone, Copy)]
pub struct CoverageMask<'a> {
    bytes: &'a [u8],
    size: PixelSize,
    stride: usize,
}

impl<'a> CoverageMask<'a> {
    pub fn new(size: PixelSize, stride: usize, bytes: &'a [u8]) -> Option<Self> {
        Some(Self {
            bytes: bytes.get(..buffer_len(size, stride)?)?,
            size,
            stride,
        })
    }
}

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

    /// Blends a coverage mask at a signed origin in surface pixels.
    pub fn blend_mask(&mut self, origin: (i32, i32), mask: CoverageMask<'_>, color: Rgb) {
        let (mask_x, mask_y) = (i64::from(origin.0), i64::from(origin.1));
        let (region_x, region_y) = (i64::from(self.rect.origin.x), i64::from(self.rect.origin.y));
        let left = mask_x.max(region_x);
        let top = mask_y.max(region_y);
        let right = (mask_x + i64::from(mask.size.width.get()))
            .min(region_x + i64::from(self.rect.size.width.get()));
        let bottom = (mask_y + i64::from(mask.size.height.get()))
            .min(region_y + i64::from(self.rect.size.height.get()));
        if left >= right || top >= bottom {
            return;
        }

        let width = (right - left) as usize;
        let dest_x = (left - region_x) as usize;
        let dest_y = (top - region_y) as usize;
        let source_x = (left - mask_x) as usize;
        let source_y = (top - mask_y) as usize;
        for row in 0..(bottom - top) as usize {
            let dest = (dest_y + row) * self.stride + dest_x;
            let source = (source_y + row) * mask.stride + source_x;
            for (pixel, &coverage) in self.pixels[dest..dest + width]
                .iter_mut()
                .zip(&mask.bytes[source..source + width])
            {
                match coverage {
                    0 => {}
                    255 => *pixel = color,
                    alpha => {
                        *pixel = Rgb {
                            red: blend_channel(pixel.red, color.red, alpha),
                            green: blend_channel(pixel.green, color.green, alpha),
                            blue: blend_channel(pixel.blue, color.blue, alpha),
                        };
                    }
                }
            }
        }
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
        let length = buffer_len(size, stride)?;
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

fn buffer_len(size: PixelSize, stride: usize) -> Option<usize> {
    let width = size.width.get() as usize;
    if stride < width {
        return None;
    }
    (size.height.get() as usize - 1)
        .checked_mul(stride)?
        .checked_add(width)
}

#[inline]
fn blend_channel(background: u8, foreground: u8, coverage: u8) -> u8 {
    let alpha = u16::from(coverage);
    ((u16::from(background) * (255 - alpha) + u16::from(foreground) * alpha + 127) / 255) as u8
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

#[cfg(test)]
mod tests {
    use super::*;
    use lune_terminal::PixelPosition;

    #[test]
    fn coverage_blends_only_the_intersection_of_strided_regions() {
        let size = |width, height| PixelSize {
            width: NonZeroU32::new(width).unwrap(),
            height: NonZeroU32::new(height).unwrap(),
        };
        let rgb = |red, green, blue| Rgb { red, green, blue };
        let foreground = rgb(255, 0, 11);
        let background = rgb(0, 255, 10);
        let mut pixels = [background; 28];
        let mut coverage = [241; 43];
        coverage[21..24].copy_from_slice(&[0, 128, 255]);
        coverage[30..33].copy_from_slice(&[255, 1, 254]);
        assert!(CoverageMask::new(size(7, 5), 6, &coverage).is_none());
        assert!(CoverageMask::new(size(7, 5), 9, &coverage[..42]).is_none());
        let mask = CoverageMask::new(size(7, 5), 9, &coverage).unwrap();
        let mut region = region(
            &mut pixels,
            7,
            PixelRect {
                origin: PixelPosition { x: 2, y: 1 },
                size: size(3, 2),
            },
        );
        region.blend_mask((-1, -1), mask, foreground);
        region.blend_mask((i32::MAX, i32::MIN), mask, foreground);

        let mut expected = [background; 28];
        expected[9..12].copy_from_slice(&[background, rgb(128, 127, 11), foreground]);
        expected[16..19].copy_from_slice(&[foreground, rgb(1, 254, 10), rgb(254, 1, 11)]);
        assert_eq!(pixels, expected);
    }
}
