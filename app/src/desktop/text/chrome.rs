//! Modern inline titlebar, spring-animated top tabs, left tab sidebar,
//! and Classic tab bar rendering.

use std::num::NonZeroU32;
use std::ops::Range;

use lune_terminal::{PaletteEntry, PixelPosition, PixelSize, Progress, Rgb};

use crate::PixelRect;

use super::super::svg;
use super::super::tab_bar;
use super::super::titlebar::{self, TitlebarAction};
use super::{ATLAS_SIDE, CHROME_LABEL_CACHE, TAB_LABEL_CACHE, TextRenderer, luminance, mix_color};

impl TextRenderer {
    pub(in super::super) fn prepare_titlebar(
        &mut self,
        view: &titlebar::View<'_>,
        progress_phase: f32,
    ) -> Range<u32> {
        let start = self.instances.len() as u32;
        let layout = view.layout;
        let (Some(width), Some(height)) = (
            NonZeroU32::new(layout.header_width),
            NonZeroU32::new(layout.height),
        ) else {
            return start..start;
        };
        let area = PixelSize { width, height };
        let prev_area = std::mem::replace(&mut self.area, area);

        let scale = view.scale.max(1.0);
        let px = |logical: f64| (logical * scale).round().max(1.0) as u32;
        let bg = view.palette.color(PaletteEntry::Background);
        let fg = view.palette.color(PaletteEntry::Foreground);
        let accent = view.palette.color(PaletteEntry::Indexed(4));
        let red = view.palette.color(PaletteEntry::Indexed(1));
        let muted = mix_color(bg, fg, 150);
        let bar_bg = mix_color(bg, fg, 7);
        let bar_alpha = if view.background_opacity >= 1.0 {
            0.94
        } else {
            (view.background_opacity * 0.92).max(0.58)
        };

        // Header strip background + subtle bottom hairline.
        self.rect(
            PixelRect {
                origin: PixelPosition { x: 0, y: 0 },
                size: area,
            },
            bar_bg,
            bar_alpha,
        );
        self.rect(
            PixelRect {
                origin: PixelPosition {
                    x: 0,
                    y: layout.height.saturating_sub(1),
                },
                size: PixelSize {
                    width,
                    height: NonZeroU32::new(1).unwrap(),
                },
            },
            mix_color(bg, fg, 34),
            0.55,
        );

        // Centered Command Bar search pill (active in Left Sidebar layout).
        if let Some(bar_rect) = layout.command_bar {
            let m = layout.metrics;
            let bar_hovered = view.hovered == Some(TitlebarAction::CommandBar);
            let bar_h = bar_rect.size.height.get();
            let bar_w = bar_rect.size.width.get();
            let radius = (bar_h.saturating_mul(36) / 100).clamp(6, bar_h / 2);
            let fill = if bar_hovered {
                mix_color(bg, fg, 20)
            } else {
                mix_color(bg, fg, 13)
            };
            let border = if bar_hovered {
                mix_color(bg, accent, 120)
            } else {
                mix_color(bg, fg, 38)
            };
            self.rounded_rect(
                bar_rect,
                radius,
                0,
                fill,
                if bar_hovered { 0.96 } else { 0.90 },
            );
            self.rounded_rect(
                bar_rect,
                radius,
                1,
                border,
                if bar_hovered { 0.85 } else { 0.65 },
            );

            let pad_x = m.edge_padding.max(8);
            let search_size = m.tab_icon_size.min(bar_h.saturating_sub(8).max(10));
            let icon_x = bar_rect.origin.x + pad_x;
            let icon_y = bar_rect.origin.y + bar_h.saturating_sub(search_size) / 2;
            self.svg_mask(
                PixelPosition {
                    x: icon_x,
                    y: icon_y,
                },
                search_size,
                svg::icons::SEARCH,
                if bar_hovered { accent } else { muted },
                if bar_hovered { 0.95 } else { 0.8 },
            );

            let ui_metrics = self.chrome_metrics;
            let cell_h = ui_metrics.cell.height.get();
            let mut right_edge = bar_rect.origin.x + bar_w.saturating_sub(pad_x);
            if !view.palette_shortcut.is_empty() {
                let key_w = self.font.measure_text(ui_metrics, view.palette_shortcut);
                let key_pad_x = m.item_gap.max(5);
                let key_pad_y = (bar_h.saturating_sub(cell_h) / 4).max(2);
                let pill_w = key_w.saturating_add(key_pad_x * 2);
                let pill_h = cell_h
                    .saturating_add(key_pad_y * 2)
                    .min(bar_h.saturating_sub(4));
                let text_start = icon_x + search_size + m.item_gap;
                if pill_w > 0
                    && bar_w
                        > pill_w
                            .saturating_add(pad_x * 3)
                            .saturating_add(search_size * 4)
                    && right_edge > text_start + pill_w + m.edge_padding
                    && let (Some(pw), Some(ph)) = (NonZeroU32::new(pill_w), NonZeroU32::new(pill_h))
                {
                    let pill_x = right_edge.saturating_sub(pill_w);
                    let pill_y = bar_rect.origin.y + bar_h.saturating_sub(pill_h) / 2;
                    let pill_rect = PixelRect {
                        origin: PixelPosition {
                            x: pill_x,
                            y: pill_y,
                        },
                        size: PixelSize {
                            width: pw,
                            height: ph,
                        },
                    };
                    let pill_r = (pill_h / 4).clamp(4, pill_h / 2);
                    self.rounded_rect(pill_rect, pill_r, 0, mix_color(bg, fg, 22), 0.72);
                    self.rounded_rect(pill_rect, pill_r, 1, mix_color(bg, fg, 48), 0.45);
                    if let Some(kw) = NonZeroU32::new(key_w.min(ATLAS_SIDE as u32)) {
                        self.glyph(
                            PixelRect {
                                origin: PixelPosition {
                                    x: pill_x + key_pad_x,
                                    y: pill_y + pill_h.saturating_sub(cell_h) / 2,
                                },
                                size: PixelSize {
                                    width: kw,
                                    height: ui_metrics.cell.height,
                                },
                            },
                            view.palette_shortcut,
                            CHROME_LABEL_CACHE,
                            if bar_hovered { fg } else { muted },
                            0.85,
                        );
                    }
                    right_edge = pill_x.saturating_sub(m.item_gap);
                }
            }

            let prompt_x = icon_x + search_size + m.item_gap;
            let prompt_y = bar_rect.origin.y + bar_h.saturating_sub(cell_h) / 2;
            let prompt_avail = right_edge.saturating_sub(prompt_x);
            if prompt_avail > 0 {
                self.clamped_chrome_text(
                    PixelPosition {
                        x: prompt_x,
                        y: prompt_y,
                    },
                    prompt_avail,
                    "Search commands...",
                    if bar_hovered { fg } else { muted },
                );
            }
        }

        // Chrome-style inline top tabs (inactive first, active/dragged on top with concave wings).
        let m = layout.metrics;
        let tab_radius = m.top_tab_radius;
        let wing_w = m.wing_width;
        let wing_h = m.wing_height;
        let term_icon_size = m.tab_icon_size;
        let close_icon_size = m.close_icon_size;
        let cell_h = self.chrome_metrics.cell.height.get();
        let active_tab_fill = mix_color(bg, fg, 20);

        let mut draw_order: Vec<usize> = (0..layout.top_tabs.len()).collect();
        draw_order.sort_by_key(|&i| {
            let slot = layout.top_tabs[i];
            if view.dragged_tab == Some(slot.id) {
                2u8
            } else if slot.index == view.active {
                1u8
            } else {
                0u8
            }
        });

        let mut rightmost_tab_x = layout.top_strip_left;
        for &slot_idx in &draw_order {
            let slot = layout.top_tabs[slot_idx];
            let Some(tab) = view.tabs.get(slot.index) else {
                continue;
            };
            let active = slot.index == view.active;
            let dragged = view.dragged_tab == Some(slot.id);
            let anim_x = view
                .animator
                .coord(slot.id, slot.rect.origin.x as f32)
                .round()
                .max(0.0) as u32;
            let mut rect = slot.rect;
            rect.origin.x = anim_x;
            rightmost_tab_x = rightmost_tab_x.max(rect.origin.x + rect.size.width.get());

            let tab_hovered = matches!(
                view.hovered,
                Some(TitlebarAction::SelectTab(i) | TitlebarAction::CloseTab(i))
                    if i == slot.index
            );

            if active || dragged {
                let active_alpha = if view.background_opacity >= 1.0 {
                    0.98
                } else {
                    (view.background_opacity * 0.96).max(0.72)
                };
                // Extend bottom by `tab_radius` (clipped by titlebar scissor) for flat bottom fusion.
                let body_h = rect.size.height.get().saturating_add(tab_radius);
                if let Some(bh) = NonZeroU32::new(body_h) {
                    self.rounded_rect(
                        PixelRect {
                            origin: rect.origin,
                            size: PixelSize {
                                width: rect.size.width,
                                height: bh,
                            },
                        },
                        tab_radius,
                        0,
                        active_tab_fill,
                        active_alpha,
                    );
                }
                if rect.origin.x >= wing_w
                    && let (Some(ww), Some(wh)) = (NonZeroU32::new(wing_w), NonZeroU32::new(wing_h))
                {
                    self.concave_wing(
                        PixelRect {
                            origin: PixelPosition {
                                x: rect.origin.x - wing_w,
                                y: layout.height.saturating_sub(wing_h),
                            },
                            size: PixelSize {
                                width: ww,
                                height: wh,
                            },
                        },
                        false,
                        active_tab_fill,
                        active_alpha,
                    );
                }
                let right_x = rect.origin.x + rect.size.width.get();
                if right_x + wing_w <= layout.width
                    && let (Some(ww), Some(wh)) = (NonZeroU32::new(wing_w), NonZeroU32::new(wing_h))
                {
                    self.concave_wing(
                        PixelRect {
                            origin: PixelPosition {
                                x: right_x,
                                y: layout.height.saturating_sub(wing_h),
                            },
                            size: PixelSize {
                                width: ww,
                                height: wh,
                            },
                        },
                        true,
                        active_tab_fill,
                        active_alpha,
                    );
                }
            } else {
                if tab_hovered {
                    let inset_y = px(3.0);
                    if let Some(h) =
                        NonZeroU32::new(rect.size.height.get().saturating_sub(inset_y * 2))
                    {
                        self.rounded_rect(
                            PixelRect {
                                origin: PixelPosition {
                                    x: rect.origin.x + px(2.0),
                                    y: rect.origin.y + inset_y,
                                },
                                size: PixelSize {
                                    width: NonZeroU32::new(
                                        rect.size.width.get().saturating_sub(px(4.0)).max(1),
                                    )
                                    .unwrap(),
                                    height: h,
                                },
                            },
                            px(7.0),
                            0,
                            mix_color(bg, fg, 18),
                            0.72,
                        );
                    }
                }
                let next_is_active_or_hovered =
                    layout.top_tabs.get(slot_idx + 1).is_some_and(|next| {
                        next.index == view.active
                            || view.dragged_tab == Some(next.id)
                            || matches!(
                                view.hovered,
                                Some(TitlebarAction::SelectTab(i) | TitlebarAction::CloseTab(i))
                                    if i == next.index
                            )
                    });
                if !tab_hovered
                    && !next_is_active_or_hovered
                    && slot_idx + 1 < layout.top_tabs.len()
                {
                    let sep_h = rect.size.height.get() / 2;
                    if let Some(sh) = NonZeroU32::new(sep_h) {
                        self.rect(
                            PixelRect {
                                origin: PixelPosition {
                                    x: rect.origin.x + rect.size.width.get(),
                                    y: rect.origin.y
                                        + rect.size.height.get().saturating_sub(sep_h) / 2,
                                },
                                size: PixelSize {
                                    width: NonZeroU32::new(1).unwrap(),
                                    height: sh,
                                },
                            },
                            mix_color(bg, fg, 55),
                            0.55,
                        );
                    }
                }
            }

            let icon_x = rect.origin.x + m.edge_padding;
            let icon_y = rect.origin.y + rect.size.height.get().saturating_sub(term_icon_size) / 2;
            let icon_color = if active || dragged { accent } else { muted };
            self.svg_mask(
                PixelPosition {
                    x: icon_x,
                    y: icon_y,
                },
                term_icon_size,
                svg::icons::TERMINAL,
                icon_color,
                if active || dragged { 0.95 } else { 0.72 },
            );

            let shifted_close = slot.close_rect.map(|mut cr| {
                let rel_x = cr.origin.x.saturating_sub(slot.rect.origin.x);
                cr.origin.x = rect.origin.x + rel_x;
                cr
            });
            let title_x = icon_x + term_icon_size + m.item_gap;
            let title_y = rect.origin.y + rect.size.height.get().saturating_sub(cell_h) / 2;
            let right_edge = shifted_close
                .map(|r| r.origin.x.saturating_sub(m.control_gap))
                .unwrap_or_else(|| {
                    rect.origin.x + rect.size.width.get().saturating_sub(m.edge_padding)
                });
            let title_avail = right_edge.saturating_sub(title_x);
            if title_avail > 0 {
                let text_color = if active || dragged { fg } else { muted };
                self.clamped_chrome_text(
                    PixelPosition {
                        x: title_x,
                        y: title_y,
                    },
                    title_avail,
                    tab.label(),
                    text_color,
                );
            }

            if let Some(close_rect) = shifted_close
                && (active || tab_hovered)
            {
                let close_hovered = view.hovered == Some(TitlebarAction::CloseTab(slot.index));
                if close_hovered {
                    let r = close_rect.size.width.get() / 2;
                    self.rounded_rect(close_rect, r, 0, mix_color(bg, fg, 48), 0.85);
                }
                let cx = close_rect.origin.x
                    + close_rect.size.width.get().saturating_sub(close_icon_size) / 2;
                let cy = close_rect.origin.y
                    + close_rect.size.height.get().saturating_sub(close_icon_size) / 2;
                self.svg_mask(
                    PixelPosition { x: cx, y: cy },
                    close_icon_size,
                    svg::icons::CLOSE,
                    if close_hovered { fg } else { muted },
                    0.9,
                );
            }

            self.tab_progress_strip(
                rect,
                tab.progress(),
                view.palette,
                progress_phase,
                m.item_gap,
            );
        }

        // Toolbar buttons (App Logo, Sidebar Toggle, New Tab `+`, and right circular Window Controls).
        for button in &layout.buttons {
            let mut btn_rect = button.rect;
            if layout.sidebar_width == 0
                && button.action == TitlebarAction::NewTab
                && !layout.top_tabs.is_empty()
            {
                btn_rect.origin.x = rightmost_tab_x + m.item_gap;
            }
            let hovered = view.hovered == Some(button.action);
            let btn_bg = bar_bg;
            if button.circular {
                let r = btn_rect.size.width.get() / 2;
                let (fill, alpha) = if hovered && button.action == TitlebarAction::Close {
                    (red, 0.85)
                } else if hovered {
                    (mix_color(btn_bg, fg, 42), 0.90)
                } else {
                    (mix_color(btn_bg, fg, 18), 0.72)
                };
                self.rounded_rect(btn_rect, r, 0, fill, alpha);
            } else if hovered {
                let r = (btn_rect.size.height.get() / 4).min(btn_rect.size.height.get() / 2);
                self.rounded_rect(btn_rect, r, 0, mix_color(btn_bg, fg, 26), 0.85);
            }

            let btn_side = btn_rect.size.width.get().min(btn_rect.size.height.get());
            let icon_size = match button.action {
                TitlebarAction::CommandPalette => (btn_side.saturating_mul(72) / 100).max(12),
                TitlebarAction::NewTab | TitlebarAction::ToggleSidebar => {
                    (btn_side.saturating_mul(55) / 100).max(10)
                }
                _ => (btn_side.saturating_mul(46) / 100).max(8),
            }
            .min(btn_side);
            let ix = btn_rect.origin.x + btn_rect.size.width.get().saturating_sub(icon_size) / 2;
            let iy = btn_rect.origin.y + btn_rect.size.height.get().saturating_sub(icon_size) / 2;
            let origin = PixelPosition { x: ix, y: iy };

            match button.action {
                TitlebarAction::CommandPalette => {
                    self.svg_color(origin, icon_size, svg::icons::APP_LOGO, 1.0);
                }
                TitlebarAction::ToggleSidebar => {
                    let expanded = !layout.sidebar_collapsed && layout.sidebar_width > 0;
                    self.svg_mask(
                        origin,
                        icon_size,
                        svg::icons::SIDEBAR,
                        if expanded {
                            accent
                        } else if hovered {
                            fg
                        } else {
                            muted
                        },
                        0.95,
                    );
                }
                TitlebarAction::NewTab => {
                    self.svg_mask(
                        origin,
                        icon_size,
                        svg::icons::PLUS,
                        if hovered { fg } else { muted },
                        0.95,
                    );
                }
                TitlebarAction::Minimize => {
                    self.svg_mask(
                        origin,
                        icon_size,
                        svg::icons::MINIMIZE,
                        if hovered { fg } else { muted },
                        0.95,
                    );
                }
                TitlebarAction::Maximize => {
                    let svg = if view.maximized {
                        svg::icons::RESTORE
                    } else {
                        svg::icons::MAXIMIZE
                    };
                    self.svg_mask(
                        origin,
                        icon_size,
                        svg,
                        if hovered { fg } else { muted },
                        0.95,
                    );
                }
                TitlebarAction::Close => {
                    let color = if hovered {
                        Rgb {
                            red: 255,
                            green: 255,
                            blue: 255,
                        }
                    } else {
                        muted
                    };
                    self.svg_mask(origin, icon_size, svg::icons::CLOSE, color, 0.95);
                }
                TitlebarAction::CommandBar
                | TitlebarAction::SidebarNewTab
                | TitlebarAction::SelectTab(_)
                | TitlebarAction::CloseTab(_) => {}
            }
        }

        self.area = prev_area;
        start..self.instances.len() as u32
    }

    pub(in super::super) fn prepare_sidebar(
        &mut self,
        view: &titlebar::View<'_>,
        progress_phase: f32,
    ) -> Range<u32> {
        let start = self.instances.len() as u32;
        let layout = view.layout;
        let sidebar_h = layout.window_height.saturating_sub(layout.height);
        let (Some(width), Some(height)) = (
            NonZeroU32::new(layout.sidebar_width),
            NonZeroU32::new(sidebar_h),
        ) else {
            return start..start;
        };
        let area = PixelSize { width, height };
        let prev_area = std::mem::replace(&mut self.area, area);

        let scale = view.scale.max(1.0);
        let px = |logical: f64| (logical * scale).round().max(1.0) as u32;
        let bg = view.palette.color(PaletteEntry::Background);
        let fg = view.palette.color(PaletteEntry::Foreground);
        let accent = view.palette.color(PaletteEntry::Indexed(4));
        let muted = mix_color(bg, fg, 145);
        let sidebar_bg = mix_color(bg, fg, 9);
        let sidebar_alpha = if view.background_opacity >= 1.0 {
            0.94
        } else {
            (view.background_opacity * 0.92).max(0.58)
        };

        self.rect(
            PixelRect {
                origin: PixelPosition { x: 0, y: 0 },
                size: area,
            },
            sidebar_bg,
            sidebar_alpha,
        );
        self.rect(
            PixelRect {
                origin: PixelPosition {
                    x: layout.sidebar_width.saturating_sub(1),
                    y: 0,
                },
                size: PixelSize {
                    width: NonZeroU32::new(1).unwrap(),
                    height,
                },
            },
            mix_color(bg, fg, 30),
            0.65,
        );

        let m = layout.metrics;
        let ui_metrics = self.chrome_metrics;
        let row_radius = m.sidebar_row_radius;
        let icon_size = m.tab_icon_size;
        let close_icon_size = m.close_icon_size;
        let cell_h = ui_metrics.cell.height.get();
        let cell_w = ui_metrics.cell.width.get().max(1);
        let indicator_w = (m.sidebar_pad_x / 2).clamp(3, 4);

        // Sidebar top section header: "TABS" label, tab count pill, and `+` button.
        if let Some(header_abs) = layout.sidebar_header_rect {
            let header_y = header_abs.origin.y.saturating_sub(layout.height);
            let header_h = header_abs.size.height.get();
            let text_y = header_y + header_h.saturating_sub(cell_h) / 2;
            let label = "TABS";
            let label_x = header_abs.origin.x + m.item_gap;
            let label_w = self.font.measure_text(ui_metrics, label);
            if let Some(lw) = NonZeroU32::new(label_w.min(header_abs.size.width.get())) {
                self.glyph(
                    PixelRect {
                        origin: PixelPosition {
                            x: label_x,
                            y: text_y,
                        },
                        size: PixelSize {
                            width: lw,
                            height: ui_metrics.cell.height,
                        },
                    },
                    label,
                    CHROME_LABEL_CACHE,
                    mix_color(bg, fg, 125),
                    0.9,
                );
            }
            let count_str = view.tabs.len().to_string();
            let count_w = self.font.measure_text(ui_metrics, &count_str);
            let badge_pad_x = m.item_gap.max(5);
            let badge_w = count_w.saturating_add(badge_pad_x * 2);
            let badge_h = cell_h.saturating_add(2).min(header_h);
            let badge_x = label_x + label_w + m.item_gap;
            let badge_y = header_y + header_h.saturating_sub(badge_h) / 2;
            let plus_left = layout
                .sidebar_header_plus
                .map_or(header_abs.origin.x + header_abs.size.width.get(), |r| {
                    r.origin.x.saturating_sub(m.item_gap)
                });
            if badge_x + badge_w <= plus_left
                && let (Some(bw), Some(bh)) = (NonZeroU32::new(badge_w), NonZeroU32::new(badge_h))
            {
                let badge_rect = PixelRect {
                    origin: PixelPosition {
                        x: badge_x,
                        y: badge_y,
                    },
                    size: PixelSize {
                        width: bw,
                        height: bh,
                    },
                };
                self.rounded_rect(badge_rect, bh.get() / 2, 0, mix_color(bg, fg, 22), 0.78);
                if let Some(cw) = NonZeroU32::new(count_w.min(ATLAS_SIDE as u32)) {
                    self.glyph(
                        PixelRect {
                            origin: PixelPosition {
                                x: badge_x + badge_pad_x,
                                y: badge_y + bh.get().saturating_sub(cell_h) / 2,
                            },
                            size: PixelSize {
                                width: cw,
                                height: ui_metrics.cell.height,
                            },
                        },
                        &count_str,
                        CHROME_LABEL_CACHE,
                        muted,
                        0.9,
                    );
                }
            }
            if let Some(plus_abs) = layout.sidebar_header_plus {
                let plus_rect = PixelRect {
                    origin: PixelPosition {
                        x: plus_abs.origin.x,
                        y: plus_abs.origin.y.saturating_sub(layout.height),
                    },
                    size: plus_abs.size,
                };
                let plus_hovered = view.hovered == Some(TitlebarAction::NewTab);
                if plus_hovered {
                    let r = (plus_rect.size.height.get() / 4).max(4);
                    self.rounded_rect(plus_rect, r, 0, mix_color(sidebar_bg, fg, 26), 0.85);
                }
                let pside = plus_rect.size.width.get().min(plus_rect.size.height.get());
                let picon = (pside.saturating_mul(54) / 100).max(10).min(pside);
                self.svg_mask(
                    PixelPosition {
                        x: plus_rect.origin.x
                            + plus_rect.size.width.get().saturating_sub(picon) / 2,
                        y: plus_rect.origin.y
                            + plus_rect.size.height.get().saturating_sub(picon) / 2,
                    },
                    picon,
                    svg::icons::PLUS,
                    if plus_hovered { fg } else { muted },
                    0.95,
                );
            }
        }

        let mut draw_order: Vec<usize> = (0..layout.sidebar_tabs.len()).collect();
        draw_order
            .sort_by_key(|&i| usize::from(view.dragged_tab == Some(layout.sidebar_tabs[i].id)));

        for &slot_idx in &draw_order {
            let slot = layout.sidebar_tabs[slot_idx];
            let Some(tab) = view.tabs.get(slot.index) else {
                continue;
            };
            let active = slot.index == view.active;
            let dragged = view.dragged_tab == Some(slot.id);
            let anim_y_abs = view
                .animator
                .coord(slot.id, slot.rect.origin.y as f32)
                .round()
                .max(layout.height as f32) as u32;
            let local_y = anim_y_abs.saturating_sub(layout.height);
            let rect = PixelRect {
                origin: PixelPosition {
                    x: slot.rect.origin.x,
                    y: local_y,
                },
                size: slot.rect.size,
            };
            let tab_hovered = matches!(
                view.hovered,
                Some(TitlebarAction::SelectTab(i) | TitlebarAction::CloseTab(i))
                    if i == slot.index
            );

            let indicator_inset = m.item_gap.max(5);
            if active || dragged {
                let fill = mix_color(bg, fg, if dragged { 28 } else { 22 });
                let border = if dragged {
                    mix_color(bg, accent, 130)
                } else {
                    mix_color(bg, fg, 48)
                };
                self.rounded_rect(rect, row_radius, 0, fill, if dragged { 0.96 } else { 0.92 });
                self.rounded_rect(
                    rect,
                    row_radius,
                    1,
                    border,
                    if dragged { 0.85 } else { 0.40 },
                );
                let ind_h = (rect.size.height.get().saturating_mul(54) / 100).max(10);
                if let (Some(iw), Some(ih)) = (NonZeroU32::new(indicator_w), NonZeroU32::new(ind_h))
                {
                    self.rounded_rect(
                        PixelRect {
                            origin: PixelPosition {
                                x: rect.origin.x + indicator_inset,
                                y: rect.origin.y + rect.size.height.get().saturating_sub(ind_h) / 2,
                            },
                            size: PixelSize {
                                width: iw,
                                height: ih,
                            },
                        },
                        indicator_w / 2,
                        0,
                        accent,
                        0.95,
                    );
                }
            } else if tab_hovered {
                self.rounded_rect(rect, row_radius, 0, mix_color(bg, fg, 15), 0.78);
            }

            let icon_x = rect.origin.x + indicator_inset + indicator_w + m.item_gap;
            let icon_y = rect.origin.y + rect.size.height.get().saturating_sub(icon_size) / 2;
            self.svg_mask(
                PixelPosition {
                    x: icon_x,
                    y: icon_y,
                },
                icon_size,
                svg::icons::TERMINAL,
                if active || dragged {
                    accent
                } else if tab_hovered {
                    mix_color(bg, fg, 185)
                } else {
                    muted
                },
                if active || dragged { 0.95 } else { 0.78 },
            );

            let shifted_close = slot.close_rect.map(|mut cr| {
                let rel_y = cr.origin.y.saturating_sub(slot.rect.origin.y);
                cr.origin.y = local_y + rel_y;
                cr
            });
            let title_x = icon_x + icon_size + m.item_gap;
            let title_y = rect.origin.y + rect.size.height.get().saturating_sub(cell_h) / 2;
            let right_edge = shifted_close
                .filter(|_| active || tab_hovered)
                .map(|r| r.origin.x.saturating_sub(m.control_gap))
                .unwrap_or_else(|| {
                    rect.origin.x + rect.size.width.get().saturating_sub(m.sidebar_pad_x)
                });
            let title_avail = right_edge.saturating_sub(title_x);
            if title_avail > 0 {
                let max_cells = title_avail / cell_w;
                let tail = titlebar::title_tail(tab.label(), max_cells);
                self.clamped_chrome_text(
                    PixelPosition {
                        x: title_x,
                        y: title_y,
                    },
                    title_avail,
                    &tail,
                    if active || dragged {
                        fg
                    } else if tab_hovered {
                        mix_color(bg, fg, 210)
                    } else {
                        muted
                    },
                );
            }

            if let Some(close_rect) = shifted_close
                && (active || tab_hovered)
            {
                let close_hovered = view.hovered == Some(TitlebarAction::CloseTab(slot.index));
                if close_hovered {
                    let r = close_rect.size.width.get() / 2;
                    self.rounded_rect(close_rect, r, 0, mix_color(bg, fg, 48), 0.85);
                }
                let cx = close_rect.origin.x
                    + close_rect.size.width.get().saturating_sub(close_icon_size) / 2;
                let cy = close_rect.origin.y
                    + close_rect.size.height.get().saturating_sub(close_icon_size) / 2;
                self.svg_mask(
                    PixelPosition { x: cx, y: cy },
                    close_icon_size,
                    svg::icons::CLOSE,
                    if close_hovered { fg } else { muted },
                    0.9,
                );
            }

            self.tab_progress_strip(
                rect,
                tab.progress(),
                view.palette,
                progress_phase,
                m.item_gap,
            );
        }

        // Pinned bottom `+ New Tab` action card.
        if let Some(footer_abs) = layout.sidebar_footer_rect {
            let footer_y = footer_abs.origin.y.saturating_sub(layout.height);
            let divider_y = footer_y.saturating_sub(m.sidebar_pad_top / 2);
            if divider_y > 0 {
                self.rect(
                    PixelRect {
                        origin: PixelPosition {
                            x: footer_abs.origin.x,
                            y: divider_y,
                        },
                        size: PixelSize {
                            width: footer_abs.size.width,
                            height: NonZeroU32::new(1).unwrap(),
                        },
                    },
                    mix_color(bg, fg, 28),
                    0.45,
                );
            }
            let footer_rect = PixelRect {
                origin: PixelPosition {
                    x: footer_abs.origin.x,
                    y: footer_y,
                },
                size: footer_abs.size,
            };
            let footer_hovered = view.hovered == Some(TitlebarAction::SidebarNewTab);
            self.rounded_rect(
                footer_rect,
                row_radius,
                0,
                mix_color(bg, fg, if footer_hovered { 22 } else { 13 }),
                if footer_hovered { 0.88 } else { 0.58 },
            );
            self.rounded_rect(
                footer_rect,
                row_radius,
                1,
                mix_color(bg, fg, if footer_hovered { 48 } else { 30 }),
                if footer_hovered { 0.65 } else { 0.38 },
            );

            let fh = footer_rect.size.height.get();
            let fw = footer_rect.size.width.get();
            let picon = icon_size.min(fh.saturating_sub(8).max(10));
            let px_left = footer_rect.origin.x + m.edge_padding;
            self.svg_mask(
                PixelPosition {
                    x: px_left,
                    y: footer_rect.origin.y + fh.saturating_sub(picon) / 2,
                },
                picon,
                svg::icons::PLUS,
                if footer_hovered { accent } else { muted },
                0.95,
            );

            let mut footer_right = footer_rect.origin.x + fw.saturating_sub(m.edge_padding);
            let label_x = px_left + picon + m.item_gap;
            let label_y = footer_rect.origin.y + fh.saturating_sub(cell_h) / 2;
            if !view.new_tab_shortcut.is_empty() {
                let hint_w = self.font.measure_text(ui_metrics, view.new_tab_shortcut);
                let min_label_w = self.font.measure_text(ui_metrics, "New Tab");
                if footer_right > label_x + min_label_w + hint_w + m.edge_padding
                    && let Some(hw) = NonZeroU32::new(hint_w.min(ATLAS_SIDE as u32))
                {
                    let hx = footer_right.saturating_sub(hint_w);
                    self.glyph(
                        PixelRect {
                            origin: PixelPosition { x: hx, y: label_y },
                            size: PixelSize {
                                width: hw,
                                height: ui_metrics.cell.height,
                            },
                        },
                        view.new_tab_shortcut,
                        CHROME_LABEL_CACHE,
                        mix_color(bg, fg, 115),
                        0.85,
                    );
                    footer_right = hx.saturating_sub(m.item_gap);
                }
            }
            let label_avail = footer_right.saturating_sub(label_x);
            if label_avail > 0 {
                self.clamped_chrome_text(
                    PixelPosition {
                        x: label_x,
                        y: label_y,
                    },
                    label_avail,
                    "New Tab",
                    if footer_hovered { fg } else { muted },
                );
            }
        }

        let total = view.tabs.len();
        let visible = layout.sidebar_visible_rows.max(1);
        if total > visible {
            let track_top = layout.sidebar_list_top.saturating_sub(layout.height);
            let track_bottom = layout.sidebar_list_bottom.saturating_sub(layout.height);
            let track_h = track_bottom.saturating_sub(track_top);
            if track_h > 16 {
                let thumb_h = ((u64::from(track_h) * visible as u64) / total as u64)
                    .max(18)
                    .min(u64::from(track_h)) as u32;
                let travel = track_h.saturating_sub(thumb_h);
                let max_scroll = total - visible;
                let thumb_y = track_top
                    + ((u64::from(travel) * layout.sidebar_scroll as u64) / max_scroll as u64)
                        as u32;
                let bar_w = px(3.0).clamp(2, 4);
                if let (Some(w), Some(h)) = (NonZeroU32::new(bar_w), NonZeroU32::new(thumb_h)) {
                    self.rounded_rect(
                        PixelRect {
                            origin: PixelPosition {
                                x: layout.sidebar_width.saturating_sub(bar_w + px(3.0)),
                                y: thumb_y,
                            },
                            size: PixelSize {
                                width: w,
                                height: h,
                            },
                        },
                        bar_w / 2,
                        0,
                        muted,
                        0.65,
                    );
                }
            }
        }

        self.area = prev_area;
        start..self.instances.len() as u32
    }

    fn tab_progress_strip(
        &mut self,
        rect: PixelRect,
        progress: Progress,
        palette: &lune_terminal::Palette,
        progress_phase: f32,
        inset: u32,
    ) {
        if progress == Progress::None {
            return;
        }
        let Some(bar_h) = NonZeroU32::new(2.min(rect.size.height.get())) else {
            return;
        };
        let total_w = rect.size.width.get().saturating_sub(inset * 2);
        let y = rect.origin.y + rect.size.height.get() - bar_h.get();
        let (bar_color, start_x, seg_w) = match progress {
            Progress::None => return,
            Progress::Normal(p) => (
                palette.color(PaletteEntry::Indexed(6)),
                0,
                ((u64::from(total_w) * u64::from(p)) / 100) as u32,
            ),
            Progress::Error(p) => (
                palette.color(PaletteEntry::Indexed(1)),
                0,
                ((u64::from(total_w) * u64::from(if p == 0 { 100 } else { p })) / 100) as u32,
            ),
            Progress::Paused(p) => (
                palette.color(PaletteEntry::Indexed(3)),
                0,
                ((u64::from(total_w) * u64::from(p)) / 100) as u32,
            ),
            Progress::Indeterminate => {
                let seg = ((total_w as f32 * 0.35).round() as u32).max(2).min(total_w);
                let x = ((total_w - seg) as f32 * progress_phase).round() as u32;
                (palette.color(PaletteEntry::Indexed(6)), x, seg)
            }
        };
        if let Some(w) = NonZeroU32::new(seg_w.min(total_w.saturating_sub(start_x))) {
            self.rect(
                PixelRect {
                    origin: PixelPosition {
                        x: rect.origin.x + inset + start_x,
                        y,
                    },
                    size: PixelSize {
                        width: w,
                        height: bar_h,
                    },
                },
                bar_color,
                0.95,
            );
        }
    }

    fn concave_wing(&mut self, rect: PixelRect, right_side: bool, color: Rgb, alpha: f32) {
        let w = rect.size.width.get().min(1023);
        let h = rect.size.height.get().min(1023);
        let packed = w | (h << 10) | (u32::from(right_side) << 20);
        self.quad(
            rect,
            color,
            alpha,
            [0.0, 0.0, w as f32, h as f32],
            [7, packed],
        );
    }

    pub(in super::super) fn prepare_tab_bar(
        &mut self,
        view: &tab_bar::View<'_>,
        progress_phase: f32,
        background_opacity: f32,
    ) -> Range<u32> {
        let start = self.instances.len() as u32;
        self.tab_bar(view, progress_phase, background_opacity);
        start..self.instances.len() as u32
    }

    fn tab_bar(&mut self, view: &tab_bar::View<'_>, progress_phase: f32, background_opacity: f32) {
        let layout = view.layout;
        let Some(area) = layout.area() else {
            return;
        };
        let terminal_area = std::mem::replace(&mut self.area, area);
        let background = view.palette.color(PaletteEntry::Background);
        let foreground = view.palette.color(PaletteEntry::Foreground);
        let accent = view.palette.color(PaletteEntry::Indexed(4));
        let active_text = [
            background,
            foreground,
            view.palette.color(PaletteEntry::Indexed(0)),
        ]
        .into_iter()
        .max_by_key(|&color| luminance(color).abs_diff(luminance(accent)))
        .unwrap();
        let inactive = mix_color(background, accent, 36);
        let inactive_text = mix_color(background, mix_color(foreground, accent, 88), 106);
        for (index, fill_rect, title, title_end) in layout.tabs() {
            let Some(tab) = view.tabs.get(index) else {
                break;
            };
            let active = index == view.active;
            let (fill, color) = if active {
                (accent, active_text)
            } else {
                (inactive, inactive_text)
            };
            self.faded_tab(
                fill_rect,
                background,
                fill,
                layout.fade_cell_width(),
                background_opacity,
                active,
            );
            if title_end != 0
                && let Some(label) = tab.label().get(..title_end)
            {
                self.tab_label(title, label, color);
            }
            let tab_progress = tab.progress();
            if tab_progress != Progress::None
                && let Some(bar_h) = NonZeroU32::new(2.min(fill_rect.size.height.get()))
            {
                let y = fill_rect.origin.y + fill_rect.size.height.get() - bar_h.get();
                let total_w = fill_rect.size.width.get();
                let (bar_color, start_x, seg_w) = match tab_progress {
                    Progress::None => (color, 0, 0),
                    Progress::Normal(p) => (
                        view.palette.color(PaletteEntry::Indexed(6)),
                        0,
                        ((u64::from(total_w) * u64::from(p)) / 100) as u32,
                    ),
                    Progress::Error(p) => (
                        view.palette.color(PaletteEntry::Indexed(1)),
                        0,
                        ((u64::from(total_w) * u64::from(if p == 0 { 100 } else { p })) / 100)
                            as u32,
                    ),
                    Progress::Paused(p) => (
                        view.palette.color(PaletteEntry::Indexed(3)),
                        0,
                        ((u64::from(total_w) * u64::from(p)) / 100) as u32,
                    ),
                    Progress::Indeterminate => {
                        let seg = ((total_w as f32 * 0.35).round() as u32).max(2).min(total_w);
                        let x = ((total_w - seg) as f32 * progress_phase).round() as u32;
                        (view.palette.color(PaletteEntry::Indexed(6)), x, seg)
                    }
                };
                if let Some(w) = NonZeroU32::new(seg_w.min(total_w.saturating_sub(start_x))) {
                    self.rect(
                        PixelRect {
                            origin: PixelPosition {
                                x: fill_rect.origin.x + start_x,
                                y,
                            },
                            size: PixelSize {
                                width: w,
                                height: bar_h,
                            },
                        },
                        bar_color,
                        0.95,
                    );
                }
            }
        }
        self.area = terminal_area;
    }

    fn faded_tab(
        &mut self,
        center: PixelRect,
        background: Rgb,
        color: Rgb,
        cell: NonZeroU32,
        background_opacity: f32,
        active: bool,
    ) {
        let mut edge = center;
        edge.size.width = cell;
        let opaque = background_opacity >= 1.0;
        let center_alpha = if opaque || active {
            1.0
        } else {
            background_opacity.max(0.55)
        };
        for (offset, amount) in [176, 112, 56].into_iter().enumerate() {
            let offset = offset as u32 * cell.get();
            let (edge_color, edge_alpha) = if opaque {
                (mix_color(background, color, amount), 1.0)
            } else {
                (color, center_alpha * (f32::from(amount) / 255.0))
            };
            edge.origin.x = center.origin.x - offset - cell.get();
            self.rect(edge, edge_color, edge_alpha);
            edge.origin.x = center.origin.x + center.size.width.get() + offset;
            self.rect(edge, edge_color, edge_alpha);
        }
        self.rect(center, color, center_alpha);
    }

    fn tab_label(&mut self, mut rect: PixelRect, text: &str, color: Rgb) {
        rect.size.width = rect
            .size
            .width
            .min(NonZeroU32::new(ATLAS_SIDE as u32).unwrap());
        self.glyph(rect, text, TAB_LABEL_CACHE, color, 1.0);
    }
}
