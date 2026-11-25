//! Modern mode Chrome-style animated top tabs, resizable left tab sidebar,
//! inline titlebar layout, and borderless edge resize helpers.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::time::Instant;

use lune_terminal::{Palette, PixelPosition, PixelSize};
use lune_text::WidthPolicy;
use unicode_segmentation::UnicodeSegmentation;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::window::ResizeDirection;

use crate::PixelRect;

use super::Tab;
use super::config::TabPosition;
use super::wayland_decorations::WindowButtons;

/// Angular frequency for the critically damped tab reordering spring (~130 ms settle).
const TAB_SPRING_OMEGA: f32 = 64.0;

/// Adaptive physical-pixel dimensions for the Modern inline titlebar, top Chrome
/// tabs, and left tab sidebar, derived dynamically from font cell metrics, window
/// dimensions, and display scale factor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct TitlebarMetrics {
    pub(super) titlebar_height: u32,
    pub(super) button_size: u32,
    pub(super) control_size: u32,
    pub(super) control_gap: u32,
    pub(super) edge_padding: u32,
    pub(super) item_gap: u32,
    pub(super) drag_gap_with_controls: u32,
    pub(super) drag_gap_plain: u32,
    pub(super) top_tab_top: u32,
    pub(super) top_tab_gap: u32,
    pub(super) top_tab_min_width: u32,
    pub(super) top_tab_max_width: u32,
    pub(super) top_tab_radius: u32,
    pub(super) wing_width: u32,
    pub(super) wing_height: u32,
    pub(super) close_size: u32,
    pub(super) tab_icon_size: u32,
    pub(super) close_icon_size: u32,
    pub(super) sidebar_min_width: u32,
    pub(super) sidebar_default_width: u32,
    pub(super) sidebar_max_width: u32,
    pub(super) sidebar_row_height: u32,
    pub(super) sidebar_row_gap: u32,
    pub(super) sidebar_pad_x: u32,
    pub(super) sidebar_pad_top: u32,
    pub(super) sidebar_row_radius: u32,
}

impl TitlebarMetrics {
    pub(super) fn compute(size: PhysicalSize<u32>, cell: PixelSize, scale: f64) -> Self {
        let scale = scale.max(1.0);
        let floor = |logical: f64| (logical * scale).round().max(1.0) as u32;
        let cell_w = cell.width.get().max(1);
        let cell_h = cell.height.get().max(1);

        let desired_height = (cell_h.saturating_mul(19) / 10).max(floor(32.0));
        let titlebar_height = if size.height <= desired_height.saturating_mul(2) {
            0
        } else {
            desired_height
        };
        let max_btn = titlebar_height.saturating_sub(floor(4.0)).max(1);
        let button_size = (titlebar_height.saturating_mul(72) / 100)
            .max(floor(22.0))
            .min(max_btn);
        let control_size = (titlebar_height.saturating_mul(62) / 100)
            .max(floor(20.0))
            .min(max_btn);
        let control_gap = (control_size / 4).max(floor(4.0));
        let edge_padding = cell_w.clamp(floor(8.0), (titlebar_height / 2).max(floor(8.0)));
        let item_gap = (edge_padding.saturating_mul(2) / 3).max(floor(5.0));

        let drag_gap_with_controls = (size.width / 20)
            .clamp(button_size, button_size.saturating_mul(2))
            .min(size.width / 8);
        let drag_gap_plain = (button_size / 2).max(edge_padding).min(size.width / 10);

        let desired_tab_h = cell_h.saturating_add((cell_h / 2).max(floor(10.0)));
        let top_tab_top = titlebar_height
            .saturating_sub(desired_tab_h)
            .clamp(floor(4.0), (titlebar_height / 4).max(floor(4.0)));
        let top_tab_h = titlebar_height.saturating_sub(top_tab_top).max(cell_h);
        let top_tab_gap = (edge_padding / 4).max(floor(2.0));
        let top_tab_min_width = cell_w
            .saturating_mul(10)
            .saturating_add(button_size)
            .max(floor(80.0));
        let top_tab_max_width = cell_w
            .saturating_mul(22)
            .saturating_add(button_size)
            .max(top_tab_min_width.saturating_mul(2))
            .min((size.width / 2).max(top_tab_min_width));
        let top_tab_radius = (top_tab_h / 3).clamp(floor(6.0), (titlebar_height / 2).max(1));
        let wing_height = top_tab_radius;
        let wing_width = (wing_height.saturating_mul(6) / 5).max(wing_height);

        let close_size = cell_h
            .min(top_tab_h.saturating_sub(floor(6.0)).max(floor(14.0)))
            .max(floor(16.0));
        let tab_icon_size = (cell_h.saturating_mul(7) / 10)
            .max(floor(12.0))
            .min(top_tab_h.saturating_sub(floor(8.0)).max(floor(12.0)));
        let close_icon_size = (close_size / 2).max(floor(8.0));

        let sidebar_pad_x = (edge_padding.saturating_mul(4) / 5).max(floor(6.0));
        let sidebar_min_width = cell_w
            .saturating_mul(15)
            .saturating_add(sidebar_pad_x.saturating_mul(2))
            .max(floor(128.0));
        let sidebar_window_cap = size.width.saturating_mul(2) / 5;
        let sidebar_max_width = cell_w
            .saturating_mul(38)
            .saturating_add(sidebar_pad_x.saturating_mul(2))
            .min(sidebar_window_cap);
        let sidebar_default_width = cell_w
            .saturating_mul(23)
            .saturating_add(sidebar_pad_x.saturating_mul(2))
            .clamp(sidebar_min_width, sidebar_max_width.max(sidebar_min_width));

        let sidebar_row_height = cell_h
            .saturating_add((cell_h.saturating_mul(7) / 10).max(floor(12.0)))
            .max(floor(32.0));
        let sidebar_row_gap = (sidebar_row_height / 7).max(floor(4.0));
        let sidebar_pad_top = sidebar_row_gap.saturating_mul(2).max(floor(8.0));
        let sidebar_row_radius = (sidebar_row_height / 4).max(floor(7.0));

        Self {
            titlebar_height,
            button_size,
            control_size,
            control_gap,
            edge_padding,
            item_gap,
            drag_gap_with_controls,
            drag_gap_plain,
            top_tab_top,
            top_tab_gap,
            top_tab_min_width,
            top_tab_max_width,
            top_tab_radius,
            wing_width,
            wing_height,
            close_size,
            tab_icon_size,
            close_icon_size,
            sidebar_min_width,
            sidebar_default_width,
            sidebar_max_width,
            sidebar_row_height,
            sidebar_row_gap,
            sidebar_pad_x,
            sidebar_pad_top,
            sidebar_row_radius,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TitlebarAction {
    CommandPalette,
    CommandBar,
    ToggleSidebar,
    SelectTab(usize),
    CloseTab(usize),
    NewTab,
    SidebarNewTab,
    Minimize,
    Maximize,
    Close,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ButtonSlot {
    pub(super) action: TitlebarAction,
    pub(super) rect: PixelRect,
    pub(super) circular: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TabSlot {
    pub(super) id: u64,
    pub(super) index: usize,
    pub(super) rect: PixelRect,
    pub(super) close_rect: Option<PixelRect>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SpringSlot {
    pub(super) current: f32,
    pub(super) velocity: f32,
    pub(super) target: f32,
}

impl SpringSlot {
    pub(super) fn settled(target: f32) -> Self {
        Self {
            current: target,
            velocity: 0.0,
            target,
        }
    }

    pub(super) fn step(&mut self, dt: f32) -> bool {
        let displacement = self.current - self.target;
        let spring = self.velocity + TAB_SPRING_OMEGA * displacement;
        let decay = (-TAB_SPRING_OMEGA * dt).exp();
        self.current = self.target + (displacement + spring * dt) * decay;
        self.velocity = (self.velocity - TAB_SPRING_OMEGA * spring * dt) * decay;

        if (self.target - self.current).abs() < 0.18 && self.velocity.abs() < 20.0 {
            self.current = self.target;
            self.velocity = 0.0;
            false
        } else {
            true
        }
    }

    fn is_moving(&self) -> bool {
        (self.target - self.current).abs() >= 0.18 || self.velocity.abs() >= 20.0
    }
}

#[derive(Default)]
pub(super) struct TabAnimator {
    slots: HashMap<u64, SpringSlot>,
    last_step: Option<Instant>,
    position: Option<TabPosition>,
}

impl TabAnimator {
    pub(super) fn sync(
        &mut self,
        position: TabPosition,
        targets: &[(u64, f32)],
        dragged: Option<(u64, f32)>,
        now: Instant,
    ) {
        if self.position != Some(position) {
            self.slots.clear();
            self.position = Some(position);
            self.last_step = None;
        }
        self.slots
            .retain(|id, _| targets.iter().any(|(tid, _)| tid == id));
        for &(id, target) in targets {
            let slot = self
                .slots
                .entry(id)
                .or_insert_with(|| SpringSlot::settled(target));
            if let Some((drag_id, drag_coord)) = dragged
                && drag_id == id
            {
                slot.current = drag_coord;
                slot.target = drag_coord;
                slot.velocity = 0.0;
            } else {
                slot.target = target;
            }
        }
        if self.moving() && self.last_step.is_none() {
            self.last_step = Some(now);
        }
    }

    pub(super) fn tick(&mut self, now: Instant) -> bool {
        if !self.moving() {
            self.last_step = None;
            return false;
        }
        let dt = self
            .last_step
            .map_or(1.0 / 60.0, |prev| {
                now.saturating_duration_since(prev).as_secs_f32()
            })
            .clamp(0.001, 0.05);
        self.last_step = Some(now);
        let mut any_moving = false;
        for slot in self.slots.values_mut() {
            if slot.step(dt) {
                any_moving = true;
            }
        }
        if !any_moving {
            self.last_step = None;
        }
        true
    }

    pub(super) fn moving(&self) -> bool {
        self.slots.values().any(SpringSlot::is_moving)
    }

    pub(super) fn coord(&self, tab_id: u64, fallback: f32) -> f32 {
        self.slots.get(&tab_id).map_or(fallback, |s| s.current)
    }

    pub(super) fn clear(&mut self) {
        self.slots.clear();
        self.last_step = None;
        self.position = None;
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TabDrag {
    pub(super) tab_id: u64,
    pub(super) grab_offset: f32,
    pub(super) start_pos: PhysicalPosition<f64>,
    pub(super) current_coord: f32,
    pub(super) moved: bool,
    pub(super) in_sidebar: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SidebarResizeDrag {
    pub(super) start_x: f64,
    pub(super) start_width_px: f64,
}

#[derive(Clone, Copy)]
pub(super) struct ReflowParams {
    pub(super) size: PhysicalSize<u32>,
    pub(super) cell: PixelSize,
    pub(super) scale: f64,
    pub(super) enabled: bool,
    pub(super) tab_position: TabPosition,
    pub(super) sidebar_collapsed: bool,
    pub(super) sidebar_width_cells: Option<f64>,
    pub(super) window_buttons: WindowButtons,
    pub(super) tabs: usize,
    pub(super) active: usize,
}

#[derive(Default)]
pub(super) struct Layout {
    pub(super) width: u32,
    pub(super) header_width: u32,
    pub(super) height: u32,
    pub(super) content_top_inset: u32,
    pub(super) window_height: u32,
    pub(super) metrics: TitlebarMetrics,
    pub(super) tab_position: TabPosition,
    pub(super) sidebar_collapsed: bool,
    pub(super) sidebar_width: u32,
    pub(super) sidebar_scroll: usize,
    pub(super) sidebar_visible_rows: usize,
    last_active: Option<usize>,
    pub(super) top_strip_left: u32,
    pub(super) top_strip_right: u32,
    pub(super) top_tab_width: u32,
    pub(super) top_tab_gap: u32,
    pub(super) sidebar_list_top: u32,
    pub(super) sidebar_list_bottom: u32,
    pub(super) sidebar_row_height: u32,
    pub(super) sidebar_row_gap: u32,
    pub(super) command_bar: Option<PixelRect>,
    pub(super) sidebar_header_rect: Option<PixelRect>,
    pub(super) sidebar_header_plus: Option<PixelRect>,
    pub(super) sidebar_footer_rect: Option<PixelRect>,
    pub(super) buttons: Vec<ButtonSlot>,
    pub(super) top_tabs: Vec<TabSlot>,
    pub(super) sidebar_tabs: Vec<TabSlot>,
}

pub(super) struct View<'a> {
    pub(super) layout: &'a Layout,
    pub(super) animator: &'a TabAnimator,
    pub(super) dragged_tab: Option<u64>,
    pub(super) tabs: &'a [Tab],
    pub(super) active: usize,
    pub(super) palette: &'a Palette,
    pub(super) hovered: Option<TitlebarAction>,
    pub(super) maximized: bool,
    pub(super) background_opacity: f32,
    pub(super) palette_shortcut: &'a str,
    pub(super) new_tab_shortcut: &'a str,
    pub(super) scale: f64,
}

impl Layout {
    pub(super) fn height(
        size: PhysicalSize<u32>,
        cell: PixelSize,
        scale: f64,
        enabled: bool,
    ) -> u32 {
        if !enabled {
            return 0;
        }
        TitlebarMetrics::compute(size, cell, scale).titlebar_height
    }

    pub(super) fn compute_sidebar_width(
        size: PhysicalSize<u32>,
        cell: PixelSize,
        scale: f64,
        enabled: bool,
        tab_position: TabPosition,
        collapsed: bool,
        custom_cells: Option<f64>,
    ) -> u32 {
        if !enabled || tab_position != TabPosition::Left || collapsed {
            return 0;
        }
        let metrics = TitlebarMetrics::compute(size, cell, scale);
        if metrics.titlebar_height == 0 || metrics.sidebar_max_width < metrics.sidebar_min_width {
            return 0;
        }
        let desired = match custom_cells {
            Some(cells) => (cells * f64::from(cell.width.get().max(1))).round() as u32,
            None => metrics.sidebar_default_width,
        };
        desired.clamp(metrics.sidebar_min_width, metrics.sidebar_max_width)
    }

    pub(super) fn reflow(&mut self, params: ReflowParams, tab_id: impl Fn(usize) -> u64) {
        let ReflowParams {
            size,
            cell,
            scale,
            enabled,
            tab_position,
            sidebar_collapsed,
            sidebar_width_cells,
            window_buttons,
            tabs,
            active,
        } = params;
        self.buttons.clear();
        self.top_tabs.clear();
        self.sidebar_tabs.clear();
        self.command_bar = None;
        self.sidebar_header_rect = None;
        self.sidebar_header_plus = None;
        self.sidebar_footer_rect = None;
        self.width = size.width;
        self.header_width = 0;
        self.content_top_inset = 0;
        self.window_height = size.height;
        self.tab_position = tab_position;
        self.sidebar_collapsed = sidebar_collapsed;
        let metrics = if enabled {
            TitlebarMetrics::compute(size, cell, scale)
        } else {
            TitlebarMetrics::default()
        };
        self.metrics = metrics;
        self.height = metrics.titlebar_height;
        self.sidebar_width = Self::compute_sidebar_width(
            size,
            cell,
            scale,
            enabled,
            tab_position,
            sidebar_collapsed,
            sidebar_width_cells,
        );
        if self.height == 0 || size.width == 0 {
            self.sidebar_width = 0;
            return;
        }
        self.header_width = size.width;
        self.content_top_inset = self.height;

        let pad_x = metrics.edge_padding;
        let btn_size = metrics.button_size;
        let btn_y = self.height.saturating_sub(btn_size) / 2;

        // Right-aligned circular window control buttons.
        let mut right_limit = self.header_width.saturating_sub(pad_x);
        if window_buttons.any() {
            let control_size = metrics.control_size;
            let control_gap = metrics.control_gap;
            let control_y = self.height.saturating_sub(control_size) / 2;
            let mut cursor_x = size.width.saturating_sub(pad_x);
            for (show, action) in [
                (window_buttons.close, TitlebarAction::Close),
                (window_buttons.maximize, TitlebarAction::Maximize),
                (window_buttons.minimize, TitlebarAction::Minimize),
            ] {
                if !show {
                    continue;
                }
                if cursor_x < control_size {
                    break;
                }
                cursor_x -= control_size;
                if let Some(side) = NonZeroU32::new(control_size) {
                    self.buttons.push(ButtonSlot {
                        action,
                        rect: PixelRect {
                            origin: PixelPosition {
                                x: cursor_x,
                                y: control_y,
                            },
                            size: PixelSize {
                                width: side,
                                height: side,
                            },
                        },
                        circular: true,
                    });
                }
                cursor_x = cursor_x.saturating_sub(control_gap);
            }
            right_limit = cursor_x;
        }

        // Left-aligned Lune logo button (opens Command Palette).
        let mut left_x = pad_x;
        if left_x.saturating_add(btn_size) <= right_limit
            && let Some(side) = NonZeroU32::new(btn_size)
        {
            self.buttons.push(ButtonSlot {
                action: TitlebarAction::CommandPalette,
                rect: PixelRect {
                    origin: PixelPosition {
                        x: left_x,
                        y: btn_y,
                    },
                    size: PixelSize {
                        width: side,
                        height: side,
                    },
                },
                circular: false,
            });
            left_x = left_x
                .saturating_add(btn_size)
                .saturating_add(metrics.item_gap);
        }

        if tab_position == TabPosition::Left {
            // Sidebar mode: collapse/expand sidebar toggle next to logo, plus centered Command Bar.
            if left_x.saturating_add(btn_size) <= right_limit
                && let Some(side) = NonZeroU32::new(btn_size)
            {
                self.buttons.push(ButtonSlot {
                    action: TitlebarAction::ToggleSidebar,
                    rect: PixelRect {
                        origin: PixelPosition {
                            x: left_x,
                            y: btn_y,
                        },
                        size: PixelSize {
                            width: side,
                            height: side,
                        },
                    },
                    circular: false,
                });
                left_x = left_x
                    .saturating_add(btn_size)
                    .saturating_add(metrics.item_gap);
            }

            // Centered VS Code / Raycast-style Command Bar search pill in the top bar.
            let bar_margin = metrics.edge_padding.saturating_mul(2);
            let avail_left = left_x.saturating_add(bar_margin);
            let avail_right = right_limit.saturating_sub(bar_margin);
            if avail_right > avail_left {
                let avail_w = avail_right - avail_left;
                let cell_w = cell.width.get().max(1);
                let cell_h = cell.height.get().max(1);
                let min_bar_w = cell_w.saturating_mul(18).min(avail_w);
                let max_bar_w = cell_w.saturating_mul(46).max(min_bar_w);
                let desired_w = (size.width.saturating_mul(42) / 100)
                    .clamp(min_bar_w, max_bar_w)
                    .min(avail_w);
                let desired_h = (self.height.saturating_mul(72) / 100)
                    .max(cell_h.saturating_add(4))
                    .min(self.height.saturating_sub(6).max(1));
                if let (Some(w_nz), Some(h_nz)) =
                    (NonZeroU32::new(desired_w), NonZeroU32::new(desired_h))
                {
                    let ideal_x = size.width.saturating_sub(desired_w) / 2;
                    let max_x = avail_right.saturating_sub(desired_w);
                    let bar_x = ideal_x.clamp(avail_left, max_x.max(avail_left));
                    let bar_y = self.height.saturating_sub(desired_h) / 2;
                    self.command_bar = Some(PixelRect {
                        origin: PixelPosition { x: bar_x, y: bar_y },
                        size: PixelSize {
                            width: w_nz,
                            height: h_nz,
                        },
                    });
                }
            }

            if self.sidebar_width == 0 {
                return;
            }

            // Polished left sidebar: top TABS header (+ button), scrollable card list, pinned footer.
            let row_pad_x = metrics.sidebar_pad_x;
            let inner_w = self.sidebar_width.saturating_sub(row_pad_x * 2 + 1);
            let row_h = metrics.sidebar_row_height;
            let row_gap = metrics.sidebar_row_gap;
            let step = row_h.saturating_add(row_gap).max(1);

            let header_h = cell
                .height
                .get()
                .saturating_add(metrics.sidebar_row_gap.saturating_mul(2))
                .max(metrics.button_size);
            let header_y = self.height.saturating_add(metrics.sidebar_pad_top);
            if let (Some(w_nz), Some(h_nz)) = (NonZeroU32::new(inner_w), NonZeroU32::new(header_h))
            {
                self.sidebar_header_rect = Some(PixelRect {
                    origin: PixelPosition {
                        x: row_pad_x,
                        y: header_y,
                    },
                    size: PixelSize {
                        width: w_nz,
                        height: h_nz,
                    },
                });
                let plus_side = metrics.button_size.min(header_h);
                if let Some(side) = NonZeroU32::new(plus_side)
                    && inner_w >= plus_side.saturating_mul(2)
                {
                    self.sidebar_header_plus = Some(PixelRect {
                        origin: PixelPosition {
                            x: row_pad_x + inner_w.saturating_sub(plus_side),
                            y: header_y + header_h.saturating_sub(plus_side) / 2,
                        },
                        size: PixelSize {
                            width: side,
                            height: side,
                        },
                    });
                }
            }

            let list_top = header_y
                .saturating_add(header_h)
                .saturating_add(metrics.sidebar_row_gap);
            let footer_h = row_h;
            let footer_pad_y = metrics.sidebar_pad_top;
            let footer_reserve = footer_h.saturating_add(footer_pad_y.saturating_mul(2));
            let has_footer = size.height
                >= list_top
                    .saturating_add(step * 2)
                    .saturating_add(footer_reserve);
            let list_bottom = if has_footer {
                let footer_y = size
                    .height
                    .saturating_sub(footer_h.saturating_add(footer_pad_y));
                if let (Some(w_nz), Some(h_nz)) =
                    (NonZeroU32::new(inner_w), NonZeroU32::new(footer_h))
                {
                    self.sidebar_footer_rect = Some(PixelRect {
                        origin: PixelPosition {
                            x: row_pad_x,
                            y: footer_y,
                        },
                        size: PixelSize {
                            width: w_nz,
                            height: h_nz,
                        },
                    });
                }
                footer_y.saturating_sub(footer_pad_y)
            } else {
                size.height.saturating_sub(metrics.sidebar_pad_top)
            };

            let avail_h = list_bottom.saturating_sub(list_top);
            let visible = ((avail_h.saturating_add(row_gap)) / step).max(1) as usize;
            self.sidebar_list_top = list_top;
            self.sidebar_list_bottom = list_bottom;
            self.sidebar_row_height = row_h;
            self.sidebar_row_gap = row_gap;
            self.sidebar_visible_rows = visible;

            let max_scroll = tabs.saturating_sub(visible);
            self.sidebar_scroll = self.sidebar_scroll.min(max_scroll);
            if self.last_active != Some(active) {
                if active < self.sidebar_scroll {
                    self.sidebar_scroll = active;
                } else if active >= self.sidebar_scroll + visible {
                    self.sidebar_scroll = active + 1 - visible;
                }
                self.last_active = Some(active);
            }
            self.sidebar_scroll = self.sidebar_scroll.min(max_scroll);

            let close_size = metrics.close_size.min(row_h.saturating_sub(4).max(1));
            let close_inset = metrics.item_gap;
            let show_close = tabs > 1 && inner_w >= close_size.saturating_mul(4);
            if let (Some(w_nz), Some(h_nz)) = (NonZeroU32::new(inner_w), NonZeroU32::new(row_h)) {
                for (vis_idx, index) in (self.sidebar_scroll..tabs).take(visible).enumerate() {
                    let y = list_top.saturating_add(step.saturating_mul(vis_idx as u32));
                    let close_rect = if show_close {
                        NonZeroU32::new(close_size).map(|side| PixelRect {
                            origin: PixelPosition {
                                x: row_pad_x + inner_w.saturating_sub(close_size + close_inset),
                                y: y + row_h.saturating_sub(close_size) / 2,
                            },
                            size: PixelSize {
                                width: side,
                                height: side,
                            },
                        })
                    } else {
                        None
                    };
                    self.sidebar_tabs.push(TabSlot {
                        id: tab_id(index),
                        index,
                        rect: PixelRect {
                            origin: PixelPosition { x: row_pad_x, y },
                            size: PixelSize {
                                width: w_nz,
                                height: h_nz,
                            },
                        },
                        close_rect,
                    });
                }
            }
            return;
        }

        // Top Chrome tab mode: Logo followed directly by Chrome tabs and `+` (no sidebar toggle).
        let drag_gap = if window_buttons.any() {
            metrics.drag_gap_with_controls
        } else {
            metrics.drag_gap_plain
        };
        let tab_gap = metrics.top_tab_gap;
        let plus_gap = metrics.item_gap;
        let plus_reserve = btn_size.saturating_add(plus_gap);
        let tabs_right = right_limit
            .saturating_sub(drag_gap)
            .saturating_sub(plus_reserve);
        let tab_top = metrics.top_tab_top;
        let tab_h = self
            .height
            .saturating_sub(tab_top)
            .max(cell.height.get().max(1));

        self.top_strip_left = left_x;
        self.top_strip_right = tabs_right.max(left_x);
        self.top_tab_gap = tab_gap;
        self.top_tab_width = 0;

        if tabs > 0 && tabs_right > left_x {
            let avail = tabs_right - left_x;
            let total_gaps = tab_gap.saturating_mul(tabs.saturating_sub(1) as u32);
            let min_w = metrics.top_tab_min_width;
            let max_w = metrics.top_tab_max_width;
            let raw_w = avail.saturating_sub(total_gaps) / tabs as u32;
            let tab_w = raw_w.clamp(min_w.min(avail), max_w).min(avail);
            self.top_tab_width = tab_w;
            let close_size = metrics.close_size.min(tab_h.saturating_sub(4).max(1));
            let close_inset = metrics.item_gap;
            let show_close = tabs > 1 && tab_w >= close_size.saturating_mul(4);

            for index in 0..tabs {
                let x = left_x.saturating_add(index as u32 * (tab_w + tab_gap));
                if x.saturating_add(tab_w) > tabs_right.saturating_add(1) {
                    break;
                }
                let (Some(w_nz), Some(h_nz)) = (NonZeroU32::new(tab_w), NonZeroU32::new(tab_h))
                else {
                    break;
                };
                let close_rect = if show_close {
                    NonZeroU32::new(close_size).map(|side| PixelRect {
                        origin: PixelPosition {
                            x: x + tab_w.saturating_sub(close_size + close_inset),
                            y: tab_top + tab_h.saturating_sub(close_size) / 2,
                        },
                        size: PixelSize {
                            width: side,
                            height: side,
                        },
                    })
                } else {
                    None
                };
                self.top_tabs.push(TabSlot {
                    id: tab_id(index),
                    index,
                    rect: PixelRect {
                        origin: PixelPosition { x, y: tab_top },
                        size: PixelSize {
                            width: w_nz,
                            height: h_nz,
                        },
                    },
                    close_rect,
                });
            }
        }

        // `+` New Tab button right after the last rendered Chrome tab.
        let plus_x = self
            .top_tabs
            .last()
            .map(|slot| slot.rect.origin.x + slot.rect.size.width.get() + plus_gap)
            .unwrap_or(left_x);
        if plus_x.saturating_add(btn_size) <= right_limit
            && let Some(side) = NonZeroU32::new(btn_size)
        {
            self.buttons.push(ButtonSlot {
                action: TitlebarAction::NewTab,
                rect: PixelRect {
                    origin: PixelPosition {
                        x: plus_x,
                        y: btn_y,
                    },
                    size: PixelSize {
                        width: side,
                        height: side,
                    },
                },
                circular: false,
            });
        }
    }

    pub(super) fn tab_targets(&self) -> Vec<(u64, f32)> {
        if self.sidebar_width > 0 {
            self.sidebar_tabs
                .iter()
                .map(|slot| (slot.id, slot.rect.origin.y as f32))
                .collect()
        } else {
            self.top_tabs
                .iter()
                .map(|slot| (slot.id, slot.rect.origin.x as f32))
                .collect()
        }
    }

    pub(super) fn insertion_index_top(&self, drag_x: f32, tab_count: usize) -> usize {
        if tab_count <= 1 || self.top_tab_width == 0 {
            return 0;
        }
        let step = (self.top_tab_width + self.top_tab_gap) as f32;
        let center_x = drag_x + self.top_tab_width as f32 * 0.5;
        let slot = ((center_x - self.top_strip_left as f32) / step).floor() as isize;
        slot.clamp(0, tab_count.saturating_sub(1) as isize) as usize
    }

    pub(super) fn clamp_top_drag_x(&self, raw_x: f32) -> f32 {
        let min_x = self.top_strip_left as f32;
        let max_x = self
            .top_tabs
            .last()
            .map_or(min_x, |last| last.rect.origin.x as f32)
            .max(min_x);
        raw_x.clamp(min_x, max_x)
    }

    pub(super) fn insertion_index_sidebar(&self, drag_y: f32, tab_count: usize) -> usize {
        if tab_count <= 1 || self.sidebar_row_height == 0 {
            return 0;
        }
        let step = (self.sidebar_row_height + self.sidebar_row_gap) as f32;
        let center_y = drag_y + self.sidebar_row_height as f32 * 0.5;
        let rel = ((center_y - self.sidebar_list_top as f32) / step).floor() as isize;
        let abs = self.sidebar_scroll as isize + rel;
        abs.clamp(0, tab_count.saturating_sub(1) as isize) as usize
    }

    pub(super) fn clamp_sidebar_drag_y(&self, raw_y: f32) -> f32 {
        let min_y = self.sidebar_list_top as f32;
        let max_y = self
            .sidebar_tabs
            .last()
            .map_or(min_y, |last| last.rect.origin.y as f32)
            .max(min_y);
        raw_y.clamp(min_y, max_y)
    }

    pub(super) fn scroll_sidebar(&mut self, delta: isize, total_tabs: usize) -> bool {
        let max_scroll = total_tabs.saturating_sub(self.sidebar_visible_rows.max(1));
        let next = (self.sidebar_scroll as isize + delta).clamp(0, max_scroll as isize) as usize;
        if next == self.sidebar_scroll {
            return false;
        }
        self.sidebar_scroll = next;
        true
    }

    pub(super) fn contains_titlebar(&self, pos: PhysicalPosition<f64>) -> bool {
        self.height != 0
            && self.header_width != 0
            && pos.x.is_finite()
            && pos.y.is_finite()
            && pos.x >= 0.0
            && pos.x < f64::from(self.header_width)
            && pos.y >= 0.0
            && pos.y < f64::from(self.height)
    }

    pub(super) fn contains_sidebar(&self, pos: PhysicalPosition<f64>) -> bool {
        self.sidebar_width != 0
            && pos.x.is_finite()
            && pos.y.is_finite()
            && pos.x >= 0.0
            && pos.x < f64::from(self.sidebar_width)
            && pos.y >= f64::from(self.height)
            && pos.y < f64::from(self.window_height)
    }

    pub(super) fn contains_any(&self, pos: PhysicalPosition<f64>) -> bool {
        self.contains_titlebar(pos) || self.contains_sidebar(pos)
    }

    pub(super) fn hit_sidebar_divider(&self, pos: PhysicalPosition<f64>, scale: f64) -> bool {
        if self.sidebar_width == 0 || !pos.x.is_finite() || !pos.y.is_finite() {
            return false;
        }
        let grab = (5.0 * scale.max(1.0)).max(4.0);
        let edge = f64::from(self.sidebar_width);
        (pos.x - edge).abs() <= grab
            && pos.y >= f64::from(self.content_top_inset)
            && pos.y < f64::from(self.window_height)
    }

    pub(super) fn hit(&self, pos: PhysicalPosition<f64>) -> Option<TitlebarAction> {
        if self.contains_titlebar(pos) {
            if let Some(bar) = self.command_bar
                && rect_contains(bar, pos)
            {
                return Some(TitlebarAction::CommandBar);
            }
            for button in &self.buttons {
                if rect_contains(button.rect, pos) {
                    return Some(button.action);
                }
            }
            for tab in &self.top_tabs {
                if let Some(close) = tab.close_rect
                    && rect_contains(close, pos)
                {
                    return Some(TitlebarAction::CloseTab(tab.index));
                }
                if rect_contains(tab.rect, pos) {
                    return Some(TitlebarAction::SelectTab(tab.index));
                }
            }
            return None;
        }
        if self.contains_sidebar(pos) {
            if let Some(plus) = self.sidebar_header_plus
                && rect_contains(plus, pos)
            {
                return Some(TitlebarAction::NewTab);
            }
            if let Some(footer) = self.sidebar_footer_rect
                && rect_contains(footer, pos)
            {
                return Some(TitlebarAction::SidebarNewTab);
            }
            for tab in &self.sidebar_tabs {
                if let Some(close) = tab.close_rect
                    && rect_contains(close, pos)
                {
                    return Some(TitlebarAction::CloseTab(tab.index));
                }
                if rect_contains(tab.rect, pos) {
                    return Some(TitlebarAction::SelectTab(tab.index));
                }
            }
        }
        None
    }

    pub(super) fn top_tab_slot(&self, index: usize) -> Option<TabSlot> {
        self.top_tabs.iter().copied().find(|s| s.index == index)
    }

    pub(super) fn sidebar_tab_slot(&self, index: usize) -> Option<TabSlot> {
        self.sidebar_tabs.iter().copied().find(|s| s.index == index)
    }
}

/// Tail-truncates long paths or titles (`…/dir/name`) so the rightmost segment stays visible.
pub(super) fn title_tail(title: &str, max_cells: u32) -> std::borrow::Cow<'_, str> {
    if max_cells == 0 {
        return std::borrow::Cow::Borrowed("");
    }
    let total: u32 = title
        .graphemes(true)
        .map(|g| WidthPolicy::Narrow.columns(g) as u32)
        .sum();
    if total <= max_cells {
        return std::borrow::Cow::Borrowed(title);
    }
    if max_cells == 1 {
        return std::borrow::Cow::Borrowed("…");
    }
    let mut used = 1u32;
    let mut start = title.len();
    for (offset, grapheme) in title.grapheme_indices(true).rev() {
        let cells = WidthPolicy::Narrow.columns(grapheme) as u32;
        if used.saturating_add(cells) > max_cells {
            break;
        }
        used = used.saturating_add(cells);
        start = offset;
    }
    std::borrow::Cow::Owned(format!("…{}", &title[start..]))
}

/// Detects outer window edges for borderless Wayland mouse resizing.
pub(super) fn edge_resize_direction(
    pos: PhysicalPosition<f64>,
    size: PhysicalSize<u32>,
    scale: f64,
) -> Option<ResizeDirection> {
    if !pos.x.is_finite() || !pos.y.is_finite() || size.width == 0 || size.height == 0 {
        return None;
    }
    let border = (6.0 * scale.max(1.0)).max(5.0);
    let w = f64::from(size.width);
    let h = f64::from(size.height);
    let left = pos.x < border;
    let right = pos.x >= w - border;
    let top = pos.y < border;
    let bottom = pos.y >= h - border;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(ResizeDirection::NorthWest),
        (_, true, true, _) => Some(ResizeDirection::NorthEast),
        (true, _, _, true) => Some(ResizeDirection::SouthWest),
        (_, true, _, true) => Some(ResizeDirection::SouthEast),
        (true, _, _, _) => Some(ResizeDirection::West),
        (_, true, _, _) => Some(ResizeDirection::East),
        (_, _, true, _) => Some(ResizeDirection::North),
        (_, _, _, true) => Some(ResizeDirection::South),
        _ => None,
    }
}

fn rect_contains(rect: PixelRect, pos: PhysicalPosition<f64>) -> bool {
    let x = f64::from(rect.origin.x);
    let y = f64::from(rect.origin.y);
    pos.x >= x
        && pos.x < x + f64::from(rect.size.width.get())
        && pos.y >= y
        && pos.y < y + f64::from(rect.size.height.get())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use lune_terminal::PixelSize;
    use winit::dpi::{PhysicalPosition, PhysicalSize};
    use winit::window::ResizeDirection;

    use super::super::config::TabPosition;
    use super::super::wayland_decorations::WindowButtons;
    use super::{
        Layout, ReflowParams, SpringSlot, TitlebarAction, TitlebarMetrics, edge_resize_direction,
        title_tail,
    };

    fn test_cell(w: u32, h: u32) -> PixelSize {
        PixelSize {
            width: NonZeroU32::new(w).unwrap(),
            height: NonZeroU32::new(h).unwrap(),
        }
    }

    #[test]
    fn reflow_lays_out_top_chrome_tabs_and_window_controls_without_overlap() {
        let mut layout = Layout::default();
        let size = PhysicalSize::new(960, 600);
        let cell = test_cell(9, 20);
        layout.reflow(
            ReflowParams {
                size,
                cell,
                scale: 1.0,
                enabled: true,
                tab_position: TabPosition::Top,
                sidebar_collapsed: false,
                sidebar_width_cells: None,
                window_buttons: WindowButtons::ALL,
                tabs: 2,
                active: 0,
            },
            |idx| (idx + 1) as u64,
        );
        assert!(layout.height >= cell.height.get() + 12);
        assert_eq!(layout.sidebar_width, 0);
        assert!(layout.command_bar.is_none());
        assert!(
            !layout
                .buttons
                .iter()
                .any(|b| b.action == TitlebarAction::ToggleSidebar)
        );
        assert_eq!(layout.top_tabs.len(), 2);

        let logo = layout
            .buttons
            .iter()
            .find(|b| b.action == TitlebarAction::CommandPalette)
            .unwrap();
        let plus = layout
            .buttons
            .iter()
            .find(|b| b.action == TitlebarAction::NewTab)
            .unwrap();
        let min_btn = layout
            .buttons
            .iter()
            .find(|b| b.action == TitlebarAction::Minimize)
            .unwrap();

        assert!(logo.rect.origin.x + logo.rect.size.width.get() < layout.top_tabs[0].rect.origin.x);
        assert!(
            layout.top_tabs[1].rect.origin.x + layout.top_tabs[1].rect.size.width.get()
                < plus.rect.origin.x
        );
        assert!(plus.rect.origin.x + plus.rect.size.width.get() < min_btn.rect.origin.x);
        assert_eq!(
            layout.insertion_index_top(layout.top_tabs[1].rect.origin.x as f32, 2),
            1
        );
    }

    #[test]
    fn sidebar_mode_renders_command_bar_and_collapsible_sidebar() {
        let mut layout = Layout::default();
        let size = PhysicalSize::new(960, 600);
        let cell = test_cell(9, 20);
        layout.reflow(
            ReflowParams {
                size,
                cell,
                scale: 1.0,
                enabled: true,
                tab_position: TabPosition::Left,
                sidebar_collapsed: false,
                sidebar_width_cells: None,
                window_buttons: WindowButtons::NONE,
                tabs: 3,
                active: 1,
            },
            |idx| (idx + 1) as u64,
        );
        let expected_w = layout.metrics.sidebar_default_width;
        assert_eq!(layout.sidebar_width, expected_w);
        assert_eq!(layout.header_width, 960);
        assert_eq!(layout.content_top_inset, layout.height);
        assert!(layout.command_bar.is_some());
        assert!(layout.sidebar_header_rect.is_some());
        assert!(layout.sidebar_footer_rect.is_some());
        assert!(layout.top_tabs.is_empty());
        assert_eq!(layout.sidebar_tabs.len(), 3);
        assert!(
            layout.hit_sidebar_divider(PhysicalPosition::new(f64::from(expected_w), 200.0), 1.0)
        );
        assert!(!layout.hit_sidebar_divider(PhysicalPosition::new(100.0, 200.0), 1.0));

        let bar = layout.command_bar.unwrap();
        assert_eq!(
            layout.hit(PhysicalPosition::new(
                f64::from(bar.origin.x + 20),
                f64::from(bar.origin.y + 8),
            )),
            Some(TitlebarAction::CommandBar)
        );

        let tab1 = layout.sidebar_tabs[1];
        assert_eq!(
            layout.hit(PhysicalPosition::new(
                f64::from(tab1.rect.origin.x + 12),
                f64::from(tab1.rect.origin.y + 8),
            )),
            Some(TitlebarAction::SelectTab(1))
        );
        assert_eq!(title_tail("/very/long/path/lune", 10), "…path/lune");

        layout.reflow(
            ReflowParams {
                size,
                cell,
                scale: 1.0,
                enabled: true,
                tab_position: TabPosition::Left,
                sidebar_collapsed: true,
                sidebar_width_cells: None,
                window_buttons: WindowButtons::NONE,
                tabs: 3,
                active: 1,
            },
            |idx| (idx + 1) as u64,
        );
        assert_eq!(layout.sidebar_width, 0);
        assert!(layout.command_bar.is_some());
    }

    #[test]
    fn metrics_adapt_to_font_size_and_window_dimensions() {
        let small = TitlebarMetrics::compute(PhysicalSize::new(960, 600), test_cell(7, 15), 1.0);
        let large = TitlebarMetrics::compute(PhysicalSize::new(1440, 900), test_cell(15, 32), 1.0);
        assert!(large.titlebar_height > small.titlebar_height);
        assert!(large.sidebar_row_height > 32);
        assert!(large.sidebar_default_width > small.sidebar_default_width);
        assert!(large.top_tab_max_width > small.top_tab_max_width);

        // Narrow windows cap sidebar width to 40% of the window.
        let narrow = TitlebarMetrics::compute(PhysicalSize::new(400, 600), test_cell(9, 20), 1.0);
        assert!(narrow.sidebar_max_width <= 160);
    }

    #[test]
    fn critically_damped_spring_settles_monotonically_without_overshoot() {
        let mut slot = SpringSlot::settled(0.0);
        slot.target = 200.0;
        let mut steps = 0;
        while slot.step(1.0 / 60.0) {
            assert!(slot.current <= 200.01);
            steps += 1;
            assert!(steps < 60);
        }
        assert_eq!(slot.current, 200.0);
        assert_eq!(slot.velocity, 0.0);
    }

    #[test]
    fn edge_resize_detects_corners_and_sides() {
        let size = PhysicalSize::new(800, 600);
        assert_eq!(
            edge_resize_direction(PhysicalPosition::new(2.0, 2.0), size, 1.0),
            Some(ResizeDirection::NorthWest)
        );
        assert_eq!(
            edge_resize_direction(PhysicalPosition::new(798.0, 598.0), size, 1.0),
            Some(ResizeDirection::SouthEast)
        );
        assert_eq!(
            edge_resize_direction(PhysicalPosition::new(400.0, 300.0), size, 1.0),
            None
        );
    }
}
