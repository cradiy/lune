use std::num::NonZeroU32;

use lune_terminal::{PixelPosition, PixelSize};
use winit::dpi::PhysicalPosition;

use crate::PixelRect;

use super::PaneId;
use super::font::FontMetrics;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CloseTarget {
    Window,
    Tab(usize),
    Pane(PaneId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Button {
    Yes,
    No,
}

impl Button {
    pub(super) const fn toggle(self) -> Self {
        match self {
            Self::Yes => Self::No,
            Self::No => Self::Yes,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Dialog {
    pub(super) target: CloseTarget,
    pub(super) message: String,
    pub(super) selected: Button,
    pub(super) hovered: Option<Button>,
}

impl Dialog {
    pub(super) fn new(target: CloseTarget, message: String) -> Self {
        Self {
            target,
            message,
            selected: Button::Yes,
            hovered: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Layout {
    pub(super) message_y: u32,
    pub(super) yes: PixelRect,
    pub(super) no: PixelRect,
    pub(super) radius: u32,
    pub(super) border: u32,
    pub(super) pad_x: u32,
    pub(super) pad_y: u32,
}

impl Layout {
    pub(super) fn compute(area: PixelSize, metrics: FontMetrics, scale: f64) -> Option<Self> {
        let scale = scale.max(1.0);
        let cell_w = metrics.cell.width.get();
        let cell_h = metrics.cell.height.get();
        let pad_x = ((f64::from(cell_w) * 1.4).round() as u32).max(8);
        let pad_y = ((f64::from(cell_h) * 0.38).round() as u32).max(4);
        let gap_x = ((f64::from(cell_w) * 1.8).round() as u32).max(10);
        let gap_y = ((f64::from(cell_h) * 0.95).round() as u32).max(10);

        let yes_w = cell_w.saturating_mul(3).saturating_add(pad_x * 2);
        let no_w = cell_w.saturating_mul(2).saturating_add(pad_x * 2);
        let btn_h = cell_h.saturating_add(pad_y * 2);

        let total_w = yes_w.saturating_add(gap_x).saturating_add(no_w);
        let total_h = cell_h.saturating_add(gap_y).saturating_add(btn_h);

        let top = area.height.get().saturating_sub(total_h) / 2;
        let btn_y = top.saturating_add(cell_h).saturating_add(gap_y);
        let start_x = area.width.get().saturating_sub(total_w) / 2;

        let yes = PixelRect {
            origin: PixelPosition {
                x: start_x,
                y: btn_y,
            },
            size: PixelSize {
                width: NonZeroU32::new(yes_w.min(area.width.get()))?,
                height: NonZeroU32::new(btn_h.min(area.height.get()))?,
            },
        };
        let no = PixelRect {
            origin: PixelPosition {
                x: start_x.saturating_add(yes_w).saturating_add(gap_x),
                y: btn_y,
            },
            size: PixelSize {
                width: NonZeroU32::new(no_w.min(area.width.get()))?,
                height: NonZeroU32::new(btn_h.min(area.height.get()))?,
            },
        };

        let radius = ((5.0 * scale).round() as u32).clamp(3, 12).min(btn_h / 2);
        let border = ((1.5 * scale).round() as u32).clamp(1, 3);

        Some(Self {
            message_y: top,
            yes,
            no,
            radius,
            border,
            pad_x,
            pad_y,
        })
    }

    pub(super) fn hit(&self, position: PhysicalPosition<f64>) -> Option<Button> {
        if contains(self.yes, position) {
            Some(Button::Yes)
        } else if contains(self.no, position) {
            Some(Button::No)
        } else {
            None
        }
    }
}

fn contains(rect: PixelRect, position: PhysicalPosition<f64>) -> bool {
    let x = f64::from(rect.origin.x);
    let y = f64::from(rect.origin.y);
    position.x >= x
        && position.x < x + f64::from(rect.size.width.get())
        && position.y >= y
        && position.y < y + f64::from(rect.size.height.get())
}

pub(super) fn format_running(subject: &str, processes: &[String], fallback_count: usize) -> String {
    let mut unique: Vec<&str> = Vec::new();
    for name in processes {
        if !name.is_empty() && !unique.contains(&name.as_str()) {
            unique.push(name.as_str());
        }
    }
    if !unique.is_empty() {
        let shown = if unique.len() > 3 {
            format!("{}, ...", unique[..3].join(", "))
        } else {
            unique.join(", ")
        };
        format!("Are you sure you want to close this {subject}? It is running: {shown}.")
    } else if fallback_count > 1 {
        format!(
            "Are you sure you want to close this {subject}? It has {fallback_count} active panes."
        )
    } else {
        format!("Are you sure you want to close this {subject}?")
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use lune_terminal::PixelSize;
    use winit::dpi::PhysicalPosition;

    use super::super::font::FontMetrics;
    use super::{Button, Layout, format_running};

    #[test]
    fn layout_centers_buttons_and_hits_accurately() {
        let area = PixelSize {
            width: NonZeroU32::new(960).unwrap(),
            height: NonZeroU32::new(600).unwrap(),
        };
        let metrics = FontMetrics {
            size: 16.0,
            cell: PixelSize {
                width: NonZeroU32::new(10).unwrap(),
                height: NonZeroU32::new(20).unwrap(),
            },
            baseline: 15,
        };
        let layout = Layout::compute(area, metrics, 1.0).unwrap();
        assert!(layout.yes.origin.x < layout.no.origin.x);
        assert_eq!(layout.yes.origin.y, layout.no.origin.y);
        assert_eq!(
            layout.hit(PhysicalPosition::new(
                f64::from(layout.yes.origin.x + 5),
                f64::from(layout.yes.origin.y + 5),
            )),
            Some(Button::Yes)
        );
        assert_eq!(
            layout.hit(PhysicalPosition::new(
                f64::from(layout.no.origin.x + 5),
                f64::from(layout.no.origin.y + 5),
            )),
            Some(Button::No)
        );
        assert_eq!(layout.hit(PhysicalPosition::new(0.0, 0.0)), None);
    }

    #[test]
    fn formats_process_names_concisely() {
        assert_eq!(
            format_running("window", &["yazi".into()], 1),
            "Are you sure you want to close this window? It is running: yazi."
        );
        assert_eq!(
            format_running("window", &["yazi".into(), "yazi".into(), "nvim".into()], 2),
            "Are you sure you want to close this window? It is running: yazi, nvim."
        );
        assert_eq!(
            format_running("window", &[], 3),
            "Are you sure you want to close this window? It has 3 active panes."
        );
    }
}
