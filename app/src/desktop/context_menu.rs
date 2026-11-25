//! Native floating right-click context menu for terminal panes, command blocks,
//! hyperlinks, tabs, and titlebar/sidebar chrome.

use std::num::NonZeroU32;

use lune_terminal::{PixelPosition, PixelSize};
use winit::dpi::PhysicalPosition;

use crate::PixelRect;

use super::PaneId;
use super::blocks::BlockAction;
use super::config::TabPosition;
use super::font::FontMetrics;
use super::shortcuts::Action;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MenuAction {
    Shortcut(Action),
    OpenLink(String),
    CopyLink(String),
    BlockAction {
        pane_id: PaneId,
        prompt_row: usize,
        action: BlockAction,
    },
    DuplicateTab(usize),
    RenameTab(usize),
    ResetTabTitle(usize),
    CloseTabAt(usize),
    CloseOtherTabs(usize),
    CloseTabsToRight(usize),
    SetTabPosition(TabPosition),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MenuEntry {
    Item {
        action: MenuAction,
        label: &'static str,
        shortcut: String,
        danger: bool,
    },
    Separator,
}

impl MenuEntry {
    pub(super) fn item(
        action: MenuAction,
        label: &'static str,
        shortcut: impl Into<String>,
    ) -> Self {
        Self::Item {
            action,
            label,
            shortcut: shortcut.into(),
            danger: false,
        }
    }

    pub(super) fn danger(
        action: MenuAction,
        label: &'static str,
        shortcut: impl Into<String>,
    ) -> Self {
        Self::Item {
            action,
            label,
            shortcut: shortcut.into(),
            danger: true,
        }
    }

    pub(super) fn is_item(&self) -> bool {
        matches!(self, Self::Item { .. })
    }
}

#[derive(Clone, Debug)]
pub(super) struct ContextMenu {
    pub(super) anchor: PixelPosition,
    pub(super) entries: Vec<MenuEntry>,
    pub(super) hovered: Option<usize>,
}

impl ContextMenu {
    pub(super) fn new(anchor: PixelPosition, entries: Vec<MenuEntry>) -> Option<Self> {
        if !entries.iter().any(MenuEntry::is_item) {
            return None;
        }
        Some(Self {
            anchor,
            entries,
            hovered: None,
        })
    }

    pub(super) fn action_at(&self, index: usize) -> Option<MenuAction> {
        match self.entries.get(index) {
            Some(MenuEntry::Item { action, .. }) => Some(action.clone()),
            _ => None,
        }
    }

    pub(super) fn step(&mut self, delta: isize) -> bool {
        let count = self.entries.len();
        if count == 0 {
            return false;
        }
        let start = match self.hovered {
            Some(idx) => idx,
            None => {
                if delta >= 0 {
                    let first = self.entries.iter().position(MenuEntry::is_item);
                    let changed = self.hovered != first;
                    self.hovered = first;
                    return changed;
                }
                let last = self.entries.iter().rposition(MenuEntry::is_item);
                let changed = self.hovered != last;
                self.hovered = last;
                return changed;
            }
        };
        let mut cursor = start;
        for _ in 0..count {
            cursor = if delta >= 0 {
                (cursor + 1) % count
            } else if cursor == 0 {
                count - 1
            } else {
                cursor - 1
            };
            if self.entries[cursor].is_item() {
                let changed = self.hovered != Some(cursor);
                self.hovered = Some(cursor);
                return changed;
            }
        }
        false
    }
}

#[derive(Clone, Debug)]
pub(super) struct RowSlot {
    pub(super) entry_index: usize,
    pub(super) rect: PixelRect,
    pub(super) is_separator: bool,
}

#[derive(Clone, Debug)]
pub(super) struct Layout {
    pub(super) card: PixelRect,
    pub(super) radius: u32,
    pub(super) pad_x: u32,
    pub(super) rows: Vec<RowSlot>,
}

impl Layout {
    pub(super) fn compute(
        window: PixelSize,
        metrics: FontMetrics,
        scale: f64,
        menu: &ContextMenu,
    ) -> Option<Self> {
        if menu.entries.is_empty() {
            return None;
        }
        let scale = scale.max(1.0);
        let floor = |logical: f64| (logical * scale).round().max(1.0) as u32;
        let cell_w = metrics.cell.width.get().max(1);
        let cell_h = metrics.cell.height.get().max(1);

        let outer_pad_x = floor(5.0);
        let outer_pad_y = floor(5.0);
        let row_pad_x = floor(10.0);
        let row_h = cell_h.saturating_add(floor(10.0)).max(floor(26.0));
        let sep_h = floor(9.0);
        let shortcut_gap = cell_w.saturating_mul(3).max(floor(18.0));

        let mut max_label_chars = 0usize;
        let mut max_shortcut_chars = 0usize;
        let mut total_rows_h = 0u32;
        for entry in &menu.entries {
            match entry {
                MenuEntry::Item {
                    label, shortcut, ..
                } => {
                    max_label_chars = max_label_chars.max(label.chars().count());
                    max_shortcut_chars = max_shortcut_chars.max(shortcut.chars().count());
                    total_rows_h = total_rows_h.saturating_add(row_h);
                }
                MenuEntry::Separator => {
                    total_rows_h = total_rows_h.saturating_add(sep_h);
                }
            }
        }

        let content_w = (max_label_chars as u32)
            .saturating_mul(cell_w)
            .saturating_add(if max_shortcut_chars > 0 {
                shortcut_gap.saturating_add((max_shortcut_chars as u32).saturating_mul(cell_w))
            } else {
                0
            })
            .saturating_add(row_pad_x * 2);
        let min_w = floor(180.0);
        let max_w = window.width.get().saturating_sub(floor(16.0)).max(1);
        let card_w = content_w
            .saturating_add(outer_pad_x * 2)
            .max(min_w)
            .min(max_w);
        let max_h = window.height.get().saturating_sub(floor(16.0)).max(1);
        let card_h = total_rows_h.saturating_add(outer_pad_y * 2).min(max_h);

        let margin = floor(8.0);
        let max_x = window.width.get().saturating_sub(card_w + margin);
        let max_y = window.height.get().saturating_sub(card_h + margin);
        // Prefer opening to the bottom-right of the pointer; flip cleanly near window edges.
        let card_x = if menu.anchor.x.saturating_add(card_w + margin) <= window.width.get() {
            menu.anchor.x.max(margin)
        } else {
            menu.anchor.x.saturating_sub(card_w).clamp(margin, max_x)
        };
        let card_y = if menu.anchor.y.saturating_add(card_h + margin) <= window.height.get() {
            menu.anchor.y.max(margin)
        } else {
            menu.anchor.y.saturating_sub(card_h).clamp(margin, max_y)
        };

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

        let row_w = NonZeroU32::new(card_w.saturating_sub(outer_pad_x * 2))?;
        let mut cursor_y = card_y.saturating_add(outer_pad_y);
        let bottom_limit = card_y.saturating_add(card_h.saturating_sub(outer_pad_y));
        let mut rows = Vec::with_capacity(menu.entries.len());
        for (index, entry) in menu.entries.iter().enumerate() {
            let (h, is_separator) = match entry {
                MenuEntry::Item { .. } => (row_h, false),
                MenuEntry::Separator => (sep_h, true),
            };
            if cursor_y.saturating_add(h) > bottom_limit {
                break;
            }
            if let Some(h_nz) = NonZeroU32::new(h) {
                rows.push(RowSlot {
                    entry_index: index,
                    rect: PixelRect {
                        origin: PixelPosition {
                            x: card_x.saturating_add(outer_pad_x),
                            y: cursor_y,
                        },
                        size: PixelSize {
                            width: row_w,
                            height: h_nz,
                        },
                    },
                    is_separator,
                });
            }
            cursor_y = cursor_y.saturating_add(h);
        }

        Some(Self {
            card,
            radius: floor(9.0).clamp(6, 14),
            pad_x: row_pad_x,
            rows,
        })
    }

    pub(super) fn contains_card(&self, point: PhysicalPosition<f64>) -> bool {
        let x = f64::from(self.card.origin.x);
        let y = f64::from(self.card.origin.y);
        point.x >= x
            && point.x < x + f64::from(self.card.size.width.get())
            && point.y >= y
            && point.y < y + f64::from(self.card.size.height.get())
    }

    pub(super) fn hit(&self, point: PhysicalPosition<f64>) -> Option<usize> {
        for slot in &self.rows {
            if slot.is_separator {
                continue;
            }
            let x = f64::from(slot.rect.origin.x);
            let y = f64::from(slot.rect.origin.y);
            if point.x >= x
                && point.x < x + f64::from(slot.rect.size.width.get())
                && point.y >= y
                && point.y < y + f64::from(slot.rect.size.height.get())
            {
                return Some(slot.entry_index);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use lune_terminal::{PixelPosition, PixelSize};
    use winit::dpi::PhysicalPosition;

    use super::{ContextMenu, Layout, MenuAction, MenuEntry};
    use crate::desktop::font::FontMetrics;
    use crate::desktop::shortcuts::Action;

    fn test_metrics() -> FontMetrics {
        FontMetrics {
            size: 13.0,
            cell: PixelSize {
                width: NonZeroU32::new(8).unwrap(),
                height: NonZeroU32::new(16).unwrap(),
            },
            baseline: 12,
        }
    }

    #[test]
    fn menu_navigation_skips_separators_and_clamps_to_window() {
        let mut menu = ContextMenu::new(
            PixelPosition { x: 790, y: 590 },
            vec![
                MenuEntry::item(MenuAction::Shortcut(Action::Copy), "Copy", "Ctrl+Shift+C"),
                MenuEntry::Separator,
                MenuEntry::item(MenuAction::Shortcut(Action::Paste), "Paste", "Ctrl+Shift+V"),
            ],
        )
        .unwrap();

        assert!(menu.step(1));
        assert_eq!(menu.hovered, Some(0));
        assert!(menu.step(1));
        assert_eq!(menu.hovered, Some(2));
        assert!(menu.step(-1));
        assert_eq!(menu.hovered, Some(0));

        let window = PixelSize {
            width: NonZeroU32::new(800).unwrap(),
            height: NonZeroU32::new(600).unwrap(),
        };
        let layout = Layout::compute(window, test_metrics(), 1.0, &menu).unwrap();
        assert!(layout.card.origin.x + layout.card.size.width.get() <= window.width.get());
        assert!(layout.card.origin.y + layout.card.size.height.get() <= window.height.get());

        // Hit-testing ignores the separator row and hits the second item cleanly.
        let sep = &layout.rows[1];
        assert_eq!(
            layout.hit(PhysicalPosition::new(
                f64::from(sep.rect.origin.x + 10),
                f64::from(sep.rect.origin.y + 2),
            )),
            None
        );
        let paste = &layout.rows[2];
        assert_eq!(
            layout.hit(PhysicalPosition::new(
                f64::from(paste.rect.origin.x + 10),
                f64::from(paste.rect.origin.y + 6),
            )),
            Some(2)
        );
    }
}
