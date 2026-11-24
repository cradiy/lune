//! IME preedit state, display-width cell layout, and cursor-area caching.

use std::num::NonZeroU32;

use lune_terminal::{CellPosition, PixelPosition, PixelSize};
use lune_text::WidthPolicy;
use unicode_segmentation::UnicodeSegmentation;

use crate::PixelRect;

const MAX_PREEDIT_BYTES: usize = 1024;

/// Active IME preedit text and optional byte-offset selection/caret range.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Preedit {
    pub(super) text: String,
    pub(super) cursor: Option<(usize, usize)>,
}

/// One positioned grapheme cell inside an inline terminal preedit overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PreeditCell<'a> {
    pub(super) position: CellPosition,
    pub(super) width: usize,
    pub(super) text: &'a str,
    pub(super) selected: bool,
}

impl Preedit {
    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn clear(&mut self) -> bool {
        if self.text.is_empty() && self.cursor.is_none() {
            return false;
        }
        self.text.clear();
        self.cursor = None;
        true
    }

    pub(super) fn set(&mut self, mut text: String, cursor: Option<(usize, usize)>) -> bool {
        text.truncate(text.floor_char_boundary(MAX_PREEDIT_BYTES));
        let cursor = cursor.map(|(start, end)| {
            let start = clamp_char_boundary(&text, start);
            let end = clamp_char_boundary(&text, end);
            (start, end)
        });
        if self.text == text && self.cursor == cursor {
            return false;
        }
        self.text = text;
        self.cursor = cursor;
        true
    }

    /// Computes the viewport cell of the preedit caret, wrapping across rows
    /// from `origin`. When `cursor` is `None`, anchors at the end of `text`.
    pub(super) fn cursor_cell(
        &self,
        origin: CellPosition,
        columns: usize,
        rows: usize,
    ) -> Option<CellPosition> {
        if self.text.is_empty() || columns == 0 || rows == 0 {
            return None;
        }
        let target_byte = self.cursor.map_or(self.text.len(), |(_, end)| end);
        let mut col = origin.column.min(columns - 1);
        let mut row = origin.row.min(rows - 1);

        for (byte_start, _, width) in graphemes_with_widths(&self.text) {
            if byte_start >= target_byte {
                break;
            }
            if width == 0 {
                continue;
            }
            let width = width.min(columns);
            if col.saturating_add(width) > columns {
                col = 0;
                row = row.saturating_add(1);
            }
            if row >= rows {
                return Some(CellPosition {
                    row: rows - 1,
                    column: columns - 1,
                });
            }
            col = col.saturating_add(width);
            if col >= columns {
                col = 0;
                row = row.saturating_add(1);
            }
        }

        Some(CellPosition {
            row: row.min(rows - 1),
            column: col.min(columns - 1),
        })
    }

    /// Lays out preedit graphemes into wrapped viewport cells starting at `origin`.
    pub(super) fn layout_cells(
        &self,
        origin: CellPosition,
        columns: usize,
        rows: usize,
    ) -> Vec<PreeditCell<'_>> {
        if self.text.is_empty() || columns == 0 || rows == 0 {
            return Vec::new();
        }
        let selection = self
            .cursor
            .map(|(start, end)| (start.min(end), start.max(end)))
            .filter(|(start, end)| start != end);

        let mut cells = Vec::new();
        let mut col = origin.column.min(columns - 1);
        let mut row = origin.row.min(rows - 1);

        for (byte_start, byte_end, width) in graphemes_with_widths(&self.text) {
            if width == 0 {
                continue;
            }
            let width = width.min(columns);
            if col.saturating_add(width) > columns {
                col = 0;
                row = row.saturating_add(1);
            }
            if row >= rows {
                break;
            }
            let selected =
                selection.is_some_and(|(start, end)| byte_start < end && byte_end > start);
            cells.push(PreeditCell {
                position: CellPosition { row, column: col },
                width,
                text: &self.text[byte_start..byte_end],
                selected,
            });
            col = col.saturating_add(width);
            if col >= columns {
                col = 0;
                row = row.saturating_add(1);
            }
        }
        cells
    }
}

/// Computes the total terminal display-cell width of `text`.
pub(super) fn display_width(text: &str) -> usize {
    graphemes_with_widths(text)
        .into_iter()
        .map(|(_, _, width)| width)
        .sum()
}

/// Deduplicates `set_ime_cursor_area` updates across frames while respecting
/// display scale changes.
#[derive(Debug, Default)]
pub(super) struct ImeAreaCache {
    last: Option<(PixelPosition, PixelSize, u64)>,
}

impl ImeAreaCache {
    pub(super) fn record_if_changed(
        &mut self,
        origin: PixelPosition,
        size: PixelSize,
        scale_factor: f64,
    ) -> bool {
        let next = (origin, size, scale_factor.to_bits());
        if self.last == Some(next) {
            return false;
        }
        self.last = Some(next);
        true
    }

    pub(super) fn invalidate(&mut self) {
        self.last = None;
    }
}

/// Clamps an IME cursor rectangle in physical pixels to stay strictly inside
/// the window surface dimensions.
pub(super) fn clamp_to_surface(
    origin: PixelPosition,
    size: PixelSize,
    surface_width: u32,
    surface_height: u32,
) -> Option<PixelRect> {
    if surface_width == 0 || surface_height == 0 {
        return None;
    }
    let width = size.width.get().min(surface_width);
    let height = size.height.get().min(surface_height);
    let x = origin.x.min(surface_width - width);
    let y = origin.y.min(surface_height - height);
    Some(PixelRect {
        origin: PixelPosition { x, y },
        size: PixelSize {
            width: NonZeroU32::new(width)?,
            height: NonZeroU32::new(height)?,
        },
    })
}

fn clamp_char_boundary(text: &str, byte: usize) -> usize {
    text.floor_char_boundary(byte.min(text.len()))
}

fn graphemes_with_widths(text: &str) -> Vec<(usize, usize, usize)> {
    text.grapheme_indices(true)
        .filter(|(_, grapheme)| !grapheme.chars().all(char::is_control))
        .map(|(start, grapheme)| {
            let width = WidthPolicy::Narrow.columns(grapheme).min(2);
            (start, start + grapheme.len(), width)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_cell_advances_across_ascii_and_wide_preedit_with_wrapping() {
        let mut preedit = Preedit::default();
        assert!(preedit.set("你好ab".into(), Some((8, 8))));
        // Origin at col 4 in a 6-column grid:
        // '你' -> (0, 4..6), wraps to (1, 0)
        // '好' -> (1, 0..2)
        // 'a'  -> (1, 2..3)
        // 'b'  -> (1, 3..4)
        // Caret ends at (1, 4).
        let caret = preedit
            .cursor_cell(CellPosition { row: 0, column: 4 }, 6, 4)
            .unwrap();
        assert_eq!(caret, CellPosition { row: 1, column: 4 });

        // Mid-preedit cursor after "你" (byte 3) lands at (1, 0).
        assert!(preedit.set("你好ab".into(), Some((3, 3))));
        let mid = preedit
            .cursor_cell(CellPosition { row: 0, column: 4 }, 6, 4)
            .unwrap();
        assert_eq!(mid, CellPosition { row: 1, column: 0 });
    }

    #[test]
    fn layout_cells_marks_selected_subrange_and_respects_bounds() {
        let mut preedit = Preedit::default();
        assert!(preedit.set("ni hao".into(), Some((3, 6))));
        let cells = preedit.layout_cells(CellPosition { row: 2, column: 1 }, 10, 4);
        assert_eq!(cells.len(), 6);
        assert!(!cells[0].selected);
        assert!(!cells[2].selected);
        assert!(cells[3].selected);
        assert!(cells[5].selected);
        assert_eq!(cells[5].position, CellPosition { row: 2, column: 6 });
    }

    #[test]
    fn cache_invalidates_on_scale_change_and_clamps_to_surface() {
        let mut cache = ImeAreaCache::default();
        let origin = PixelPosition { x: 100, y: 200 };
        let size = PixelSize {
            width: NonZeroU32::new(10).unwrap(),
            height: NonZeroU32::new(20).unwrap(),
        };
        assert!(cache.record_if_changed(origin, size, 1.0));
        assert!(!cache.record_if_changed(origin, size, 1.0));
        assert!(cache.record_if_changed(origin, size, 1.5));
        cache.invalidate();
        assert!(cache.record_if_changed(origin, size, 1.5));

        let clamped = clamp_to_surface(
            PixelPosition { x: 995, y: 590 },
            PixelSize {
                width: NonZeroU32::new(10).unwrap(),
                height: NonZeroU32::new(20).unwrap(),
            },
            1000,
            600,
        )
        .unwrap();
        assert_eq!(clamped.origin, PixelPosition { x: 990, y: 580 });
    }
}
