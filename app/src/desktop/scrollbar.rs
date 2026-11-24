use std::num::NonZeroU32;

use lune_terminal::{PixelPosition, PixelSize};

use crate::PixelRect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ScrollbarMetrics {
    pub(super) track: PixelRect,
    pub(super) thumb: PixelRect,
    pub(super) grab_left: u32,
    pub(super) margin: u32,
    pub(super) travel: u32,
    pub(super) history_rows: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ScrollbarView {
    pub(super) track: PixelRect,
    pub(super) thumb: PixelRect,
    pub(super) highlighted: bool,
}

impl ScrollbarMetrics {
    pub(super) fn compute(
        area: PixelSize,
        history_rows: usize,
        viewport_rows: usize,
        viewport_offset: usize,
        scale: f64,
    ) -> Option<Self> {
        if history_rows == 0 || viewport_rows == 0 {
            return None;
        }
        let scale = scale.max(1.0);
        let margin = (2.0 * scale).round().max(1.0) as u32;
        let thumb_width = (6.0 * scale)
            .round()
            .max(4.0)
            .min(f64::from(area.width.get()) * 0.25) as u32;
        let grab_width = (12.0 * scale)
            .round()
            .max(f64::from(thumb_width) + f64::from(margin)) as u32;
        let track_height = area.height.get().saturating_sub(margin.saturating_mul(2));
        if thumb_width == 0 || track_height < 8 || area.width.get() <= thumb_width + margin {
            return None;
        }

        let total_rows = history_rows.saturating_add(viewport_rows);
        let min_thumb = ((24.0 * scale).round().max(16.0) as u32).min(track_height);
        let raw_thumb =
            ((viewport_rows as f64 / total_rows as f64) * f64::from(track_height)).round() as u32;
        let thumb_height = raw_thumb.clamp(min_thumb, track_height);
        let travel = track_height.saturating_sub(thumb_height);

        let clamped_offset = viewport_offset.min(history_rows);
        let from_top = (history_rows - clamped_offset) as f64 / history_rows as f64;
        let thumb_y = margin + (from_top * f64::from(travel)).round() as u32;
        let x = area.width.get() - margin - thumb_width;
        let grab_left = area.width.get().saturating_sub(grab_width);

        Some(Self {
            track: PixelRect {
                origin: PixelPosition { x, y: margin },
                size: PixelSize {
                    width: NonZeroU32::new(thumb_width)?,
                    height: NonZeroU32::new(track_height)?,
                },
            },
            thumb: PixelRect {
                origin: PixelPosition { x, y: thumb_y },
                size: PixelSize {
                    width: NonZeroU32::new(thumb_width)?,
                    height: NonZeroU32::new(thumb_height)?,
                },
            },
            grab_left,
            margin,
            travel,
            history_rows,
        })
    }

    pub(super) fn contains(&self, local_x: f64, local_y: f64) -> bool {
        local_x >= f64::from(self.grab_left)
            && local_y >= f64::from(self.track.origin.y)
            && local_y < f64::from(self.track.origin.y + self.track.size.height.get())
    }

    pub(super) fn thumb_contains_y(&self, local_y: f64) -> bool {
        local_y >= f64::from(self.thumb.origin.y)
            && local_y < f64::from(self.thumb.origin.y + self.thumb.size.height.get())
    }

    pub(super) fn offset_for_y(&self, local_y: f64, grab_offset_y: f64) -> usize {
        if self.travel == 0 || self.history_rows == 0 {
            return 0;
        }
        let min_y = f64::from(self.margin);
        let max_y = f64::from(self.margin + self.travel);
        let thumb_top = (local_y - grab_offset_y).clamp(min_y, max_y);
        let from_top = (thumb_top - min_y) / f64::from(self.travel);
        ((1.0 - from_top) * self.history_rows as f64)
            .round()
            .clamp(0.0, self.history_rows as f64) as usize
    }

    pub(super) fn view(self, highlighted: bool) -> ScrollbarView {
        ScrollbarView {
            track: self.track,
            thumb: self.thumb,
            highlighted,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use lune_terminal::PixelSize;

    use super::ScrollbarMetrics;

    fn area(width: u32, height: u32) -> PixelSize {
        PixelSize {
            width: NonZeroU32::new(width).unwrap(),
            height: NonZeroU32::new(height).unwrap(),
        }
    }

    #[test]
    fn scrollbar_positions_thumb_and_maps_drag_offsets() {
        let size = area(800, 604);
        let at_bottom = ScrollbarMetrics::compute(size, 100, 20, 0, 1.0).unwrap();
        assert_eq!(at_bottom.track.origin.y, 2);
        assert_eq!(at_bottom.track.size.height.get(), 600);
        assert_eq!(
            at_bottom.thumb.origin.y + at_bottom.thumb.size.height.get(),
            602
        );

        let at_top = ScrollbarMetrics::compute(size, 100, 20, 100, 1.0).unwrap();
        assert_eq!(at_top.thumb.origin.y, 2);

        let mid = ScrollbarMetrics::compute(size, 100, 20, 50, 1.0).unwrap();
        assert_eq!(mid.offset_for_y(2.0, 0.0), 100);
        assert_eq!(mid.offset_for_y(602.0, 0.0), 0);
        assert_eq!(mid.offset_for_y(f64::from(mid.thumb.origin.y), 0.0), 50);
        assert!(ScrollbarMetrics::compute(size, 0, 20, 0, 1.0).is_none());
    }
}
