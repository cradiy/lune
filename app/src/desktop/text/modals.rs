//! Modal and popup surface rendering: confirmation dialogs, right-click context menus,
//! and the fuzzy Command Palette.

use std::num::NonZeroU32;
use std::ops::Range;

use lune_terminal::{PaletteEntry, PixelPosition, PixelSize, Rgb};

use crate::PixelRect;

use super::super::confirm;
use super::super::context_menu::{self, ContextMenu, MenuEntry};
use super::super::palette::{self, CommandPalette};
use super::super::svg;
use super::{ATLAS_SIDE, CHROME_LABEL_CACHE, LABEL_CACHE, TextRenderer, mix_color};

impl TextRenderer {
    pub(in super::super) fn prepare_modal(
        &mut self,
        area: PixelSize,
        palette: &lune_terminal::Palette,
        dialog: &confirm::Dialog,
        scale: f64,
        background_opacity: f32,
    ) -> Range<u32> {
        let start = self.instances.len() as u32;
        self.area = area;
        let bg = palette.color(PaletteEntry::Background);
        let fg = palette.color(PaletteEntry::Foreground);
        let backdrop_alpha = if background_opacity < 1.0 {
            (background_opacity * 0.68).max(0.58)
        } else {
            0.58
        };
        self.rect(
            PixelRect {
                origin: PixelPosition { x: 0, y: 0 },
                size: area,
            },
            bg,
            backdrop_alpha,
        );
        let Some(layout) = confirm::Layout::compute(area, self.metrics, scale, dialog) else {
            return start..self.instances.len() as u32;
        };

        let green = palette.color(PaletteEntry::Indexed(2));
        let red = palette.color(PaletteEntry::Indexed(1));
        let amber = palette.color(PaletteEntry::Indexed(3));
        let accent = if dialog.target.is_paste() { amber } else { red };
        let card_fill = mix_color(bg, fg, 22);
        let card_border = mix_color(bg, accent, 150);

        self.floating_card(
            layout.card,
            layout.card_radius,
            bg,
            card_fill,
            card_border,
            0.97,
        );
        self.rounded_rect(
            layout.indicator,
            layout.indicator.size.width.get() / 2,
            0,
            accent,
            0.95,
        );

        let title = dialog.target.title();
        let title_w = self
            .font
            .measure_text(self.metrics, title)
            .min(layout.message_max_w)
            .min(ATLAS_SIDE as u32);
        if let Some(width) = NonZeroU32::new(title_w) {
            self.glyph(
                PixelRect {
                    origin: layout.title_origin,
                    size: PixelSize {
                        width,
                        height: self.metrics.cell.height,
                    },
                },
                title,
                LABEL_CACHE,
                accent,
                1.0,
            );
        }

        for (index, line) in layout.lines.iter().enumerate() {
            let origin = PixelPosition {
                x: layout.message_origin.x,
                y: layout
                    .message_origin
                    .y
                    .saturating_add(layout.line_step.saturating_mul(index as u32)),
            };
            self.clamped_text(origin, layout.message_max_w, line, fg);
        }

        let muted_border = mix_color(bg, fg, 90);
        for (button, rect, accel, rest, accel_color) in [
            (confirm::Button::Yes, layout.yes, "Y", "es", green),
            (confirm::Button::No, layout.no, "N", "o", red),
        ] {
            let active = dialog.selected == button || dialog.hovered == Some(button);
            let fill = if active {
                mix_color(card_fill, accent, 38)
            } else {
                mix_color(bg, fg, 12)
            };
            let (border_color, border_alpha) = if active {
                (accent, 0.95)
            } else {
                (muted_border, 0.78)
            };
            self.rounded_rect(rect, layout.radius, 0, fill, 0.92);
            self.rounded_rect(
                rect,
                layout.radius,
                layout.border,
                border_color,
                border_alpha,
            );
            self.button_label(rect, accel, rest, accel_color, fg);
        }
        start..self.instances.len() as u32
    }

    pub(in super::super) fn prepare_context_menu(
        &mut self,
        area: PixelSize,
        palette: &lune_terminal::Palette,
        menu: &ContextMenu,
        scale: f64,
        background_opacity: f32,
    ) -> Range<u32> {
        let start = self.instances.len() as u32;
        self.area = area;
        let ui_metrics = self.chrome_metrics;
        let Some(layout) = context_menu::Layout::compute(area, ui_metrics, scale, menu) else {
            return start..self.instances.len() as u32;
        };

        let bg = palette.color(PaletteEntry::Background);
        let fg = palette.color(PaletteEntry::Foreground);
        let accent = palette.color(PaletteEntry::Indexed(4));
        let red = palette.color(PaletteEntry::Indexed(1));
        let muted = mix_color(bg, fg, 130);
        let sep_color = mix_color(bg, fg, 48);

        let card_fill = mix_color(bg, fg, 22);
        let card_border = mix_color(bg, fg, 68);
        let card_alpha = if background_opacity < 1.0 { 0.96 } else { 0.98 };
        self.floating_card(
            layout.card,
            layout.radius,
            bg,
            card_fill,
            card_border,
            card_alpha,
        );

        let ui_cell_h = ui_metrics.cell.height.get();
        let scale_f = scale.max(1.0);
        let floor = |logical: f64| (logical * scale_f).round().max(1.0) as u32;
        let row_radius = floor(5.0).clamp(4, 8);
        let shortcut_gap = floor(12.0);

        for slot in &layout.rows {
            let Some(entry) = menu.entries.get(slot.entry_index) else {
                continue;
            };
            match entry {
                MenuEntry::Separator => {
                    let sep_x = slot.rect.origin.x.saturating_add(layout.pad_x / 2);
                    let sep_w = slot.rect.size.width.get().saturating_sub(layout.pad_x);
                    if let Some(w) = NonZeroU32::new(sep_w) {
                        let sep_y = slot.rect.origin.y + slot.rect.size.height.get() / 2;
                        self.rect(
                            PixelRect {
                                origin: PixelPosition { x: sep_x, y: sep_y },
                                size: PixelSize {
                                    width: w,
                                    height: NonZeroU32::new(1).unwrap(),
                                },
                            },
                            sep_color,
                            0.55,
                        );
                    }
                }
                MenuEntry::Item {
                    label,
                    shortcut,
                    danger,
                    ..
                } => {
                    let hovered = menu.hovered == Some(slot.entry_index);
                    if hovered {
                        let hover_fill = if *danger {
                            mix_color(bg, red, 62)
                        } else {
                            mix_color(bg, accent, 54)
                        };
                        self.rounded_rect(slot.rect, row_radius, 0, hover_fill, 0.92);
                    }
                    let text_y = slot.rect.origin.y
                        + slot.rect.size.height.get().saturating_sub(ui_cell_h) / 2;
                    let text_x = slot.rect.origin.x.saturating_add(layout.pad_x);
                    let right_edge = slot
                        .rect
                        .origin
                        .x
                        .saturating_add(slot.rect.size.width.get())
                        .saturating_sub(layout.pad_x);

                    let mut shortcut_reserve = 0u32;
                    if !shortcut.is_empty() {
                        let sw = self.font.measure_text(ui_metrics, shortcut);
                        if right_edge > text_x.saturating_add(sw + shortcut_gap)
                            && let Some(sw_nz) = NonZeroU32::new(sw.min(ATLAS_SIDE as u32))
                        {
                            let sx = right_edge.saturating_sub(sw_nz.get());
                            let sc = if hovered {
                                mix_color(bg, fg, 215)
                            } else {
                                muted
                            };
                            self.glyph(
                                PixelRect {
                                    origin: PixelPosition { x: sx, y: text_y },
                                    size: PixelSize {
                                        width: sw_nz,
                                        height: ui_metrics.cell.height,
                                    },
                                },
                                shortcut,
                                CHROME_LABEL_CACHE,
                                sc,
                                1.0,
                            );
                            shortcut_reserve = sw_nz.get().saturating_add(shortcut_gap);
                        }
                    }

                    let label_max_w =
                        right_edge.saturating_sub(text_x.saturating_add(shortcut_reserve));
                    if label_max_w > 0 {
                        let label_color = if *danger {
                            if hovered {
                                mix_color(fg, red, 215)
                            } else {
                                mix_color(bg, red, 210)
                            }
                        } else if hovered {
                            fg
                        } else {
                            mix_color(bg, fg, 225)
                        };
                        self.clamped_chrome_text(
                            PixelPosition {
                                x: text_x,
                                y: text_y,
                            },
                            label_max_w,
                            label,
                            label_color,
                        );
                    }
                }
            }
        }
        start..self.instances.len() as u32
    }

    pub(in super::super) fn prepare_command_palette(
        &mut self,
        area: PixelSize,
        palette: &lune_terminal::Palette,
        cmd_palette: &CommandPalette,
        scale: f64,
        background_opacity: f32,
    ) -> Range<u32> {
        let start = self.instances.len() as u32;
        self.area = area;
        let bg = palette.color(PaletteEntry::Background);
        let fg = palette.color(PaletteEntry::Foreground);
        let accent = palette.color(PaletteEntry::Indexed(4));
        let green = palette.color(PaletteEntry::Indexed(2));
        let red = palette.color(PaletteEntry::Indexed(1));
        let muted = mix_color(bg, fg, 125);

        let backdrop_alpha = if background_opacity < 1.0 {
            (background_opacity * 0.56).max(0.45)
        } else {
            0.45
        };
        self.rect(
            PixelRect {
                origin: PixelPosition { x: 0, y: 0 },
                size: area,
            },
            bg,
            backdrop_alpha,
        );

        let Some(layout) =
            palette::Layout::compute(area, self.metrics, scale, cmd_palette.match_count())
        else {
            return start..self.instances.len() as u32;
        };

        let card_fill = mix_color(bg, fg, 22);
        let card_border = mix_color(bg, accent, 145);
        self.floating_card(
            layout.card,
            layout.card_radius,
            bg,
            card_fill,
            card_border,
            0.97,
        );

        let cell_w = self.metrics.cell.width.get();
        let cell_h = self.metrics.cell.height.get();
        let gap = (cell_w * 2 / 3).max(6);
        let mut input_left = layout.input_origin.x;
        let input_right = layout.input_origin.x.saturating_add(layout.input_max_w);

        // Search icon at root, or submenu breadcrumb.
        if let Some(name) = cmd_palette.breadcrumb {
            let prefix = format!("{name} >");
            let prefix_w = self.font.measure_text(self.metrics, &prefix);
            if let Some(width) = NonZeroU32::new(
                prefix_w
                    .min(input_right.saturating_sub(input_left))
                    .min(ATLAS_SIDE as u32),
            ) {
                self.glyph(
                    PixelRect {
                        origin: PixelPosition {
                            x: input_left,
                            y: layout.input_origin.y,
                        },
                        size: PixelSize {
                            width,
                            height: self.metrics.cell.height,
                        },
                    },
                    &prefix,
                    LABEL_CACHE,
                    accent,
                    1.0,
                );
                input_left = input_left.saturating_add(width.get() + gap);
            }
        } else {
            let icon_size = (cell_h * 4 / 5).clamp(12, cell_h);
            if input_right > input_left + icon_size + gap
                && self.svg_mask(
                    PixelPosition {
                        x: input_left,
                        y: layout.input_origin.y + cell_h.saturating_sub(icon_size) / 2,
                    },
                    icon_size,
                    svg::icons::SEARCH,
                    accent,
                    1.0,
                )
            {
                input_left = input_left.saturating_add(icon_size + gap);
            }
        }

        // Right-aligned match counter.
        let count_text = format!(
            "{}/{}",
            cmd_palette.match_count(),
            cmd_palette.total_count()
        );
        let count_w = self.font.measure_text(self.metrics, &count_text);
        let mut query_right = input_right;
        if count_w > 0 && query_right > input_left + count_w + gap {
            let x = query_right - count_w;
            if let Some(width) = NonZeroU32::new(count_w.min(ATLAS_SIDE as u32)) {
                self.glyph(
                    PixelRect {
                        origin: PixelPosition {
                            x,
                            y: layout.input_origin.y,
                        },
                        size: PixelSize {
                            width,
                            height: self.metrics.cell.height,
                        },
                    },
                    &count_text,
                    LABEL_CACHE,
                    muted,
                    1.0,
                );
            }
            query_right = x.saturating_sub(gap);
        }

        // Query text + IME preedit or placeholder.
        let query_avail = query_right.saturating_sub(input_left);
        if query_avail > 0 {
            let full_query = if cmd_palette.preedit.is_empty() {
                std::borrow::Cow::Borrowed(cmd_palette.query.as_str())
            } else {
                std::borrow::Cow::Owned(format!("{}{}", cmd_palette.query, cmd_palette.preedit))
            };
            if full_query.is_empty() {
                let placeholder = match cmd_palette.breadcrumb {
                    Some("Theme") => "Filter themes...",
                    Some("Commands") => "Filter command history...",
                    Some("Scrollbar") => "Select scrollbar visibility...",
                    _ => "Type a command...",
                };
                self.clamped_text(
                    PixelPosition {
                        x: input_left,
                        y: layout.input_origin.y,
                    },
                    query_avail,
                    placeholder,
                    muted,
                );
                if let Some(bar_w) = NonZeroU32::new(2.min(query_avail)) {
                    self.rect(
                        PixelRect {
                            origin: PixelPosition {
                                x: input_left,
                                y: layout.input_origin.y,
                            },
                            size: PixelSize {
                                width: bar_w,
                                height: self.metrics.cell.height,
                            },
                        },
                        accent,
                        0.9,
                    );
                }
            } else {
                let mut visible: &str = &full_query;
                let mut measured = self.font.measure_text(self.metrics, visible);
                while measured > query_avail.saturating_sub(2) {
                    let Some(next) = visible.char_indices().nth(1).map(|(idx, _)| idx) else {
                        break;
                    };
                    visible = &visible[next..];
                    measured = self.font.measure_text(self.metrics, visible);
                }
                let draw_w = measured.min(query_avail).min(ATLAS_SIDE as u32);
                if let Some(width) = NonZeroU32::new(draw_w) {
                    self.glyph(
                        PixelRect {
                            origin: PixelPosition {
                                x: input_left,
                                y: layout.input_origin.y,
                            },
                            size: PixelSize {
                                width,
                                height: self.metrics.cell.height,
                            },
                        },
                        visible,
                        LABEL_CACHE,
                        fg,
                        1.0,
                    );
                }
                let caret_x = input_left.saturating_add(draw_w);
                if caret_x < query_right
                    && let Some(bar_w) = NonZeroU32::new(2.min(query_right - caret_x))
                {
                    self.rect(
                        PixelRect {
                            origin: PixelPosition {
                                x: caret_x,
                                y: layout.input_origin.y,
                            },
                            size: PixelSize {
                                width: bar_w,
                                height: self.metrics.cell.height,
                            },
                        },
                        accent,
                        0.9,
                    );
                }
            }
        }

        // Divider between input and list.
        self.rect(
            PixelRect {
                origin: PixelPosition {
                    x: layout.card.origin.x.saturating_add(1),
                    y: layout.divider_y,
                },
                size: PixelSize {
                    width: NonZeroU32::new(layout.card.size.width.get().saturating_sub(2).max(1))
                        .unwrap(),
                    height: NonZeroU32::new(1).unwrap(),
                },
            },
            mix_color(bg, fg, 60),
            0.7,
        );

        let row_radius = (layout.row_height / 4).clamp(4, 8);
        let indicator_w = (cell_w / 3).clamp(3, 4);
        let indicator_h = (cell_h * 2 / 3).max(6).min(cell_h);

        let mut drawn = 0usize;
        for (visible_idx, (filtered_idx, entry)) in cmd_palette
            .visible_slice()
            .take(layout.visible_rows)
            .enumerate()
        {
            let Some(row_rect) = layout.row_rect(visible_idx) else {
                break;
            };
            drawn += 1;
            let is_selected = filtered_idx == cmd_palette.selected;
            let is_hovered = cmd_palette.hovered == Some(visible_idx);
            if is_selected {
                let sel_fill = mix_color(card_fill, accent, 42);
                self.rounded_rect(row_rect, row_radius, 0, sel_fill, 0.92);
                if let (Some(iw), Some(ih)) =
                    (NonZeroU32::new(indicator_w), NonZeroU32::new(indicator_h))
                {
                    self.rounded_rect(
                        PixelRect {
                            origin: PixelPosition {
                                x: row_rect.origin.x.saturating_add(4),
                                y: row_rect.origin.y
                                    + row_rect.size.height.get().saturating_sub(indicator_h) / 2,
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
            } else if is_hovered {
                let hover_fill = mix_color(bg, fg, 34);
                self.rounded_rect(row_rect, row_radius, 0, hover_fill, 0.55);
            }

            let text_y = row_rect.origin.y + layout.row_pad_y;
            let text_left = row_rect.origin.x + (layout.row_pad_x / 2).max(10) + indicator_w;
            let mut text_right = row_rect.origin.x
                + row_rect
                    .size
                    .width
                    .get()
                    .saturating_sub((layout.row_pad_x / 2).max(10));

            if !entry.detail.is_empty() {
                let dw = self.font.measure_text(self.metrics, &entry.detail);
                if dw > 0 && text_right > text_left + dw + gap {
                    let x = text_right - dw;
                    let detail_color = if entry.detail.starts_with("exit ") {
                        red
                    } else if matches!(entry.detail.as_str(), "ok" | "On" | "Active") {
                        green
                    } else if entry.detail.ends_with('>') {
                        accent
                    } else {
                        muted
                    };
                    if let Some(width) = NonZeroU32::new(dw.min(ATLAS_SIDE as u32)) {
                        self.glyph(
                            PixelRect {
                                origin: PixelPosition { x, y: text_y },
                                size: PixelSize {
                                    width,
                                    height: self.metrics.cell.height,
                                },
                            },
                            &entry.detail,
                            LABEL_CACHE,
                            detail_color,
                            1.0,
                        );
                    }
                    text_right = x.saturating_sub(gap);
                }
            }

            let title_max = text_right.saturating_sub(text_left);
            if title_max > 0 {
                self.clamped_text(
                    PixelPosition {
                        x: text_left,
                        y: text_y,
                    },
                    title_max,
                    &entry.title,
                    if is_selected {
                        fg
                    } else {
                        mix_color(bg, fg, 225)
                    },
                );
            }
        }

        if drawn == 0
            && let Some(row_rect) = layout.row_rect(0)
        {
            self.clamped_text(
                PixelPosition {
                    x: row_rect.origin.x + (layout.row_pad_x / 2).max(10),
                    y: row_rect.origin.y + layout.row_pad_y,
                },
                row_rect.size.width.get().saturating_sub(layout.row_pad_x),
                "No matching commands",
                muted,
            );
        }

        // Subtle scrollbar indicator when the list overflows visible rows.
        let total = cmd_palette.match_count();
        if total > layout.visible_rows && layout.visible_rows > 0 {
            let track_h = layout.row_height.saturating_mul(layout.visible_rows as u32);
            let thumb_h = ((u64::from(track_h) * layout.visible_rows as u64) / total as u64)
                .max(16)
                .min(u64::from(track_h)) as u32;
            let travel = track_h.saturating_sub(thumb_h);
            let max_scroll = total - layout.visible_rows;
            let thumb_y = layout.list_top
                + ((u64::from(travel) * cmd_palette.scroll as u64) / max_scroll as u64) as u32;
            let bar_w = 3u32;
            let bar_x =
                layout.card.origin.x + layout.card.size.width.get().saturating_sub(bar_w + 3);
            if let (Some(w), Some(h)) = (NonZeroU32::new(bar_w), NonZeroU32::new(thumb_h)) {
                self.rounded_rect(
                    PixelRect {
                        origin: PixelPosition {
                            x: bar_x,
                            y: thumb_y,
                        },
                        size: PixelSize {
                            width: w,
                            height: h,
                        },
                    },
                    1,
                    0,
                    muted,
                    0.75,
                );
            }
        }

        start..self.instances.len() as u32
    }

    fn button_label(
        &mut self,
        rect: PixelRect,
        accel: &str,
        rest: &str,
        accel_color: Rgb,
        rest_color: Rgb,
    ) {
        let cell_h = self.metrics.cell.height;
        let accel_w = self.font.measure_text(self.metrics, accel);
        let rest_w = self.font.measure_text(self.metrics, rest);
        let total_w = accel_w.saturating_add(rest_w);
        let x = rect.origin.x + rect.size.width.get().saturating_sub(total_w) / 2;
        let y = rect.origin.y + rect.size.height.get().saturating_sub(cell_h.get()) / 2;
        if let Some(width) = NonZeroU32::new(accel_w) {
            self.glyph(
                PixelRect {
                    origin: PixelPosition { x, y },
                    size: PixelSize {
                        width,
                        height: cell_h,
                    },
                },
                accel,
                LABEL_CACHE,
                accel_color,
                1.0,
            );
        }
        if let Some(width) = NonZeroU32::new(rest_w) {
            self.glyph(
                PixelRect {
                    origin: PixelPosition {
                        x: x.saturating_add(accel_w),
                        y,
                    },
                    size: PixelSize {
                        width,
                        height: cell_h,
                    },
                },
                rest,
                LABEL_CACHE,
                rest_color,
                1.0,
            );
        }
    }
}
