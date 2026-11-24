use std::num::NonZeroU32;

use lune_terminal::{PixelPosition, PixelSize};
use winit::dpi::PhysicalPosition;

use crate::PixelRect;

use super::PaneId;
use super::font::FontMetrics;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ConfirmTarget {
    CloseWindow,
    CloseTab(usize),
    ClosePane(PaneId),
    Paste { pane: PaneId, text: String },
}

impl ConfirmTarget {
    pub(super) const fn title(&self) -> &'static str {
        match self {
            Self::CloseWindow => "Close Window",
            Self::CloseTab(_) => "Close Tab",
            Self::ClosePane(_) => "Close Pane",
            Self::Paste { .. } => "Confirm Paste",
        }
    }

    pub(super) const fn is_paste(&self) -> bool {
        matches!(self, Self::Paste { .. })
    }
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
    pub(super) target: ConfirmTarget,
    pub(super) message: String,
    pub(super) selected: Button,
    pub(super) hovered: Option<Button>,
}

impl Dialog {
    pub(super) fn new(target: ConfirmTarget, message: String) -> Self {
        let selected = match target {
            ConfirmTarget::Paste { .. } => Button::No,
            _ => Button::Yes,
        };
        Self {
            target,
            message,
            selected,
            hovered: None,
        }
    }

    pub(super) fn message_columns(&self) -> usize {
        self.message.chars().count().max(self.target.title().len())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Layout {
    pub(super) card: PixelRect,
    pub(super) card_radius: u32,
    pub(super) indicator: PixelRect,
    pub(super) title_origin: PixelPosition,
    pub(super) message_origin: PixelPosition,
    pub(super) message_max_w: u32,
    pub(super) yes: PixelRect,
    pub(super) no: PixelRect,
    pub(super) radius: u32,
    pub(super) border: u32,
}

impl Layout {
    pub(super) fn compute(
        area: PixelSize,
        metrics: FontMetrics,
        scale: f64,
        message_columns: usize,
    ) -> Option<Self> {
        let scale = scale.max(1.0);
        let cell_w = metrics.cell.width.get();
        let cell_h = metrics.cell.height.get();
        let margin = (cell_w * 2).max(16);
        let card_pad_x = ((f64::from(cell_w) * 2.0).round() as u32).max(16);
        let card_pad_y = ((f64::from(cell_h) * 0.85).round() as u32).max(14);
        let title_gap = ((f64::from(cell_h) * 0.45).round() as u32).max(6);
        let button_gap_y = ((f64::from(cell_h) * 0.95).round() as u32).max(12);

        let btn_pad_x = ((f64::from(cell_w) * 1.5).round() as u32).max(10);
        let btn_pad_y = ((f64::from(cell_h) * 0.34).round() as u32).max(4);
        let btn_gap_x = ((f64::from(cell_w) * 1.1).round() as u32).max(8);

        let yes_w = cell_w.saturating_mul(3).saturating_add(btn_pad_x * 2);
        let no_w = cell_w.saturating_mul(2).saturating_add(btn_pad_x * 2);
        let btn_h = cell_h.saturating_add(btn_pad_y * 2);
        let buttons_w = yes_w.saturating_add(btn_gap_x).saturating_add(no_w);

        let indicator_w = (cell_w / 3).clamp(3, 5);
        let indicator_gap = (cell_w * 2 / 3).max(6);
        let indicator_h = (cell_h * 2 / 3).max(6).min(cell_h);

        let max_card_w = area.width.get().saturating_sub(margin * 2).min(960).max(
            buttons_w
                .saturating_add(card_pad_x * 2)
                .min(area.width.get()),
        );
        let content_est_w = (message_columns as u32)
            .saturating_mul(cell_w)
            .max(buttons_w)
            .max(cell_w.saturating_mul(28));
        let card_w = content_est_w
            .saturating_add(card_pad_x * 2)
            .min(max_card_w)
            .min(area.width.get());
        let card_h = card_pad_y
            .saturating_mul(2)
            .saturating_add(cell_h)
            .saturating_add(title_gap)
            .saturating_add(cell_h)
            .saturating_add(button_gap_y)
            .saturating_add(btn_h)
            .min(area.height.get());

        let card_x = area.width.get().saturating_sub(card_w) / 2;
        let card_y = area.height.get().saturating_sub(card_h) / 2;
        let card = PixelRect {
            origin: PixelPosition {
                x: card_x,
                y: card_y,
            },
            size: PixelSize {
                width: NonZeroU32::new(card_w)?,
                height: NonZeroU32::new(card_h)?,
            },
        };

        let title_y = card_y.saturating_add(card_pad_y);
        let indicator = PixelRect {
            origin: PixelPosition {
                x: card_x.saturating_add(card_pad_x),
                y: title_y.saturating_add(cell_h.saturating_sub(indicator_h) / 2),
            },
            size: PixelSize {
                width: NonZeroU32::new(indicator_w)?,
                height: NonZeroU32::new(indicator_h)?,
            },
        };
        let title_origin = PixelPosition {
            x: card_x
                .saturating_add(card_pad_x)
                .saturating_add(indicator_w)
                .saturating_add(indicator_gap),
            y: title_y,
        };
        let message_origin = PixelPosition {
            x: card_x.saturating_add(card_pad_x),
            y: title_y.saturating_add(cell_h).saturating_add(title_gap),
        };
        let message_max_w = card_w.saturating_sub(card_pad_x * 2);

        let btn_y = message_origin
            .y
            .saturating_add(cell_h)
            .saturating_add(button_gap_y);
        let btn_right = card_x.saturating_add(card_w).saturating_sub(card_pad_x);
        let no_x = btn_right.saturating_sub(no_w);
        let yes_x = no_x.saturating_sub(btn_gap_x).saturating_sub(yes_w);

        let yes = PixelRect {
            origin: PixelPosition { x: yes_x, y: btn_y },
            size: PixelSize {
                width: NonZeroU32::new(yes_w.min(area.width.get()))?,
                height: NonZeroU32::new(btn_h.min(area.height.get()))?,
            },
        };
        let no = PixelRect {
            origin: PixelPosition { x: no_x, y: btn_y },
            size: PixelSize {
                width: NonZeroU32::new(no_w.min(area.width.get()))?,
                height: NonZeroU32::new(btn_h.min(area.height.get()))?,
            },
        };

        let card_radius = ((8.0 * scale).round() as u32).clamp(6, 14).min(card_h / 2);
        let radius = ((5.0 * scale).round() as u32).clamp(4, 10).min(btn_h / 2);
        let border = ((1.0 * scale).round() as u32).clamp(1, 2);

        Some(Self {
            card,
            card_radius,
            indicator,
            title_origin,
            message_origin,
            message_max_w,
            yes,
            no,
            radius,
            border,
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

pub(super) fn needs_paste_confirm(text: &str) -> bool {
    text.bytes()
        .any(|byte| matches!(byte, b'\n' | b'\r') || (byte < 0x20 && byte != b'\t') || byte == 0x7f)
}

pub(super) fn format_paste(text: &str) -> String {
    let lines = text
        .split('\n')
        .filter(|line| !line.is_empty())
        .count()
        .max(1);
    let chars = text.chars().count();
    if text.contains('\n') || text.contains('\r') {
        format!("Paste {lines} lines ({chars} chars) without bracketed paste?")
    } else {
        format!("Paste text containing control characters ({chars} chars)?")
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use lune_terminal::PixelSize;
    use winit::dpi::PhysicalPosition;

    use super::super::font::FontMetrics;
    use super::{
        Button, ConfirmTarget, Dialog, Layout, format_paste, format_running, needs_paste_confirm,
    };

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
        let layout = Layout::compute(area, metrics, 1.0, 40).unwrap();
        assert!(layout.card.origin.x < layout.yes.origin.x);
        assert!(layout.yes.origin.x < layout.no.origin.x);
        assert_eq!(layout.yes.origin.y, layout.no.origin.y);
        assert!(
            layout.no.origin.x + layout.no.size.width.get()
                <= layout.card.origin.x + layout.card.size.width.get()
        );
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

    #[test]
    fn paste_confirmation_detects_newlines_and_defaults_to_no() {
        assert!(!needs_paste_confirm("ls -la\tfoo"));
        assert!(needs_paste_confirm("echo hi\nrm -rf /"));
        assert!(needs_paste_confirm("echo hi\r"));
        assert!(needs_paste_confirm("abc\x1bdef"));
        assert_eq!(
            format_paste("echo a\necho b\n"),
            "Paste 2 lines (14 chars) without bracketed paste?"
        );
        let dialog = Dialog::new(
            ConfirmTarget::Paste {
                pane: 1,
                text: "a\nb".into(),
            },
            format_paste("a\nb"),
        );
        assert_eq!(dialog.selected, Button::No);
    }
}
