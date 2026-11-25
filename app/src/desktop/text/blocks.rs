//! Command block background rails, sticky header, hover toolbar, and status pill rendering.

use std::num::NonZeroU32;
use std::ops::Range;

use lune_terminal::{CellPosition, PaletteEntry, PixelPosition, PixelSize};

use crate::{PixelRect, PresentationFrame};

use super::super::blocks;
use super::super::paint::PaneOverlay;
use super::super::svg;
use super::{ATLAS_SIDE, CHROME_LABEL_CACHE, TextRenderer, mix_color};

impl TextRenderer {
    /// Prepares the Modern-mode command block backdrop and dedicated left gutter rail
    /// in full pane coordinates, rendered behind terminal cell backgrounds and glyphs.
    pub(in super::super) fn prepare_pane_blocks_bg(
        &mut self,
        pane_rect: PixelRect,
        text_origin: PixelPosition,
        frame: &PresentationFrame<'_>,
        overlay: &PaneOverlay<'_>,
        blocks: &[blocks::CommandBlock],
    ) -> Range<u32> {
        let start = self.instances.len() as u32;
        if !overlay.command_blocks || blocks.is_empty() {
            return start..start;
        }
        let first_row = frame.viewport.range().start;
        let rows = usize::from(frame.geometry.pty_size().rows);

        let prev_area = std::mem::replace(&mut self.area, pane_rect.size);
        let gutter = text_origin.x.saturating_sub(pane_rect.origin.x);
        let pane_w = pane_rect.size.width.get();
        let pane_bg = frame.palette.color(if frame.reverse_video {
            PaletteEntry::Foreground
        } else {
            PaletteEntry::Background
        });
        let pane_fg = frame.palette.color(if frame.reverse_video {
            PaletteEntry::Background
        } else {
            PaletteEntry::Foreground
        });
        let green = frame.palette.color(PaletteEntry::Indexed(2));
        let red = frame.palette.color(PaletteEntry::Indexed(1));
        let gold = frame.palette.color(PaletteEntry::Indexed(3));
        let accent = frame.palette.color(PaletteEntry::Indexed(4));
        let scale_f = overlay.scale.max(1.0);
        let rail_w = (3.0 * scale_f).round().clamp(2.0, 4.0) as u32;
        let card_radius = (8.0 * scale_f).round().clamp(6.0, 11.0) as u32;
        let scrollbar_reserve = overlay
            .scrollbar
            .map_or(0, |sb| sb.track.size.width.get() + 6);
        let rail_x = if gutter > rail_w {
            (gutter - rail_w) / 2
        } else {
            1
        };
        let card_x = (rail_x / 2).max(1);
        let card_w = pane_w.saturating_sub(scrollbar_reserve.saturating_add(card_x + 4));

        for block in blocks {
            let hovered = overlay
                .hovered_block
                .is_some_and(|h| h.prompt_row == block.prompt_row);
            let vis_start = block.prompt_row.saturating_sub(first_row);
            let vis_end = block.end_row.saturating_sub(first_row).min(rows);
            if vis_start >= vis_end || card_w < rail_w * 4 {
                continue;
            }
            let (Some(top_r), Some(bot_r)) = (
                frame.geometry.row_rect(vis_start, 0..1),
                frame.geometry.row_rect(vis_end - 1, 0..1),
            ) else {
                continue;
            };
            let block_top = top_r.origin.y;
            let block_bottom = bot_r.origin.y.saturating_add(bot_r.size.height.get());
            let block_h = block_bottom.saturating_sub(block_top);

            // Whisper-clean top divider between blocks (drawn behind Powerline & glyphs).
            if block.prompt_row > 0
                && block.prompt_row >= first_row
                && let Some(dw) = NonZeroU32::new(
                    pane_w
                        .saturating_sub(gutter.saturating_add(scrollbar_reserve).saturating_add(8)),
                )
            {
                self.rect(
                    PixelRect {
                        origin: PixelPosition {
                            x: gutter,
                            y: block_top,
                        },
                        size: PixelSize {
                            width: dw,
                            height: NonZeroU32::new(1).unwrap(),
                        },
                    },
                    mix_color(pane_bg, pane_fg, 36),
                    0.35,
                );
            }

            // Soft backdrop surface behind hovered, failed, or bookmarked blocks (never washes out Powerline!).
            let show_surface = block.is_error()
                || block.bookmarked
                || (hovered && (block.is_finished() || block.is_running()));
            if show_surface
                && block_h >= 4
                && let (Some(cw), Some(ch)) = (
                    NonZeroU32::new(card_w),
                    NonZeroU32::new(block_h.saturating_sub(2)),
                )
            {
                let surface_rect = PixelRect {
                    origin: PixelPosition {
                        x: card_x,
                        y: block_top.saturating_add(1),
                    },
                    size: PixelSize {
                        width: cw,
                        height: ch,
                    },
                };
                let r = card_radius.min(ch.get() / 2);
                if block.is_error() {
                    self.rounded_rect(
                        surface_rect,
                        r,
                        0,
                        mix_color(pane_bg, red, 20),
                        if hovered { 0.42 } else { 0.26 },
                    );
                    if hovered {
                        self.rounded_rect(surface_rect, r, 1, mix_color(pane_bg, red, 130), 0.55);
                    }
                } else if block.bookmarked {
                    self.rounded_rect(
                        surface_rect,
                        r,
                        0,
                        mix_color(pane_bg, gold, 18),
                        if hovered { 0.42 } else { 0.24 },
                    );
                    if hovered {
                        self.rounded_rect(surface_rect, r, 1, mix_color(pane_bg, gold, 135), 0.55);
                    }
                } else {
                    self.rounded_rect(surface_rect, r, 0, mix_color(pane_bg, pane_fg, 13), 0.54);
                    self.rounded_rect(surface_rect, r, 1, mix_color(pane_bg, accent, 110), 0.42);
                }
            }

            // Dedicated left gutter status rail (cleanly outside column 0 text!).
            let gutter_hovered = overlay.hovered_block.is_some_and(|h| {
                h.prompt_row == block.prompt_row
                    && h.hovered_action == Some(blocks::BlockAction::SelectOutput)
            });
            if block.is_finished() || block.is_running() || block.bookmarked {
                let rail_inset_y = (3.0 * scale_f).round() as u32;
                let rail_h = block_h.saturating_sub(rail_inset_y * 2).max(4);
                let draw_w = if gutter_hovered { rail_w + 1 } else { rail_w };
                let draw_x = if gutter > draw_w {
                    (gutter - draw_w) / 2
                } else {
                    rail_x
                };
                if let (Some(rw), Some(rh)) = (NonZeroU32::new(draw_w), NonZeroU32::new(rail_h)) {
                    let (rail_color, rail_alpha) = if block.bookmarked {
                        (gold, if hovered || gutter_hovered { 1.0 } else { 0.92 })
                    } else if block.is_error() {
                        (red, if hovered || gutter_hovered { 1.0 } else { 0.90 })
                    } else if block.is_running() {
                        (accent, 0.92)
                    } else if hovered || gutter_hovered {
                        (green, 0.98)
                    } else {
                        (mix_color(pane_bg, green, 145), 0.62)
                    };
                    self.rounded_rect(
                        PixelRect {
                            origin: PixelPosition {
                                x: draw_x,
                                y: block_top.saturating_add(rail_inset_y),
                            },
                            size: PixelSize {
                                width: rw,
                                height: rh,
                            },
                        },
                        draw_w / 2,
                        0,
                        rail_color,
                        rail_alpha,
                    );
                }
            } else if block.prompt_row >= first_row {
                // Active idle prompt: sleek subtle accent pill in the left gutter.
                let row_h = top_r.size.height.get();
                let pill_h = (row_h.saturating_mul(55) / 100).max(6);
                let pill_y = block_top + row_h.saturating_sub(pill_h) / 2;
                if let (Some(rw), Some(rh)) = (NonZeroU32::new(rail_w), NonZeroU32::new(pill_h)) {
                    self.rounded_rect(
                        PixelRect {
                            origin: PixelPosition {
                                x: rail_x,
                                y: pill_y,
                            },
                            size: PixelSize {
                                width: rw,
                                height: rh,
                            },
                        },
                        rail_w / 2,
                        0,
                        accent,
                        0.78,
                    );
                }
            }
        }

        self.area = prev_area;
        start..self.instances.len() as u32
    }

    pub(super) fn command_block_overlays(
        &mut self,
        frame: &PresentationFrame<'_>,
        blocks: &[blocks::CommandBlock],
        hovered_block: Option<blocks::HoveredBlock>,
        scrollbar_reserve: u32,
        scale: f64,
    ) {
        let source = frame.viewport.source();
        let first_row = frame.viewport.range().start;
        let rows = usize::from(frame.geometry.pty_size().rows);
        let ui_metrics = self.chrome_metrics;
        let ui_cell_h = ui_metrics.cell.height.get();
        let scale_f = scale.max(1.0);
        let floor = |logical: f64| (logical * scale_f).round().max(1.0) as u32;

        let bg = frame.palette.color(if frame.reverse_video {
            PaletteEntry::Foreground
        } else {
            PaletteEntry::Background
        });
        let fg = frame.palette.color(if frame.reverse_video {
            PaletteEntry::Background
        } else {
            PaletteEntry::Foreground
        });
        let green = frame.palette.color(PaletteEntry::Indexed(2));
        let red = frame.palette.color(PaletteEntry::Indexed(1));
        let gold = frame.palette.color(PaletteEntry::Indexed(3));
        let accent = frame.palette.color(PaletteEntry::Indexed(4));

        // Sticky Command Header: pinned glass strip when a long block's prompt scrolled off-screen.
        let mut sticky_prompt_row = None;
        if let Some(sticky) = blocks::sticky_block(blocks, first_row)
            && let Some(cmd) = blocks::extract_command(source, &sticky)
        {
            let has_output = blocks::has_output(source, &sticky);
            if let Some(layout) = blocks::StickyHeaderLayout::compute(
                self.area,
                ui_metrics.cell,
                scale,
                scrollbar_reserve,
                true,
                has_output,
            ) {
                sticky_prompt_row = Some(sticky.prompt_row);
                let sticky_hover = hovered_block
                    .filter(|h| h.on_sticky && h.prompt_row == sticky.prompt_row)
                    .and_then(|h| h.hovered_action);
                let body_hovered = sticky_hover == Some(blocks::BlockAction::JumpToPrompt);

                let status_color = if sticky.bookmarked {
                    gold
                } else if sticky.is_error() {
                    red
                } else if sticky.is_running() {
                    accent
                } else {
                    green
                };
                let bar_fill = mix_color(bg, fg, if body_hovered { 28 } else { 20 });
                let bar_border = if body_hovered {
                    mix_color(bg, accent, 150)
                } else {
                    mix_color(bg, status_color, 105)
                };
                self.floating_card(layout.bar, layout.radius, bg, bar_fill, bar_border, 0.95);
                self.rounded_rect(
                    layout.accent_bar,
                    layout.accent_bar.size.width.get() / 2,
                    0,
                    status_color,
                    0.95,
                );

                // Optional compact status pill before the command text when space allows.
                let mut text_x = layout.text_x;
                let mut text_max_w = layout.text_max_w;
                if let Some(badge) = blocks::format_status_badge(&sticky) {
                    let badge_text_w = self.font.measure_text(ui_metrics, &badge);
                    let badge_pad_x = (ui_metrics.cell.width.get() * 2 / 3).max(floor(5.0));
                    let badge_w = badge_text_w.saturating_add(badge_pad_x * 2);
                    let badge_gap = floor(8.0);
                    if text_max_w
                        > badge_w.saturating_add(badge_gap + ui_metrics.cell.width.get() * 6)
                        && let (Some(bw), Some(bh)) = (
                            NonZeroU32::new(badge_w),
                            NonZeroU32::new(ui_cell_h.saturating_add(floor(2.0))),
                        )
                    {
                        let badge_y = layout.bar.origin.y
                            + layout.bar.size.height.get().saturating_sub(bh.get()) / 2;
                        let badge_rect = PixelRect {
                            origin: PixelPosition {
                                x: text_x,
                                y: badge_y,
                            },
                            size: PixelSize {
                                width: bw,
                                height: bh,
                            },
                        };
                        let (b_fill, b_border, b_fg) = if sticky.bookmarked {
                            (
                                mix_color(bg, gold, 44),
                                mix_color(bg, gold, 165),
                                mix_color(fg, gold, 200),
                            )
                        } else if sticky.is_error() {
                            (
                                mix_color(bg, red, 48),
                                mix_color(bg, red, 165),
                                mix_color(fg, red, 200),
                            )
                        } else if sticky.is_running() {
                            (
                                mix_color(bg, accent, 38),
                                mix_color(bg, accent, 145),
                                mix_color(fg, accent, 195),
                            )
                        } else {
                            (
                                mix_color(bg, green, 32),
                                mix_color(bg, green, 125),
                                mix_color(fg, green, 190),
                            )
                        };
                        self.rounded_rect(badge_rect, bh.get() / 2, 0, b_fill, 0.90);
                        self.rounded_rect(badge_rect, bh.get() / 2, 1, b_border, 0.72);
                        if let Some(tw) = NonZeroU32::new(badge_text_w.min(ATLAS_SIDE as u32)) {
                            self.glyph(
                                PixelRect {
                                    origin: PixelPosition {
                                        x: text_x + badge_pad_x,
                                        y: badge_y + bh.get().saturating_sub(ui_cell_h) / 2,
                                    },
                                    size: PixelSize {
                                        width: tw,
                                        height: ui_metrics.cell.height,
                                    },
                                },
                                &badge,
                                CHROME_LABEL_CACHE,
                                b_fg,
                                1.0,
                            );
                        }
                        let consumed = badge_w.saturating_add(badge_gap);
                        text_x = text_x.saturating_add(consumed);
                        text_max_w = text_max_w.saturating_sub(consumed);
                    }
                }

                // Single-line command preview.
                let single_line: std::borrow::Cow<'_, str> = if cmd.contains('\n') {
                    std::borrow::Cow::Owned(cmd.lines().next().unwrap_or("").trim().to_owned())
                } else {
                    std::borrow::Cow::Borrowed(cmd.as_str())
                };
                if text_max_w > 0 && !single_line.is_empty() {
                    let ty = layout.bar.origin.y
                        + layout.bar.size.height.get().saturating_sub(ui_cell_h) / 2;
                    let cmd_color = if body_hovered {
                        fg
                    } else {
                        mix_color(bg, fg, 225)
                    };
                    self.clamped_chrome_text(
                        PixelPosition { x: text_x, y: ty },
                        text_max_w,
                        &single_line,
                        cmd_color,
                    );
                }

                // Right-aligned quick action buttons on the sticky header.
                let icon_size = (ui_cell_h.saturating_mul(7) / 10).max(floor(11.0));
                let icon_gap = (ui_metrics.cell.width.get() / 2).max(floor(4.0));
                let btn_pad_x = (ui_metrics.cell.width.get() * 2 / 3).max(floor(5.0));
                let idle_label = mix_color(bg, fg, 210);
                for button in &layout.buttons {
                    let btn_hovered = sticky_hover == Some(button.action);
                    let btn_h = button.rect.size.height.get();
                    let btn_r = (btn_h / 3).clamp(floor(4.0), btn_h / 2);
                    if btn_hovered {
                        self.rounded_rect(button.rect, btn_r, 0, mix_color(bg, accent, 58), 0.92);
                    }
                    let icon = match button.action {
                        blocks::BlockAction::CopyCommand
                        | blocks::BlockAction::CopyOutput
                        | blocks::BlockAction::CopyFull => svg::icons::COPY,
                        _ => svg::icons::PLAY,
                    };
                    let icon_color = if btn_hovered {
                        if button.action == blocks::BlockAction::Rerun {
                            green
                        } else {
                            accent
                        }
                    } else if button.action == blocks::BlockAction::Rerun {
                        mix_color(bg, green, 200)
                    } else {
                        mix_color(bg, accent, 200)
                    };
                    let ix = button.rect.origin.x + btn_pad_x;
                    let iy = button.rect.origin.y + btn_h.saturating_sub(icon_size) / 2;
                    self.svg_mask(
                        PixelPosition { x: ix, y: iy },
                        icon_size,
                        icon,
                        icon_color,
                        0.98,
                    );
                    let lx = ix + icon_size + icon_gap;
                    let ly = button.rect.origin.y + btn_h.saturating_sub(ui_cell_h) / 2;
                    let avail = button
                        .rect
                        .origin
                        .x
                        .saturating_add(button.rect.size.width.get())
                        .saturating_sub(lx + btn_pad_x / 2);
                    if avail > 0 {
                        self.clamped_chrome_text(
                            PixelPosition { x: lx, y: ly },
                            avail,
                            button.label,
                            if btn_hovered { fg } else { idle_label },
                        );
                    }
                }
            }
        }

        for block in blocks {
            if sticky_prompt_row == Some(block.prompt_row) {
                continue;
            }
            let is_hovered =
                hovered_block.is_some_and(|h| !h.on_sticky && h.prompt_row == block.prompt_row);
            if is_hovered {
                let has_command = blocks::has_command(source, block);
                let has_output = blocks::has_output(source, block);
                let vis_start = block.prompt_row.saturating_sub(first_row).min(rows - 1);
                let anchor_y = frame
                    .geometry
                    .row_rect(vis_start, 0..1)
                    .map_or(0, |r| r.origin.y);
                if let Some(toolbar) = blocks::BlockToolbarLayout::compute(
                    self.area,
                    anchor_y,
                    ui_metrics.cell,
                    scale,
                    scrollbar_reserve,
                    has_command,
                    has_output,
                ) {
                    let pill_fill = mix_color(bg, fg, 26);
                    let pill_border = mix_color(bg, accent, 140);
                    self.floating_card(
                        toolbar.pill,
                        toolbar.radius,
                        bg,
                        pill_fill,
                        pill_border,
                        0.97,
                    );

                    let hovered_action = hovered_block.and_then(|h| h.hovered_action);
                    let icon_size = (ui_cell_h.saturating_mul(7) / 10).max(floor(11.0));
                    let icon_gap = (ui_metrics.cell.width.get() / 2).max(floor(4.0));
                    let btn_pad_x = (ui_metrics.cell.width.get() * 3 / 4).max(floor(6.0));
                    let idle_label = mix_color(bg, fg, 210);

                    for button in &toolbar.buttons {
                        let btn_hovered = hovered_action == Some(button.action);
                        let btn_h = button.rect.size.height.get();
                        let btn_r = (btn_h / 3).clamp(floor(4.0), btn_h / 2);
                        if btn_hovered {
                            self.rounded_rect(
                                button.rect,
                                btn_r,
                                0,
                                mix_color(bg, accent, 58),
                                0.92,
                            );
                        }
                        let icon = match button.action {
                            blocks::BlockAction::CopyCommand
                            | blocks::BlockAction::CopyOutput
                            | blocks::BlockAction::CopyFull => svg::icons::COPY,
                            _ => svg::icons::PLAY,
                        };
                        let icon_color = if btn_hovered {
                            if button.action == blocks::BlockAction::Rerun {
                                green
                            } else {
                                accent
                            }
                        } else if button.action == blocks::BlockAction::Rerun {
                            mix_color(bg, green, 200)
                        } else {
                            mix_color(bg, accent, 200)
                        };
                        let ix = button.rect.origin.x + btn_pad_x;
                        let iy = button.rect.origin.y + btn_h.saturating_sub(icon_size) / 2;
                        self.svg_mask(
                            PixelPosition { x: ix, y: iy },
                            icon_size,
                            icon,
                            icon_color,
                            0.98,
                        );
                        let lx = ix + icon_size + icon_gap;
                        let ly = button.rect.origin.y + btn_h.saturating_sub(ui_cell_h) / 2;
                        let avail = button
                            .rect
                            .origin
                            .x
                            .saturating_add(button.rect.size.width.get())
                            .saturating_sub(lx + btn_pad_x / 2);
                        if avail > 0 {
                            self.clamped_chrome_text(
                                PixelPosition { x: lx, y: ly },
                                avail,
                                button.label,
                                if btn_hovered { fg } else { idle_label },
                            );
                        }
                    }
                    continue;
                }
            }

            // Compact right-aligned status pill on finished/running visible prompt rows.
            if (!block.is_finished() && !block.is_running()) || block.prompt_row < first_row {
                continue;
            }
            let vis_row = block.prompt_row - first_row;
            if vis_row >= rows {
                continue;
            }
            let Some(badge) = blocks::format_status_badge(block) else {
                continue;
            };
            let Some(row_rect) = frame.geometry.row_rect(vis_row, 0..1) else {
                continue;
            };
            let text_w = self.font.measure_text(ui_metrics, &badge);
            let pad_x = (ui_metrics.cell.width.get() * 3 / 4).max(floor(7.0));
            let badge_w = text_w.saturating_add(pad_x * 2);
            let badge_h = ui_cell_h
                .saturating_add(floor(2.0))
                .min(row_rect.size.height.get());
            let margin = floor(10.0);
            let right_edge = self
                .area
                .width
                .get()
                .saturating_sub(scrollbar_reserve.saturating_add(margin));
            if right_edge <= badge_w + margin {
                continue;
            }
            let badge_x = right_edge - badge_w;

            // Avoid overlapping prompt/command text on the right side of the prompt row.
            let text_end_x = source
                .row(block.prompt_row)
                .and_then(|row| {
                    let end = row.wrap_end().unwrap_or(row.cells().len());
                    source.selection_bounds(
                        CellPosition {
                            row: block.prompt_row,
                            column: 0,
                        }..CellPosition {
                            row: block.prompt_row,
                            column: end,
                        },
                    )
                })
                .filter(|bounds| bounds.end.column > 0)
                .and_then(|bounds| frame.geometry.row_rect(vis_row, 0..bounds.end.column))
                .map_or(0, |r| r.origin.x.saturating_add(r.size.width.get()));
            if text_end_x.saturating_add(margin) > badge_x {
                continue;
            }

            let badge_y =
                row_rect.origin.y + row_rect.size.height.get().saturating_sub(badge_h) / 2;
            if let (Some(bw), Some(bh)) = (NonZeroU32::new(badge_w), NonZeroU32::new(badge_h)) {
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
                let (fill, border, text_color, alpha) = if block.bookmarked {
                    (
                        mix_color(bg, gold, 44),
                        mix_color(bg, gold, 165),
                        mix_color(fg, gold, 200),
                        0.92,
                    )
                } else if block.is_error() {
                    (
                        mix_color(bg, red, 48),
                        mix_color(bg, red, 175),
                        mix_color(fg, red, 200),
                        0.92,
                    )
                } else if block.is_running() {
                    (
                        mix_color(bg, accent, 38),
                        mix_color(bg, accent, 150),
                        mix_color(fg, accent, 195),
                        0.90,
                    )
                } else {
                    (
                        mix_color(bg, green, 32),
                        mix_color(bg, green, 125),
                        mix_color(fg, green, 190),
                        0.86,
                    )
                };
                self.rounded_rect(badge_rect, bh.get() / 2, 0, fill, alpha);
                self.rounded_rect(badge_rect, bh.get() / 2, 1, border, 0.72);
                if let Some(tw) = NonZeroU32::new(text_w.min(ATLAS_SIDE as u32)) {
                    self.glyph(
                        PixelRect {
                            origin: PixelPosition {
                                x: badge_x + pad_x,
                                y: badge_y + bh.get().saturating_sub(ui_cell_h) / 2,
                            },
                            size: PixelSize {
                                width: tw,
                                height: ui_metrics.cell.height,
                            },
                        },
                        &badge,
                        CHROME_LABEL_CACHE,
                        text_color,
                        0.96,
                    );
                }
            }
        }
    }
}
