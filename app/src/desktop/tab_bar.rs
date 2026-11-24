use std::num::NonZeroU32;

use lune_terminal::{Palette, PixelPosition, PixelSize};
use lune_text::WidthPolicy;
use unicode_segmentation::UnicodeSegmentation;
use winit::dpi::{PhysicalPosition, PhysicalSize};

use crate::PixelRect;

use super::Tab;

const FADE_CELLS: u32 = 3;
const PADDING_CELLS: u32 = 1;
const GAP_CELLS: u32 = 1;
const MIN_TAB_CELLS: u32 = 2 * (FADE_CELLS + PADDING_CELLS) + 1;

#[derive(Default)]
pub(super) struct Layout {
    pub(super) top: u32,
    pub(super) height: u32,
    width: u32,
    cell_width: u32,
    first: usize,
    slots: Vec<Slot>,
}

struct Slot {
    index: usize,
    start: u32,
    title_cells: u32,
    title_end: usize,
}

impl Layout {
    pub(super) fn height(size: PhysicalSize<u32>, cell: PixelSize, tabs: usize) -> u32 {
        let height = cell.height.get();
        if tabs >= 2 && size.height / 2 >= height && size.width / cell.width >= MIN_TAB_CELLS {
            height
        } else {
            0
        }
    }

    pub(super) fn reflow<'a>(
        &mut self,
        size: PhysicalSize<u32>,
        cell: PixelSize,
        active: usize,
        tabs: usize,
        title: impl Fn(usize) -> &'a str,
    ) {
        self.height = Self::height(size, cell, tabs);
        self.width = size.width;
        self.top = size.height - self.height;
        self.cell_width = cell.width.get();
        self.slots.clear();
        if self.height == 0 {
            self.first = 0;
            return;
        }

        let columns = size.width / self.cell_width;
        let visible = (columns / (MIN_TAB_CELLS + GAP_CELLS))
            .max(1)
            .min(tabs as u32);
        let max_title = (columns / visible)
            .saturating_sub(MIN_TAB_CELLS - 1 + GAP_CELLS)
            .max(1);
        self.first = self
            .first
            .min(active)
            .max((active + 1).saturating_sub(visible as usize))
            .min(tabs - visible as usize);
        let mut column = 0;
        for index in self.first..self.first + visible as usize {
            let mut title_cells = 0;
            let mut title_end = 0;
            for (offset, grapheme) in title(index).grapheme_indices(true) {
                let cells = WidthPolicy::Narrow.columns(grapheme) as u32;
                if title_cells + cells > max_title {
                    break;
                }
                title_cells += cells;
                title_end = offset + grapheme.len();
            }
            let title_cells = title_cells.max(1);
            self.slots.push(Slot {
                index,
                start: column,
                title_cells,
                title_end,
            });
            column += title_cells + 2 * (FADE_CELLS + PADDING_CELLS) + GAP_CELLS;
        }
    }

    pub(super) fn area(&self) -> Option<PixelSize> {
        Some(PixelSize {
            width: NonZeroU32::new(self.width)?,
            height: NonZeroU32::new(self.height)?,
        })
    }

    pub(super) fn tabs(&self) -> impl Iterator<Item = (usize, PixelRect, PixelRect, usize)> + '_ {
        self.slots.iter().map(|tab| {
            let center = tab.start + FADE_CELLS;
            let fill = self.rect(center, tab.title_cells + 2 * PADDING_CELLS);
            let title = self.rect(center + PADDING_CELLS, tab.title_cells);
            (tab.index, fill, title, tab.title_end)
        })
    }

    pub(super) fn fade_cell_width(&self) -> NonZeroU32 {
        NonZeroU32::new(self.cell_width).unwrap()
    }

    fn rect(&self, start: u32, cells: u32) -> PixelRect {
        PixelRect {
            origin: PixelPosition {
                x: start * self.cell_width,
                y: 0,
            },
            size: PixelSize {
                width: NonZeroU32::new(cells * self.cell_width).unwrap(),
                height: NonZeroU32::new(self.height).unwrap(),
            },
        }
    }

    pub(super) fn contains(&self, position: PhysicalPosition<f64>) -> bool {
        position.y >= f64::from(self.top)
            && position.y < f64::from(self.top + self.height)
            && position.x >= 0.0
            && position.x < f64::from(self.width)
    }

    pub(super) fn hit(&self, position: PhysicalPosition<f64>) -> Option<usize> {
        if !self.contains(position) {
            return None;
        }
        let column = position.x as u32 / self.cell_width;
        self.slots
            .iter()
            .find(|tab| {
                column >= tab.start
                    && column < tab.start + tab.title_cells + 2 * (FADE_CELLS + PADDING_CELLS)
            })
            .map(|tab| tab.index)
    }
}

pub(super) struct View<'a> {
    pub(super) layout: &'a Layout,
    pub(super) tabs: &'a [Tab],
    pub(super) active: usize,
    pub(super) palette: &'a Palette,
}
