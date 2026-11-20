use std::num::NonZeroU32;

use lune_pty::PtySize;
use lune_terminal::{CellPosition, MAX_CELLS, PixelPosition, PixelSize};

/// Physical pixel geometry measured from the terminal content origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridGeometry {
    size: PtySize,
    cell: Option<PixelSize>,
}

impl GridGeometry {
    /// Fits whole cells inside the available content area, leaving unused
    /// pixels at the right and bottom. The grid must fit terminal and PTY limits.
    pub fn fit(available: PixelSize, cell: PixelSize) -> Option<Self> {
        let columns = u16::try_from(available.width.get() / cell.width).ok()?;
        let rows = u16::try_from(available.height.get() / cell.height).ok()?;
        if columns == 0 || rows == 0 || u32::from(columns) * u32::from(rows) > MAX_CELLS as u32 {
            return None;
        }
        let pixel_width = u16::try_from(u32::from(columns) * cell.width.get()).ok()?;
        let pixel_height = u16::try_from(u32::from(rows) * cell.height.get()).ok()?;
        Some(Self {
            size: PtySize {
                rows,
                columns,
                pixel_width,
                pixel_height,
            },
            cell: Some(cell),
        })
    }

    pub(crate) fn from_pty_size(size: PtySize) -> Self {
        let cell = exact_cell_extent(size.pixel_width, size.columns)
            .zip(exact_cell_extent(size.pixel_height, size.rows))
            .map(|(width, height)| PixelSize { width, height });
        Self { size, cell }
    }

    pub fn pty_size(self) -> PtySize {
        self.size
    }

    pub fn text_area_pixels(self) -> Option<PixelSize> {
        let width = NonZeroU32::new(u32::from(self.size.pixel_width))?;
        let height = NonZeroU32::new(u32::from(self.size.pixel_height))?;
        Some(PixelSize { width, height })
    }

    /// Uniform cell dimensions when both text area extents divide evenly into the grid.
    pub fn cell_pixels(self) -> Option<PixelSize> {
        self.cell
    }

    pub(crate) fn contains(self, pixel: PixelPosition) -> bool {
        pixel.x < u32::from(self.size.pixel_width) && pixel.y < u32::from(self.size.pixel_height)
    }

    /// Maps a pixel inside the grid to its cell when uniform cell metrics are known.
    pub fn cell_at(self, pixel: PixelPosition) -> Option<CellPosition> {
        let cell = self.cell?;
        self.contains(pixel).then(|| CellPosition {
            row: (pixel.y / cell.height) as usize,
            column: (pixel.x / cell.width) as usize,
        })
    }
}

fn exact_cell_extent(pixels: u16, cells: u16) -> Option<NonZeroU32> {
    if cells == 0 || !pixels.is_multiple_of(cells) {
        return None;
    }
    NonZeroU32::new(u32::from(pixels / cells))
}
