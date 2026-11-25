//! Pane and window overlay rendering: IME preedit, quick hints, search bar,
//! mode badge, scrollbar, progress bar, split dividers, and status toast notices.

use std::num::NonZeroU32;
use std::ops::Range;

use lune_terminal::{CellPosition, PaletteEntry, PixelPosition, PixelSize, Progress};

use crate::{PixelRect, PresentationFrame};

use super::super::hints::HintMode;
use super::super::ime::Preedit;
use super::super::paint::{NoticeKind, NoticeView};
use super::super::scrollbar::ScrollbarView;
use super::super::search::SearchBar;
use super::super::split::{Axis, Divider};
use super::super::svg;
use super::super::vi;
use super::{ATLAS_SIDE, LABEL_CACHE, TextRenderer, horizontal, luminance, mix_color};

impl TextRenderer {
    pub(super) fn ime_preedit(
        &mut self,
        frame: &PresentationFrame<'_>,
        origin: CellPosition,
        preedit: &Preedit,
    ) {
        let columns = usize::from(frame.geometry.pty_size().columns);
        let rows = usize::from(frame.geometry.pty_size().rows);
        let cells = preedit.layout_cells(origin, columns, rows);
        if cells.is_empty() {
            return;
        }
        let bg = frame.palette.color(if frame.reverse_video {
            PaletteEntry::Foreground
        } else {
            PaletteEntry::Background
        });
        let cursor_color = frame.palette.color(PaletteEntry::Cursor);
        let selected_fg = [
            frame.palette.color(PaletteEntry::Background),
            frame.palette.color(PaletteEntry::Foreground),
            frame.palette.color(PaletteEntry::Indexed(0)),
            frame.palette.color(PaletteEntry::Indexed(15)),
        ]
        .into_iter()
        .max_by_key(|&color| luminance(color).abs_diff(luminance(cursor_color)))
        .unwrap();
        let thickness = ((self.metrics.size / 14.0).round().max(1.0) as u32)
            .min(self.metrics.cell.height.get());
        let underline_y = (self.metrics.baseline.max(0) as u32 + thickness)
            .min(self.metrics.cell.height.get().saturating_sub(thickness));

        for cell in &cells {
            let Some(rect) = frame.geometry.row_rect(
                cell.position.row,
                cell.position.column..cell.position.column + cell.width,
            ) else {
                continue;
            };
            let (fill, fg) = if cell.selected {
                (cursor_color, selected_fg)
            } else {
                (bg, cursor_color)
            };
            self.rect(rect, fill, 1.0);
            if cell.text != " " {
                let cache = usize::from(cell.width > 1) << 2;
                self.glyph(rect, cell.text, cache, fg, 1.0);
            }
            self.rect(horizontal(rect, underline_y, thickness), fg, 1.0);
        }

        if let Some(caret) = preedit.cursor_cell(origin, columns, rows)
            && let Some(mut rect) = frame
                .geometry
                .row_rect(caret.row, caret.column..caret.column + 1)
        {
            rect.size.width = NonZeroU32::new(2).unwrap().min(rect.size.width);
            self.rect(rect, cursor_color, 1.0);
        }
    }

    pub(super) fn hints_overlay(&mut self, frame: &PresentationFrame<'_>, hints: &HintMode) {
        let visible = frame.viewport.range();
        let columns = usize::from(frame.geometry.pty_size().columns);
        let bg = frame.palette.color(PaletteEntry::Background);
        let fg = frame.palette.color(PaletteEntry::Foreground);
        let accent = frame.palette.color(PaletteEntry::Indexed(3));
        let badge_fg = [
            bg,
            fg,
            frame.palette.color(PaletteEntry::Indexed(0)),
            frame.palette.color(PaletteEntry::Indexed(15)),
        ]
        .into_iter()
        .max_by_key(|&color| luminance(color).abs_diff(luminance(accent)))
        .unwrap();
        let cell_h = self.metrics.cell.height.get();
        let cell_w = self.metrics.cell.width.get();
        let pad_x = (cell_w / 3).max(3);
        let radius = (cell_h / 5).clamp(3, 6);
        let underline_h = ((self.metrics.size / 14.0).round().max(1.0) as u32).min(cell_h);
        let underline_y = (self.metrics.baseline.max(0) as u32 + underline_h)
            .min(cell_h.saturating_sub(underline_h));

        for hint in hints.active_hints() {
            // Subtle span underline + tint across every visible row of the hint.
            let first = hint.range.start.row.max(visible.start);
            let last = hint.range.end.row.min(visible.end.saturating_sub(1));
            for abs_row in first..=last {
                if let Some(span) = hint.columns_on_row(abs_row, columns)
                    && let Some(rect) = frame.geometry.row_rect(abs_row - visible.start, span)
                {
                    self.rect(rect, accent, 0.16);
                    self.rect(horizontal(rect, underline_y, underline_h), accent, 0.75);
                }
            }

            // High-contrast label pill anchored at the start of the hint span.
            if !visible.contains(&hint.range.start.row) {
                continue;
            }
            let local_row = hint.range.start.row - visible.start;
            let Some(anchor) = frame.geometry.row_rect(
                local_row,
                hint.range.start.column..hint.range.start.column + 1,
            ) else {
                continue;
            };
            let display_label = hint.label.to_ascii_uppercase();
            let text_w = self.font.measure_text(self.metrics, &display_label);
            let pill_w = text_w.saturating_add(pad_x * 2).min(self.area.width.get());
            let Some(width) = NonZeroU32::new(pill_w) else {
                continue;
            };
            let x = anchor
                .origin
                .x
                .min(self.area.width.get().saturating_sub(pill_w));
            let pill = PixelRect {
                origin: PixelPosition {
                    x,
                    y: anchor.origin.y,
                },
                size: PixelSize {
                    width,
                    height: self.metrics.cell.height,
                },
            };
            self.rounded_rect(pill, radius, 0, accent, 0.96);

            let typed_len = hints.typed.len().min(display_label.len());
            let mut text_x = pill.origin.x + pad_x;
            if typed_len > 0 {
                let prefix = &display_label[..typed_len];
                let pw = self.font.measure_text(self.metrics, prefix);
                if let Some(w) = NonZeroU32::new(pw.min(ATLAS_SIDE as u32)) {
                    self.glyph(
                        PixelRect {
                            origin: PixelPosition {
                                x: text_x,
                                y: pill.origin.y,
                            },
                            size: PixelSize {
                                width: w,
                                height: self.metrics.cell.height,
                            },
                        },
                        prefix,
                        LABEL_CACHE,
                        mix_color(accent, badge_fg, 120),
                        1.0,
                    );
                }
                text_x = text_x.saturating_add(pw);
            }
            let rest = &display_label[typed_len..];
            if !rest.is_empty() {
                let rw = self.font.measure_text(self.metrics, rest);
                if let Some(w) = NonZeroU32::new(rw.min(ATLAS_SIDE as u32)) {
                    self.glyph(
                        PixelRect {
                            origin: PixelPosition {
                                x: text_x,
                                y: pill.origin.y,
                            },
                            size: PixelSize {
                                width: w,
                                height: self.metrics.cell.height,
                            },
                        },
                        rest,
                        LABEL_CACHE,
                        badge_fg,
                        1.0,
                    );
                }
            }
        }

        if hints.typed.is_empty() {
            self.mode_badge(frame.palette, "HINTS");
        } else {
            let label = format!("HINTS · {}", hints.typed.to_ascii_uppercase());
            self.mode_badge(frame.palette, &label);
        }
    }

    pub(super) fn mode_badge(&mut self, palette: &lune_terminal::Palette, label: &str) {
        let cell_h = self.metrics.cell.height.get();
        let cell_w = self.metrics.cell.width.get();
        let pad_x = (cell_w * 2 / 3).max(6);
        let pad_y = (cell_h / 6).max(2);
        let margin = (cell_w / 2).max(6);
        let text_w = self.font.measure_text(self.metrics, label);
        let box_w = text_w.saturating_add(pad_x * 2).min(ATLAS_SIDE as u32);
        let box_h = cell_h.saturating_add(pad_y * 2);
        if text_w == 0
            || self.area.width.get() < box_w + margin * 2
            || self.area.height.get() < box_h + margin
        {
            return;
        }
        let Some(width) = NonZeroU32::new(box_w) else {
            return;
        };
        let Some(height) = NonZeroU32::new(box_h) else {
            return;
        };
        let rect = PixelRect {
            origin: PixelPosition {
                x: self.area.width.get().saturating_sub(box_w + margin),
                y: margin,
            },
            size: PixelSize { width, height },
        };
        let bg = palette.color(PaletteEntry::Background);
        let fg = palette.color(PaletteEntry::Foreground);
        let accent = palette.color(PaletteEntry::Indexed(4));
        let fill = mix_color(bg, fg, 22);
        let border = mix_color(bg, accent, 140);
        let radius = (box_h / 4).clamp(4, 9);
        self.floating_card(rect, radius, bg, fill, border, 0.94);
        if let Some(label_w) = NonZeroU32::new(text_w.min(ATLAS_SIDE as u32)) {
            self.glyph(
                PixelRect {
                    origin: PixelPosition {
                        x: rect.origin.x + pad_x,
                        y: rect.origin.y + pad_y,
                    },
                    size: PixelSize {
                        width: label_w,
                        height: self.metrics.cell.height,
                    },
                },
                label,
                LABEL_CACHE,
                accent,
                1.0,
            );
        }
    }

    pub(super) fn search_bar(
        &mut self,
        palette: &lune_terminal::Palette,
        search: &SearchBar,
        preedit: &str,
    ) {
        let cell_h = self.metrics.cell.height.get();
        let cell_w = self.metrics.cell.width.get();
        let pad_x = (cell_w / 2).max(4);
        let pad_y = (cell_h / 5).max(3);
        let margin = (cell_w / 2).max(6);
        let box_h = cell_h.saturating_add(pad_y * 2);
        if self.area.height.get() < box_h || self.area.width.get() < cell_w * 8 {
            return;
        }
        let max_w = self
            .area
            .width
            .get()
            .saturating_sub(margin * 2)
            .min(ATLAS_SIDE as u32);
        let desired_w = (cell_w * 36).min(max_w);
        let Some(box_w) = NonZeroU32::new(desired_w) else {
            return;
        };
        let Some(box_h_nz) = NonZeroU32::new(box_h) else {
            return;
        };
        let rect = PixelRect {
            origin: PixelPosition {
                x: self.area.width.get().saturating_sub(box_w.get() + margin),
                y: margin.min(self.area.height.get().saturating_sub(box_h)),
            },
            size: PixelSize {
                width: box_w,
                height: box_h_nz,
            },
        };

        let bg = palette.color(PaletteEntry::Background);
        let fg = palette.color(PaletteEntry::Foreground);
        let accent = if search.vi_mode && search.direction == vi::Direction::Backward {
            palette.color(PaletteEntry::Indexed(3))
        } else {
            palette.color(PaletteEntry::Indexed(4))
        };
        let red = palette.color(PaletteEntry::Indexed(1));
        let fill = mix_color(bg, fg, 22);
        let border = if search.invalid_regex {
            red
        } else {
            mix_color(bg, fg, 95)
        };
        let radius = (box_h / 4).clamp(4, 10);
        self.floating_card(rect, radius, bg, fill, border, 0.96);

        let text_y = rect.origin.y + pad_y;
        let mut inner_left = rect.origin.x + pad_x;
        let mut cursor_right = rect.origin.x + rect.size.width.get().saturating_sub(pad_x);
        let badge_gap = (cell_w / 2).max(4);

        if search.vi_mode {
            let prefix = match search.direction {
                vi::Direction::Forward => "/",
                vi::Direction::Backward => "?",
            };
            let prefix_w = self.font.measure_text(self.metrics, prefix);
            if let Some(width) =
                NonZeroU32::new(prefix_w.min(cursor_right.saturating_sub(inner_left)))
            {
                self.glyph(
                    PixelRect {
                        origin: PixelPosition {
                            x: inner_left,
                            y: text_y,
                        },
                        size: PixelSize {
                            width,
                            height: self.metrics.cell.height,
                        },
                    },
                    prefix,
                    LABEL_CACHE,
                    accent,
                    1.0,
                );
                inner_left = inner_left.saturating_add(width.get() + badge_gap);
            }
        } else if search.row_scope.is_some() {
            let scope_label = "Block";
            let scope_w = self.font.measure_text(self.metrics, scope_label);
            let scope_pad = (cell_w / 2).max(4);
            let pill_w = scope_w.saturating_add(scope_pad * 2);
            if cursor_right.saturating_sub(inner_left) > pill_w + cell_w * 4
                && let (Some(pw), Some(ph)) = (NonZeroU32::new(pill_w), NonZeroU32::new(cell_h))
            {
                let pill_rect = PixelRect {
                    origin: PixelPosition {
                        x: inner_left,
                        y: text_y,
                    },
                    size: PixelSize {
                        width: pw,
                        height: ph,
                    },
                };
                self.rounded_rect(
                    pill_rect,
                    (cell_h / 4).clamp(3, 6),
                    0,
                    mix_color(bg, accent, 52),
                    0.92,
                );
                if let Some(tw) = NonZeroU32::new(scope_w.min(ATLAS_SIDE as u32)) {
                    self.glyph(
                        PixelRect {
                            origin: PixelPosition {
                                x: inner_left + scope_pad,
                                y: text_y,
                            },
                            size: PixelSize {
                                width: tw,
                                height: self.metrics.cell.height,
                            },
                        },
                        scope_label,
                        LABEL_CACHE,
                        accent,
                        1.0,
                    );
                }
                inner_left = inner_left.saturating_add(pill_w + badge_gap);
            }
        } else {
            let icon_size = (cell_h * 3 / 4).clamp(10, cell_h);
            if cursor_right > inner_left + icon_size + badge_gap
                && self.svg_mask(
                    PixelPosition {
                        x: inner_left,
                        y: text_y + cell_h.saturating_sub(icon_size) / 2,
                    },
                    icon_size,
                    svg::icons::SEARCH,
                    mix_color(bg, fg, 150),
                    1.0,
                )
            {
                inner_left = inner_left.saturating_add(icon_size + badge_gap);
            }
        }

        // Right-aligned toggles: `.*` (regex) and `Aa` (case-sensitive).
        for (label, active) in [(".*", search.regex_mode), ("Aa", search.case_sensitive)] {
            let w = self.font.measure_text(self.metrics, label);
            if w == 0 || cursor_right < inner_left + w + badge_gap {
                continue;
            }
            let x = cursor_right - w;
            let color = if active {
                accent
            } else {
                mix_color(bg, fg, 110)
            };
            if let Some(width) = NonZeroU32::new(w.min(ATLAS_SIDE as u32)) {
                self.glyph(
                    PixelRect {
                        origin: PixelPosition { x, y: text_y },
                        size: PixelSize {
                            width,
                            height: self.metrics.cell.height,
                        },
                    },
                    label,
                    LABEL_CACHE,
                    color,
                    1.0,
                );
            }
            cursor_right = x.saturating_sub(badge_gap);
        }

        // Match count or `invalid` status.
        let status = search.status_label();
        if !status.is_empty() {
            let w = self.font.measure_text(self.metrics, &status);
            if w != 0 && cursor_right >= inner_left + w + badge_gap {
                let x = cursor_right - w;
                let color = if search.invalid_regex {
                    red
                } else if search.matches.is_empty() {
                    mix_color(bg, fg, 110)
                } else {
                    mix_color(bg, fg, 185)
                };
                if let Some(width) = NonZeroU32::new(w.min(ATLAS_SIDE as u32)) {
                    self.glyph(
                        PixelRect {
                            origin: PixelPosition { x, y: text_y },
                            size: PixelSize {
                                width,
                                height: self.metrics.cell.height,
                            },
                        },
                        &status,
                        LABEL_CACHE,
                        color,
                        1.0,
                    );
                }
                cursor_right = x.saturating_sub(badge_gap);
            }
        }

        // Query + IME preedit (or placeholder when empty).
        let query_avail = cursor_right.saturating_sub(inner_left);
        if query_avail == 0 {
            return;
        }
        let full_query = if preedit.is_empty() {
            std::borrow::Cow::Borrowed(search.query.as_str())
        } else {
            std::borrow::Cow::Owned(format!("{}{preedit}", search.query))
        };
        if full_query.is_empty() {
            let placeholder = if search.vi_mode {
                match search.direction {
                    vi::Direction::Forward => "Search forward",
                    vi::Direction::Backward => "Search backward",
                }
            } else {
                "Search"
            };
            let w = self
                .font
                .measure_text(self.metrics, placeholder)
                .min(query_avail)
                .min(ATLAS_SIDE as u32);
            if let Some(width) = NonZeroU32::new(w) {
                self.glyph(
                    PixelRect {
                        origin: PixelPosition {
                            x: inner_left,
                            y: text_y,
                        },
                        size: PixelSize {
                            width,
                            height: self.metrics.cell.height,
                        },
                    },
                    placeholder,
                    LABEL_CACHE,
                    mix_color(bg, fg, 100),
                    1.0,
                );
            }
            if let Some(bar_w) = NonZeroU32::new(2.min(query_avail)) {
                self.rect(
                    PixelRect {
                        origin: PixelPosition {
                            x: inner_left,
                            y: text_y,
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
            // Keep the tail of long queries visible inside the input box.
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
                            x: inner_left,
                            y: text_y,
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
            let caret_x = inner_left.saturating_add(draw_w);
            if caret_x < cursor_right
                && let Some(bar_w) = NonZeroU32::new(2.min(cursor_right - caret_x))
            {
                self.rect(
                    PixelRect {
                        origin: PixelPosition {
                            x: caret_x,
                            y: text_y,
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

    pub(super) fn scrollbar(&mut self, palette: &lune_terminal::Palette, view: ScrollbarView) {
        let background = palette.color(PaletteEntry::Background);
        let foreground = palette.color(PaletteEntry::Foreground);
        let thumb_color = palette.color(PaletteEntry::Cursor);
        if view.highlighted {
            let track_color = mix_color(background, foreground, 42);
            self.rounded_rect(view.track, view.radius, 0, track_color, 0.30);
            self.rounded_rect(view.thumb, view.radius, 0, thumb_color, 0.90);
        } else {
            self.rounded_rect(view.thumb, view.radius, 0, thumb_color, 0.72);
        }
    }

    pub(in super::super) fn prepare_chrome(
        &mut self,
        area: PixelSize,
        palette: &lune_terminal::Palette,
        dividers: &[Divider],
        focused_rect: Option<PixelRect>,
        notice: Option<NoticeView<'_>>,
        top_inset: u32,
    ) -> Range<u32> {
        let start = self.instances.len() as u32;
        self.area = area;
        if !dividers.is_empty() {
            let bg = palette.color(PaletteEntry::Background);
            let fg = palette.color(PaletteEntry::Foreground);
            let base = mix_color(bg, fg, 60);
            let accent = palette.color(PaletteEntry::Indexed(4));
            for divider in dividers {
                self.rect(divider.rect, base, 0.85);
                if let Some(focused) = focused_rect
                    && let Some(segment) = divider_focus_segment(*divider, focused)
                {
                    self.rect(segment, accent, 0.9);
                }
            }
        }
        if let Some(notice) = notice {
            self.notice_toast(notice, palette, top_inset);
        }
        start..self.instances.len() as u32
    }

    pub(super) fn progress_bar(
        &mut self,
        palette: &lune_terminal::Palette,
        progress: Progress,
        phase: f32,
    ) {
        if progress == Progress::None {
            return;
        }
        let width = self.area.width.get();
        let height = 3.min(self.area.height.get());
        let Some(bar_height) = NonZeroU32::new(height) else {
            return;
        };
        let track = PixelRect {
            origin: PixelPosition { x: 0, y: 0 },
            size: PixelSize {
                width: self.area.width,
                height: bar_height,
            },
        };
        let (color, fill_rect) = match progress {
            Progress::None => return,
            Progress::Normal(percent) => {
                let fill = ((u64::from(width) * u64::from(percent)) / 100) as u32;
                let fill = if percent == 0 {
                    0
                } else {
                    fill.max(2).min(width)
                };
                (
                    palette.color(PaletteEntry::Indexed(4)),
                    NonZeroU32::new(fill).map(|w| PixelRect {
                        origin: PixelPosition { x: 0, y: 0 },
                        size: PixelSize {
                            width: w,
                            height: bar_height,
                        },
                    }),
                )
            }
            Progress::Error(percent) => {
                let pct = if percent == 0 { 100 } else { percent };
                let fill = ((u64::from(width) * u64::from(pct)) / 100).max(2) as u32;
                (
                    palette.color(PaletteEntry::Indexed(1)),
                    NonZeroU32::new(fill.min(width)).map(|w| PixelRect {
                        origin: PixelPosition { x: 0, y: 0 },
                        size: PixelSize {
                            width: w,
                            height: bar_height,
                        },
                    }),
                )
            }
            Progress::Paused(percent) => {
                let fill = ((u64::from(width) * u64::from(percent)) / 100) as u32;
                let fill = if percent == 0 {
                    0
                } else {
                    fill.max(2).min(width)
                };
                (
                    palette.color(PaletteEntry::Indexed(3)),
                    NonZeroU32::new(fill).map(|w| PixelRect {
                        origin: PixelPosition { x: 0, y: 0 },
                        size: PixelSize {
                            width: w,
                            height: bar_height,
                        },
                    }),
                )
            }
            Progress::Indeterminate => {
                let seg = ((width as f32 * 0.28).round() as u32).clamp(12.min(width), width);
                let travel = (width + seg) as f32;
                let pos = (phase * travel) as i32 - seg as i32;
                let start = pos.max(0) as u32;
                let end = (pos + seg as i32).clamp(0, width as i32) as u32;
                (
                    palette.color(PaletteEntry::Indexed(6)),
                    NonZeroU32::new(end.saturating_sub(start)).map(|w| PixelRect {
                        origin: PixelPosition { x: start, y: 0 },
                        size: PixelSize {
                            width: w,
                            height: bar_height,
                        },
                    }),
                )
            }
        };
        self.rect(track, color, 0.2);
        if let Some(fill) = fill_rect {
            self.rect(fill, color, 0.95);
        }
    }

    fn notice_toast(
        &mut self,
        notice: NoticeView<'_>,
        palette: &lune_terminal::Palette,
        top_inset: u32,
    ) {
        let text = notice.text.trim();
        if text.is_empty() {
            return;
        }
        let cell_h = self.metrics.cell.height.get();
        let cell_w = self.metrics.cell.width.get();
        let pad_x = cell_w.max(10);
        let pad_y = (cell_h / 4).max(4);
        let margin = (cell_h / 2).max(10);
        let dot_w = (cell_w / 3).clamp(3, 5);
        let dot_gap = (cell_w * 2 / 3).max(6);
        let box_h = cell_h.saturating_add(pad_y * 2);
        if self.area.height.get() < top_inset + box_h + margin
            || self.area.width.get() < pad_x * 2 + dot_w
        {
            return;
        }

        let max_box_w = self
            .area
            .width
            .get()
            .saturating_sub(margin * 2)
            .min(ATLAS_SIDE as u32);
        let chrome_w = pad_x * 2 + dot_w + dot_gap;
        let max_text_w = max_box_w.saturating_sub(chrome_w);
        if max_text_w == 0 {
            return;
        }

        let (display, text_w) = self.truncate_to_width(text, max_text_w);

        let box_w = text_w.saturating_add(chrome_w).min(max_box_w);
        let Some(width) = NonZeroU32::new(box_w) else {
            return;
        };
        let Some(height) = NonZeroU32::new(box_h) else {
            return;
        };
        let rect = PixelRect {
            origin: PixelPosition {
                x: self.area.width.get().saturating_sub(box_w + margin),
                y: top_inset.saturating_add(margin),
            },
            size: PixelSize { width, height },
        };

        let bg = palette.color(PaletteEntry::Background);
        let fg = palette.color(PaletteEntry::Foreground);
        let accent = match notice.kind {
            NoticeKind::Success => palette.color(PaletteEntry::Indexed(2)),
            NoticeKind::Warning => palette.color(PaletteEntry::Indexed(3)),
            NoticeKind::Error => palette.color(PaletteEntry::Indexed(1)),
        };
        let fill = mix_color(bg, fg, 22);
        let border = mix_color(bg, accent, 155);
        let radius = (box_h / 4).clamp(6, 10);
        self.floating_card(rect, radius, bg, fill, border, 0.96);

        let indicator_h = (cell_h * 2 / 3).max(6).min(cell_h);
        if let (Some(iw), Some(ih)) = (NonZeroU32::new(dot_w), NonZeroU32::new(indicator_h)) {
            let indicator = PixelRect {
                origin: PixelPosition {
                    x: rect.origin.x + pad_x,
                    y: rect.origin.y + (box_h - indicator_h) / 2,
                },
                size: PixelSize {
                    width: iw,
                    height: ih,
                },
            };
            self.rounded_rect(indicator, dot_w / 2, 0, accent, 0.95);
        }

        if let Some(tw) = NonZeroU32::new(text_w.min(ATLAS_SIDE as u32)) {
            self.glyph(
                PixelRect {
                    origin: PixelPosition {
                        x: rect.origin.x + pad_x + dot_w + dot_gap,
                        y: rect.origin.y + pad_y,
                    },
                    size: PixelSize {
                        width: tw,
                        height: self.metrics.cell.height,
                    },
                },
                &display,
                LABEL_CACHE,
                fg,
                1.0,
            );
        }
    }
}

fn divider_focus_segment(divider: Divider, focused: PixelRect) -> Option<PixelRect> {
    let dr = divider.rect;
    let dx0 = dr.origin.x;
    let dy0 = dr.origin.y;
    let dx1 = dx0 + dr.size.width.get();
    let dy1 = dy0 + dr.size.height.get();
    let fx0 = focused.origin.x;
    let fy0 = focused.origin.y;
    let fx1 = fx0 + focused.size.width.get();
    let fy1 = fy0 + focused.size.height.get();
    match divider.axis {
        Axis::Columns if dx1 == fx0 || fx1 == dx0 => {
            let y0 = dy0.max(fy0);
            let y1 = dy1.min(fy1);
            let height = NonZeroU32::new(y1.saturating_sub(y0))?;
            Some(PixelRect {
                origin: PixelPosition { x: dx0, y: y0 },
                size: PixelSize {
                    width: dr.size.width,
                    height,
                },
            })
        }
        Axis::Rows if dy1 == fy0 || fy1 == dy0 => {
            let x0 = dx0.max(fx0);
            let x1 = dx1.min(fx1);
            let width = NonZeroU32::new(x1.saturating_sub(x0))?;
            Some(PixelRect {
                origin: PixelPosition { x: x0, y: dy0 },
                size: PixelSize {
                    width,
                    height: dr.size.height,
                },
            })
        }
        _ => None,
    }
}
