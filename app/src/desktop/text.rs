use std::collections::HashMap;
use std::num::NonZeroU32;
use std::ops::Range;

use guillotiere::{Allocation, AtlasAllocator, size2};
use lune_terminal::{
    CellPosition, CellText, Color, CursorShape, DisplayUnit, HyperlinkId, PaletteEntry,
    PixelPosition, PixelSize, Progress, Rgb, SizedTextRow, Style, Underline,
};

use crate::gpu::{create_quad_pipeline, linear_colors, write_instances};
use crate::{PixelRect, PresentationFrame, PresentationStyle, PresentedCursor};

use super::blocks;
use super::box_drawing;
use super::confirm;
use super::contrast;
use super::font::{Font, FontConfig, FontMetrics};
use super::hints::HintMode;
use super::ime::Preedit;
use super::ligature;
use super::paint::{NoticeKind, NoticeView, PaneOverlay};
use super::palette::{self, CommandPalette};
use super::scrollbar::ScrollbarView;
use super::search::SearchBar;
use super::selection::SelectionKind;
use super::split::{Axis, Divider};
use super::svg::{self, SvgCache, SvgMode};
use super::tab_bar;
use super::titlebar::{self, TitlebarAction};
use super::url::HoveredLink;
use super::vi;

const ATLAS_SIDE: i32 = 1024;
const MAX_ATLAS_LAYERS: u32 = 32;
const CHROME_FONT_SCALE: f32 = 0.82;
const LABEL_CACHE: usize = 8;
const CHROME_LABEL_CACHE: usize = 9;
const ICON_CACHE: usize = 10;
const TAB_LABEL_CACHE: usize = ICON_CACHE + 4;
const LIGATURE_CACHE: usize = TAB_LABEL_CACHE + 1;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Quad {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 4],
    sprite: [u32; 2],
}

impl Quad {
    fn new(
        area: PixelSize,
        rect: PixelRect,
        color: Rgb,
        alpha: f32,
        uv: [f32; 4],
        sprite: [u32; 2],
    ) -> Self {
        let scale_x = 2.0 / area.width.get() as f32;
        let scale_y = 2.0 / area.height.get() as f32;
        let lut = linear_colors();
        Self {
            rect: [
                rect.origin.x as f32 * scale_x - 1.0,
                1.0 - rect.origin.y as f32 * scale_y,
                rect.size.width.get() as f32 * scale_x,
                -(rect.size.height.get() as f32) * scale_y,
            ],
            uv,
            color: [
                lut[color.red as usize],
                lut[color.green as usize],
                lut[color.blue as usize],
                alpha,
            ],
            sprite,
        }
    }
}

#[derive(Clone, Copy)]
struct Sprite {
    allocation: Allocation,
    layer: usize,
    colored: bool,
    generation: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct SizedRaster {
    width: u32,
    height: u32,
    font_size: u32,
    baseline: i32,
    rendition: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct SvgSpriteKey {
    source: usize,
    width: u32,
    height: u32,
    colored: bool,
}

#[derive(Clone, Copy)]
struct TextArea {
    row: PixelRect,
    top: i32,
    font: FontMetrics,
}

impl TextArea {
    fn sized(mut row: PixelRect, part: &SizedTextRow, base: FontMetrics) -> Option<Self> {
        let size = part.size();
        let fraction = size.fraction();
        if fraction == 0.0 {
            return None;
        }
        let width = row.size.width.get();
        let cell_height = row.size.height.get();
        let height = cell_height * u32::from(size.scale);
        let scaled_width = (width as f32 * fraction).round().max(1.0) as u32;
        let scaled_height = (height as f32 * fraction).round().max(1.0) as u32;
        row.origin.x += aligned_offset(width - scaled_width, size.horizontal);
        let top = aligned_offset(height - scaled_height, size.vertical);
        let scale = f32::from(size.scale) * fraction;
        Some(Self {
            row,
            top: top as i32 - (part.row_offset as u32 * cell_height) as i32,
            font: FontMetrics {
                size: base.size * scale,
                cell: PixelSize {
                    width: NonZeroU32::new(scaled_width).unwrap(),
                    height: NonZeroU32::new(scaled_height).unwrap(),
                },
                baseline: (base.baseline as f32 * scale).round() as i32,
            },
        })
    }

    fn slice(self, width: NonZeroU32, y: u32, height: u32) -> Option<(PixelRect, u32)> {
        let top = self.top + y as i32;
        let first = top.max(0);
        let last = (top + height as i32).min(self.row.size.height.get() as i32);
        if first >= last {
            return None;
        }
        let mut rect = horizontal(self.row, first as u32, (last - first) as u32);
        rect.size.width = width;
        Some((rect, (first - top) as u32))
    }
}

pub(super) struct TextRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    atlas: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    layers: Vec<AtlasAllocator>,
    glyphs: [HashMap<Box<str>, Sprite>; LIGATURE_CACHE + 4],
    sized_glyphs: HashMap<SizedRaster, HashMap<Box<str>, Sprite>>,
    svg: SvgCache,
    svg_sprites: HashMap<SvgSpriteKey, Sprite>,
    font: Font,
    metrics: FontMetrics,
    chrome_metrics: FontMetrics,
    minimum_contrast: f32,
    generation: u64,
    evicted: bool,
    image: Vec<u8>,
    instances: Vec<Quad>,
    buffer: Option<wgpu::Buffer>,
    motion_buffer: Option<wgpu::Buffer>,
    area: PixelSize,
}

impl TextRenderer {
    pub(super) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        mut font: Font,
        metrics: FontMetrics,
        minimum_contrast: f32,
    ) -> Self {
        let chrome_metrics = measure_chrome_metrics(&mut font, metrics);
        let atlas = atlas_texture(device, 1);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Terminal foreground"),
            source: wgpu::ShaderSource::Wgsl(include_str!("text.wgsl").into()),
        });
        let pipeline = create_quad_pipeline(
            device,
            &module,
            format,
            "vertex",
            "fragment",
            Some(wgpu::BlendState::ALPHA_BLENDING),
            wgpu::VertexBufferLayout {
                array_stride: size_of::<Quad>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Uint32x2],
            },
        );
        let bind_group = atlas_binding(device, &pipeline, &atlas);
        Self {
            device: device.clone(),
            queue: queue.clone(),
            pipeline,
            atlas,
            bind_group,
            layers: vec![AtlasAllocator::new(size2(ATLAS_SIDE, ATLAS_SIDE))],
            glyphs: Default::default(),
            sized_glyphs: HashMap::new(),
            svg: SvgCache::default(),
            svg_sprites: HashMap::new(),
            font,
            metrics,
            chrome_metrics,
            minimum_contrast,
            generation: 0,
            evicted: false,
            image: Vec::new(),
            instances: Vec::new(),
            buffer: None,
            motion_buffer: None,
            area: metrics.cell,
        }
    }

    pub(super) fn metrics(&self) -> FontMetrics {
        self.metrics
    }

    pub(super) fn chrome_metrics(&self) -> FontMetrics {
        self.chrome_metrics
    }

    pub(super) fn measure(&mut self, size: f32, scale_factor: f64) -> FontMetrics {
        self.font.measure(size, scale_factor)
    }

    pub(super) fn measure_chrome(&mut self, base: FontMetrics) -> FontMetrics {
        measure_chrome_metrics(&mut self.font, base)
    }

    pub(super) fn set_font(&mut self, config: &FontConfig) -> bool {
        if !self.font.reconfigure(config) {
            return false;
        }
        self.chrome_metrics = measure_chrome_metrics(&mut self.font, self.metrics);
        self.clear_glyph_cache();
        true
    }

    pub(super) fn set_minimum_contrast(&mut self, value: f32) -> bool {
        if self.minimum_contrast == value {
            return false;
        }
        self.minimum_contrast = value;
        true
    }

    pub(super) fn set_metrics(&mut self, metrics: FontMetrics) {
        if self.metrics == metrics {
            return;
        }
        self.metrics = metrics;
        self.chrome_metrics = measure_chrome_metrics(&mut self.font, metrics);
        self.clear_glyph_cache();
    }

    fn clear_glyph_cache(&mut self) {
        for layer in &mut self.layers {
            layer.clear();
        }
        for cache in &mut self.glyphs {
            cache.clear();
        }
        self.sized_glyphs.clear();
        self.svg_sprites.clear();
    }

    pub(super) fn begin_frame(&mut self) {
        self.generation += 1;
        self.evicted = false;
        self.instances.clear();
    }

    /// Prepares the Modern-mode command block backdrop and dedicated left gutter rail
    /// in full pane coordinates, rendered behind terminal cell backgrounds and glyphs.
    pub(super) fn prepare_pane_blocks_bg(
        &mut self,
        pane_rect: PixelRect,
        text_origin: PixelPosition,
        frame: &PresentationFrame<'_>,
        overlay: &PaneOverlay<'_>,
    ) -> Range<u32> {
        let start = self.instances.len() as u32;
        if !overlay.command_blocks {
            return start..start;
        }
        let first_row = frame.viewport.range().start;
        let rows = usize::from(frame.geometry.pty_size().rows);
        let blocks =
            blocks::blocks_in_viewport(frame.viewport.source(), first_row..first_row + rows);
        if blocks.is_empty() {
            return start..start;
        }

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

        for block in &blocks {
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

            // Soft backdrop surface behind hovered or failed blocks (never washes out Powerline!).
            let show_surface =
                block.is_error() || (hovered && (block.is_finished() || block.is_running()));
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
                } else {
                    self.rounded_rect(surface_rect, r, 0, mix_color(pane_bg, pane_fg, 13), 0.54);
                    self.rounded_rect(surface_rect, r, 1, mix_color(pane_bg, accent, 110), 0.42);
                }
            }

            // Dedicated left gutter status rail (cleanly outside column 0 text!).
            if block.is_finished() || block.is_running() {
                let rail_inset_y = (3.0 * scale_f).round() as u32;
                let rail_h = block_h.saturating_sub(rail_inset_y * 2).max(4);
                if let (Some(rw), Some(rh)) = (NonZeroU32::new(rail_w), NonZeroU32::new(rail_h)) {
                    let (rail_color, rail_alpha) = if block.is_error() {
                        (red, if hovered { 1.0 } else { 0.90 })
                    } else if block.is_running() {
                        (accent, 0.92)
                    } else if hovered {
                        (green, 0.95)
                    } else {
                        (mix_color(pane_bg, green, 145), 0.62)
                    };
                    self.rounded_rect(
                        PixelRect {
                            origin: PixelPosition {
                                x: rail_x,
                                y: block_top.saturating_add(rail_inset_y),
                            },
                            size: PixelSize {
                                width: rw,
                                height: rh,
                            },
                        },
                        rail_w / 2,
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

    pub(super) fn prepare_pane(
        &mut self,
        frame: &PresentationFrame<'_>,
        cursor: Option<PresentedCursor>,
        overlay: &PaneOverlay<'_>,
    ) -> Result<Range<u32>, Box<dyn std::error::Error>> {
        let PaneOverlay {
            selection,
            hovered_link,
            progress,
            progress_phase,
            preedit,
            vi,
            search,
            hints,
            zoomed,
            scrollbar,
            command_blocks,
            hovered_block,
            scale,
            dim_alpha,
        } = *overlay;
        let hovered_hyperlink = match hovered_link {
            Some(HoveredLink::Explicit(id)) => Some(*id),
            _ => None,
        };
        let detected_link = match hovered_link {
            Some(HoveredLink::Detected(url)) => Some(url),
            _ => None,
        };
        let instance_start = self.instances.len() as u32;
        self.area = frame.geometry.text_area_pixels().unwrap();
        let columns = usize::from(frame.geometry.pty_size().columns);
        let ligatures = self.font.ligatures();
        let first_row = frame.viewport.range().start;
        let search_highlight = frame.palette.color(PaletteEntry::Indexed(3));
        let search_active_fg = [
            frame.palette.color(PaletteEntry::Background),
            frame.palette.color(PaletteEntry::Foreground),
            frame.palette.color(PaletteEntry::Indexed(0)),
            frame.palette.color(PaletteEntry::Indexed(15)),
        ]
        .into_iter()
        .max_by_key(|&color| luminance(color).abs_diff(luminance(search_highlight)))
        .unwrap();
        let text_selection = selection
            .filter(|sel| sel.kind == SelectionKind::Stream)
            .and_then(|sel| {
                frame
                    .viewport
                    .source()
                    .selection_text(sel.range.clone())
                    .ok()
            });
        let sized_cursor = cursor.and_then(|cursor| {
            let pos = cursor.position;
            frame
                .viewport
                .row(pos.row)?
                .sized_text_at(pos.column)
                .map(|part| (cursor, part))
        });
        let primary_blocks = if command_blocks {
            let rows = usize::from(frame.geometry.pty_size().rows);
            blocks::blocks_in_viewport(frame.viewport.source(), first_row..first_row + rows)
        } else {
            Vec::new()
        };
        for (row, source) in frame.viewport.iter_rows().enumerate() {
            let semantic = source.semantic();
            if !command_blocks
                && semantic.prompt_start
                && semantic.exit_code.is_some_and(|code| code != 0)
                && let Some(mut rect) = frame.geometry.row_rect(row, 0..1)
            {
                rect.size.width = NonZeroU32::new(2.min(rect.size.width.get())).unwrap();
                self.rect(rect, frame.palette.color(PaletteEntry::Indexed(1)), 0.9);
            }
            let mut sized = source.sized_text_rows().peekable();
            let visible_end = source
                .wrap_end()
                .unwrap_or(source.cells().len())
                .min(columns);
            let selected = selection
                .and_then(|sel| sel.columns_on_row(frame.viewport.source(), first_row + row));
            let detected_columns =
                detected_link.and_then(|url| url.columns_on_row(first_row + row, visible_end));
            let cursor_column = cursor
                .filter(|cursor| cursor.position.row == row && cursor.appearance.visible)
                .map(|cursor| cursor.position.column);
            let block_cursor = cursor
                .filter(|cursor| cursor.appearance.shape == CursorShape::Block)
                .and(cursor_column);
            if let Some(mut span) = selected.clone() {
                for part in source.sized_text_rows() {
                    if part.column >= span.end {
                        break;
                    }
                    if span.start < part.column {
                        self.selection_background(frame, row, span.start..part.column);
                    }
                    span.start = span.start.max(part.columns().end);
                }
                self.selection_background(frame, row, span);
            }
            let mut active_search_span: Option<Range<usize>> = None;
            if let Some(search) = search {
                let chronological = first_row + row;
                for (is_current, matched) in
                    search.visible_matches(chronological..chronological + 1)
                {
                    if let Some(span) = matched.columns_on_row(chronological, columns)
                        && let Some(rect) = frame.geometry.row_rect(row, span.clone())
                    {
                        let alpha = if is_current { 0.88 } else { 0.32 };
                        self.rect(rect, search_highlight, alpha);
                        if is_current {
                            active_search_span = Some(span);
                        }
                    }
                }
            }
            if let Some(runs) = frame.row_runs(row, 0..columns) {
                for run in runs {
                    let run = run?;
                    let style = run.style;
                    if style.style.hidden {
                        continue;
                    }
                    let alpha = if style.style.faint { 0.6 } else { 1.0 };
                    let run_fg = contrast::enforce(
                        style.foreground,
                        style.background,
                        self.minimum_contrast,
                    );
                    let run_underline = if style.style.underline_color == Color::Default {
                        run_fg
                    } else {
                        contrast::enforce(
                            style.underline_color,
                            style.background,
                            self.minimum_contrast,
                        )
                    };
                    let styled_run = PresentationStyle {
                        foreground: run_fg,
                        underline_color: run_underline,
                        ..style
                    };
                    let span = run.columns();
                    let mut units = run.units();
                    while let Some(unit) = units.next() {
                        if unit.cell.is_erased() || unit.text == CellText::Scalar(' ') {
                            continue;
                        }
                        let unit_selected = selected
                            .as_ref()
                            .is_some_and(|span| span.contains(&unit.column));
                        let in_active_match = active_search_span
                            .as_ref()
                            .is_some_and(|span| span.contains(&unit.column));
                        let exempt = matches!(
                            unit.text,
                            CellText::Scalar(ch) if contrast::is_exempt_character(ch)
                        );
                        let color = if unit_selected {
                            frame.palette.graphics_color(251)
                        } else if in_active_match {
                            search_active_fg
                        } else if exempt {
                            style.foreground
                        } else {
                            run_fg
                        };
                        if ligatures
                            && let CellText::Scalar(character) = unit.text
                            && ligature::can_start(character)
                        {
                            let start = unit.column;
                            let mut end = span.end.min(visible_end);
                            if let Some(selection) = &selected
                                && start < selection.end
                            {
                                end = end.min(if unit_selected {
                                    selection.end
                                } else {
                                    selection.start
                                });
                            }
                            if let Some(column) = block_cursor.filter(|&column| column >= start) {
                                end = end.min(column);
                            }
                            let previous = (start > span.start).then(|| source.cells()[start - 1]);
                            if let Some(cells) = source.cells().get(start..end)
                                && let Some(candidate) = ligature::prefix(cells, previous)
                            {
                                let text = candidate.text();
                                if let Some(rect) =
                                    frame.geometry.row_rect(row, start..start + text.len())
                                    && rect.size.width.get() <= ATLAS_SIDE as u32
                                {
                                    let rendition = usize::from(style.style.bold)
                                        | (usize::from(style.style.italic) << 1);
                                    if self.glyph(
                                        rect,
                                        text,
                                        LIGATURE_CACHE + rendition,
                                        color,
                                        alpha,
                                    ) {
                                        units.nth(text.len() - 2);
                                        continue;
                                    }
                                }
                            }
                        }
                        if let Some(rect) = frame
                            .geometry
                            .row_rect(row, unit.column..unit.column + unit.cell.width())
                        {
                            if let Some(icon) = icon_character(unit) {
                                let can_extend = icon_can_extend(
                                    frame,
                                    source,
                                    visible_end,
                                    unit.column,
                                    style.background,
                                );
                                self.icon(rect, icon, style.style, color, alpha, can_extend, None);
                            } else {
                                self.unit(rect, unit, style.style, color, alpha);
                            }
                        }
                    }
                    let link = styled_run.style.hyperlink;
                    let mut link_style = *styled_run.style;
                    let decorated = if link != HyperlinkId(0) {
                        if hovered_hyperlink == Some(link) {
                            link_style.underline = Underline::Curly;
                        } else if link_style.underline == Underline::None {
                            link_style.underline = Underline::Single;
                        }
                        PresentationStyle {
                            style: &link_style,
                            ..styled_run
                        }
                    } else {
                        styled_run
                    };
                    if decorated.style.strikethrough || decorated.style.underline != Underline::None
                    {
                        let mut columns = run.columns();
                        while let Some(part) = sized.peek().map(|part| part.columns()) {
                            if part.start >= columns.end {
                                break;
                            }
                            if columns.start < part.start {
                                self.row_decorations(
                                    frame,
                                    row,
                                    columns.start..part.start,
                                    decorated,
                                    alpha,
                                );
                            }
                            columns.start = columns.start.max(part.end);
                            if part.end > columns.end {
                                break;
                            }
                            sized.next();
                        }
                        self.row_decorations(frame, row, columns, decorated, alpha);
                    }
                    if link == HyperlinkId(0)
                        && decorated.style.underline == Underline::None
                        && let Some(detected) = &detected_columns
                    {
                        let run_cols = run.columns();
                        let start = run_cols.start.max(detected.start);
                        let end = run_cols.end.min(detected.end);
                        if start < end {
                            let mut url_style = *styled_run.style;
                            url_style.underline = Underline::Single;
                            let url_decorated = PresentationStyle {
                                style: &url_style,
                                ..styled_run
                            };
                            self.row_decorations(frame, row, start..end, url_decorated, alpha);
                        }
                    }
                }
            }
            for part in source.sized_text_rows() {
                let Some(rect) = frame.geometry.row_rect(row, part.columns()) else {
                    continue;
                };
                let mut style = frame
                    .resolve_style(source.cells()[part.column].style())
                    .ok_or(lune_terminal::StyleError::InvalidId(
                        source.cells()[part.column].style(),
                    ))?;
                let covering = sized_cursor
                    .filter(|(_, target)| part.belongs_to(target))
                    .map(|(cursor, _)| cursor);
                let block_cursor =
                    covering.filter(|cursor| cursor.appearance.shape == CursorShape::Block);
                let selected = text_selection
                    .is_some_and(|range| range.includes_sized_text(first_row + row, part));
                if let Some(cursor) = block_cursor {
                    self.rect(rect, cursor.color, 1.0);
                } else if selected {
                    self.rect(rect, frame.palette.graphics_color(250), 1.0);
                }
                if !style.style.hidden
                    && let Some(area) = TextArea::sized(rect, part, self.metrics)
                {
                    let foreground = if block_cursor.is_some() {
                        Some(style.background)
                    } else if selected {
                        Some(frame.palette.graphics_color(251))
                    } else {
                        None
                    };
                    if let Some(foreground) = foreground {
                        if style.style.underline_color == Color::Default {
                            style.underline_color = foreground;
                        }
                        style.foreground = foreground;
                    } else {
                        let mut chars = part.text().chars();
                        let exempt = chars
                            .next()
                            .filter(|_| chars.next().is_none())
                            .is_some_and(contrast::is_exempt_character);
                        if !exempt {
                            let fg = contrast::enforce(
                                style.foreground,
                                style.background,
                                self.minimum_contrast,
                            );
                            if style.style.underline_color == Color::Default {
                                style.underline_color = fg;
                            } else {
                                style.underline_color = contrast::enforce(
                                    style.underline_color,
                                    style.background,
                                    self.minimum_contrast,
                                );
                            }
                            style.foreground = fg;
                        }
                    }
                    let alpha = if style.style.faint { 0.6 } else { 1.0 };
                    let rendition =
                        usize::from(style.style.bold) | (usize::from(style.style.italic) << 1);
                    self.sized_glyph(area, part.text(), rendition, style.foreground, alpha);
                    let link = style.style.hyperlink;
                    let mut link_style = *style.style;
                    let decorated = if link != HyperlinkId(0) {
                        if hovered_hyperlink == Some(link) {
                            link_style.underline = Underline::Curly;
                        } else if link_style.underline == Underline::None {
                            link_style.underline = Underline::Single;
                        }
                        PresentationStyle {
                            style: &link_style,
                            ..style
                        }
                    } else {
                        style
                    };
                    self.decorations(area, decorated, alpha);
                }
                if let Some(cursor) = covering {
                    let mut edge = rect;
                    match cursor.appearance.shape {
                        CursorShape::Bar => {
                            edge.size.width = NonZeroU32::new(2).unwrap().min(edge.size.width);
                            self.rect(edge, cursor.color, 1.0);
                        }
                        CursorShape::Underline
                            if part.row_offset + 1 == usize::from(part.size().scale) =>
                        {
                            self.rect(
                                horizontal(
                                    edge,
                                    edge.size.height.get().saturating_sub(2),
                                    edge.size.height.get().min(2),
                                ),
                                cursor.color,
                                1.0,
                            );
                        }
                        _ => {}
                    }
                }
            }
        }
        let preedit_text = preedit.map_or("", Preedit::text);
        if let Some(cursor) = cursor
            && preedit.is_none()
            && sized_cursor.is_none()
            && let Some(paint) = frame.cursor_paint(cursor, NonZeroU32::new(2).unwrap())?
        {
            self.rect(paint.rect, paint.color, 1.0);
            if let Some(glyph) = paint.glyph {
                if let Some(icon) = icon_character(glyph.unit) {
                    let can_extend = frame
                        .viewport
                        .row(cursor.position.row)
                        .zip(frame.resolve_style(glyph.unit.cell.style()))
                        .is_some_and(|(source, style)| {
                            let visible_end = source
                                .wrap_end()
                                .unwrap_or(source.cells().len())
                                .min(columns);
                            icon_can_extend(
                                frame,
                                source,
                                visible_end,
                                glyph.unit.column,
                                style.background,
                            )
                        });
                    self.icon(
                        paint.rect,
                        icon,
                        glyph.style,
                        glyph.color,
                        1.0,
                        can_extend,
                        Some(0..paint.rect.size.width.get()),
                    );
                } else {
                    self.unit(paint.rect, glyph.unit, glyph.style, glyph.color, 1.0);
                }
            } else if cursor.appearance.shape == CursorShape::Block
                && cursor.position.column > 0
                && let Some(source) = frame.viewport.row(cursor.position.row)
            {
                let prev_col = cursor.position.column - 1;
                let visible_end = source
                    .wrap_end()
                    .unwrap_or(source.cells().len())
                    .min(columns);
                if let Some(prev_cell) = source.cells().get(prev_col).copied()
                    && !prev_cell.is_erased()
                    && !prev_cell.is_continuation()
                    && !prev_cell.has_extended_text()
                    && let Some(ch) = prev_cell.character()
                    && let Some(icon) = icon_character(DisplayUnit {
                        column: prev_col,
                        cell: prev_cell,
                        text: CellText::Scalar(ch),
                    })
                    && let Some(prev_style) = frame.resolve_style(prev_cell.style())
                    && !prev_style.style.hidden
                    && icon_can_extend(frame, source, visible_end, prev_col, prev_style.background)
                    && let Some(prev_rect) = frame
                        .geometry
                        .row_rect(cursor.position.row, prev_col..prev_col + 1)
                {
                    let cell_w = prev_rect.size.width.get();
                    let slice_end = cell_w.saturating_add(paint.rect.size.width.get());
                    let overflow_fg = if prev_style.background != paint.color {
                        prev_style.background
                    } else if prev_style.foreground != paint.color {
                        prev_style.foreground
                    } else if luminance(paint.color) >= 128 {
                        Rgb {
                            red: 0,
                            green: 0,
                            blue: 0,
                        }
                    } else {
                        Rgb {
                            red: 255,
                            green: 255,
                            blue: 255,
                        }
                    };
                    self.icon(
                        prev_rect,
                        icon,
                        prev_style.style,
                        overflow_fg,
                        1.0,
                        true,
                        Some(cell_w..slice_end),
                    );
                }
            }
        }
        if !primary_blocks.is_empty() && search.is_none() && vi.is_none() && hints.is_none() {
            let scrollbar_reserve = scrollbar.map_or(0, |sb| sb.track.size.width.get() + 6);
            self.command_block_overlays(
                frame,
                &primary_blocks,
                hovered_block,
                scrollbar_reserve,
                scale,
            );
        }
        if let Some(hints) = hints {
            self.hints_overlay(frame, hints);
        } else if vi.is_some() {
            if search.is_none() {
                self.mode_badge(frame.palette, "VI");
            }
        } else {
            if zoomed && search.is_none() {
                self.mode_badge(frame.palette, "ZOOM");
            }
            if let Some(preedit) = preedit
                && search.is_none()
                && let Some(cursor) = frame.viewport.cursor()
            {
                self.ime_preedit(
                    frame,
                    CellPosition {
                        row: cursor.row,
                        column: cursor.column,
                    },
                    preedit,
                );
            }
        }
        if let Some(search) = search {
            self.search_bar(frame.palette, search, preedit_text);
        }
        self.progress_bar(frame.palette, progress, progress_phase);
        if let Some(scrollbar) = scrollbar {
            self.scrollbar(frame.palette, scrollbar);
        }
        if dim_alpha > 0.0 {
            let veil = frame.palette.color(if frame.reverse_video {
                PaletteEntry::Foreground
            } else {
                PaletteEntry::Background
            });
            self.rect(
                PixelRect {
                    origin: PixelPosition { x: 0, y: 0 },
                    size: self.area,
                },
                veil,
                dim_alpha,
            );
        }
        Ok(instance_start..self.instances.len() as u32)
    }

    fn command_block_overlays(
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
        let accent = frame.palette.color(PaletteEntry::Indexed(4));

        for block in blocks {
            let is_hovered = hovered_block.is_some_and(|h| h.prompt_row == block.prompt_row);
            if is_hovered {
                let has_command = blocks::extract_command(source, block).is_some();
                let has_output = blocks::extract_output(source, block).is_some();
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
                            blocks::BlockAction::CopyCommand | blocks::BlockAction::CopyOutput => {
                                svg::icons::COPY
                            }
                            blocks::BlockAction::Rerun => svg::icons::PLAY,
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
                let (fill, border, text_color, alpha) = if block.is_error() {
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

    fn ime_preedit(
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

    fn hints_overlay(&mut self, frame: &PresentationFrame<'_>, hints: &HintMode) {
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

    fn mode_badge(&mut self, palette: &lune_terminal::Palette, label: &str) {
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

    fn search_bar(&mut self, palette: &lune_terminal::Palette, search: &SearchBar, preedit: &str) {
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

    fn scrollbar(&mut self, palette: &lune_terminal::Palette, view: ScrollbarView) {
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

    pub(super) fn prepare_chrome(
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

    pub(super) fn prepare_modal(
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

    pub(super) fn prepare_command_palette(
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

    fn floating_card(
        &mut self,
        rect: PixelRect,
        radius: u32,
        shadow: Rgb,
        fill: Rgb,
        border: Rgb,
        alpha: f32,
    ) {
        if let (Some(w), Some(h)) = (
            NonZeroU32::new(rect.size.width.get().saturating_add(2)),
            NonZeroU32::new(rect.size.height.get().saturating_add(2)),
        ) {
            let halo = PixelRect {
                origin: PixelPosition {
                    x: rect.origin.x.saturating_sub(1),
                    y: rect.origin.y.saturating_sub(1),
                },
                size: PixelSize {
                    width: w,
                    height: h,
                },
            };
            self.rounded_rect(halo, radius.saturating_add(1), 0, shadow, 0.45);
        }
        self.rounded_rect(rect, radius, 0, fill, alpha);
        self.rounded_rect(rect, radius, 1, border, 0.95);
    }

    fn truncate_to_width_with<'a>(
        &mut self,
        metrics: FontMetrics,
        text: &'a str,
        max_w: u32,
    ) -> (std::borrow::Cow<'a, str>, u32) {
        let measured = self.font.measure_text(metrics, text);
        if measured <= max_w {
            return (std::borrow::Cow::Borrowed(text), measured);
        }
        let ellipsis = "...";
        let ellipsis_w = self.font.measure_text(metrics, ellipsis);
        let budget = max_w.saturating_sub(ellipsis_w);
        let mut end = 0;
        for (idx, ch) in text.char_indices() {
            let next = idx + ch.len_utf8();
            if self.font.measure_text(metrics, &text[..next]) > budget {
                break;
            }
            end = next;
        }
        let truncated = format!("{}{ellipsis}", text[..end].trim_end());
        let width = self.font.measure_text(metrics, &truncated).min(max_w);
        (std::borrow::Cow::Owned(truncated), width)
    }

    fn truncate_to_width<'a>(
        &mut self,
        text: &'a str,
        max_w: u32,
    ) -> (std::borrow::Cow<'a, str>, u32) {
        self.truncate_to_width_with(self.metrics, text, max_w)
    }

    fn clamped_text(&mut self, origin: PixelPosition, max_w: u32, text: &str, color: Rgb) {
        let limit = max_w.min(ATLAS_SIDE as u32);
        if limit == 0 {
            return;
        }
        let (display, text_w) = self.truncate_to_width(text, limit);
        if let Some(width) = NonZeroU32::new(text_w.min(limit)) {
            self.glyph(
                PixelRect {
                    origin,
                    size: PixelSize {
                        width,
                        height: self.metrics.cell.height,
                    },
                },
                &display,
                LABEL_CACHE,
                color,
                1.0,
            );
        }
    }

    fn clamped_chrome_text(&mut self, origin: PixelPosition, max_w: u32, text: &str, color: Rgb) {
        let limit = max_w.min(ATLAS_SIDE as u32);
        if limit == 0 {
            return;
        }
        let metrics = self.chrome_metrics;
        let (display, text_w) = self.truncate_to_width_with(metrics, text, limit);
        if let Some(width) = NonZeroU32::new(text_w.min(limit)) {
            self.glyph(
                PixelRect {
                    origin,
                    size: PixelSize {
                        width,
                        height: metrics.cell.height,
                    },
                },
                &display,
                CHROME_LABEL_CACHE,
                color,
                1.0,
            );
        }
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

    fn rounded_rect(
        &mut self,
        rect: PixelRect,
        radius: u32,
        thickness: u32,
        color: Rgb,
        alpha: f32,
    ) {
        let w = rect.size.width.get().min(2047);
        let h = rect.size.height.get().min(2047);
        let r = radius.min(63).min(w / 2).min(h / 2);
        let t = thickness.min(15);
        let packed = r | (t << 6) | (w << 10) | (h << 21);
        let half_w = w as f32 * 0.5;
        let half_h = h as f32 * 0.5;
        self.quad(
            rect,
            color,
            alpha,
            [-half_w, -half_h, w as f32, h as f32],
            [6, packed],
        );
    }

    pub(super) fn prepare_titlebar(
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

    pub(super) fn prepare_sidebar(
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

    pub(super) fn prepare_tab_bar(
        &mut self,
        view: &tab_bar::View<'_>,
        progress_phase: f32,
        background_opacity: f32,
    ) -> Range<u32> {
        let start = self.instances.len() as u32;
        self.tab_bar(view, progress_phase, background_opacity);
        start..self.instances.len() as u32
    }

    pub(super) fn finish_frame(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if std::mem::size_of_val(self.instances.as_slice()) as u64
            > self.device.limits().max_buffer_size
        {
            return Err(
                std::io::Error::other("Terminal foreground exceeds the GPU buffer limit").into(),
            );
        }
        write_instances(&self.device, &self.queue, &mut self.buffer, &self.instances);
        Ok(())
    }

    fn progress_bar(&mut self, palette: &lune_terminal::Palette, progress: Progress, phase: f32) {
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

    pub(super) fn prepare_moving_cursor(&mut self, area: PixelSize, rect: PixelRect, color: Rgb) {
        let quad = Quad::new(area, rect, color, 1.0, [0.0; 4], [0, 0]);
        write_instances(&self.device, &self.queue, &mut self.motion_buffer, &[quad]);
    }

    pub(super) fn draw_range(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        range: Range<u32>,
        moving: bool,
    ) {
        if range.is_empty() && !moving {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        if let Some(buffer) = &self.buffer
            && !range.is_empty()
        {
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..4, range);
        }
        if moving && let Some(buffer) = &self.motion_buffer {
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..4, 0..1);
        }
    }

    fn unit(
        &mut self,
        rect: PixelRect,
        unit: DisplayUnit<'_>,
        style: &Style,
        color: Rgb,
        alpha: f32,
    ) {
        let mut bytes = [0; 4];
        let text = match unit.text {
            CellText::Scalar(character) => character.encode_utf8(&mut bytes),
            CellText::Extended(text) => text,
            CellText::Continuation => return,
        };
        let cache = usize::from(style.bold)
            | (usize::from(style.italic) << 1)
            | (usize::from(rect.size.width > self.metrics.cell.width) << 2);
        self.glyph(rect, text, cache, color, alpha);
    }

    fn glyph(
        &mut self,
        rect: PixelRect,
        text: &str,
        cache_index: usize,
        color: Rgb,
        alpha: f32,
    ) -> bool {
        let width = rect.size.width.get();
        let height = rect.size.height.get();
        let cache = &mut self.glyphs[cache_index];
        if matches!(
            cache_index,
            LABEL_CACHE | CHROME_LABEL_CACHE | TAB_LABEL_CACHE
        ) && cache.get(text).is_some_and(|sprite| {
            sprite.allocation.rectangle.width() != width as i32
                || sprite.allocation.rectangle.height() != height as i32
        }) {
            let sprite = cache.remove(text).unwrap();
            self.layers[sprite.layer].deallocate(sprite.allocation.id);
        }
        let sprite = if let Some(sprite) = cache.get_mut(text) {
            sprite.generation = self.generation;
            *sprite
        } else {
            let ligature_run = cache_index >= LIGATURE_CACHE;
            let rendition = if ligature_run {
                cache_index - LIGATURE_CACHE
            } else {
                match cache_index {
                    CHROME_LABEL_CACHE | TAB_LABEL_CACHE => 0,
                    _ => cache_index & 3,
                }
            };
            let raster_metrics = if cache_index == CHROME_LABEL_CACHE {
                self.chrome_metrics
            } else {
                self.metrics
            };
            let procedural = if cache_index < LABEL_CACHE {
                let mut chars = text.chars();
                chars
                    .next()
                    .filter(|_| chars.next().is_none())
                    .and_then(|ch| {
                        box_drawing::rasterize(self.metrics, ch, width, height, &mut self.image)
                    })
            } else {
                None
            };
            let colored = match procedural {
                Some(true) => false,
                Some(false) => return true,
                None => {
                    let Some(colored) = self.font.rasterize(
                        raster_metrics,
                        text,
                        rendition,
                        width,
                        ligature_run,
                        &mut self.image,
                    ) else {
                        return false;
                    };
                    colored
                }
            };
            let Some(sprite) = self.upload_sprite(width, height, colored) else {
                return false;
            };
            self.glyphs[cache_index].insert(text.into(), sprite);
            sprite
        };
        let corner = sprite.allocation.rectangle.min;
        self.quad(
            rect,
            color,
            alpha,
            [
                corner.x as f32,
                corner.y as f32,
                width as f32,
                rect.size.height.get() as f32,
            ],
            [if sprite.colored { 2 } else { 1 }, sprite.layer as u32],
        );
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn icon(
        &mut self,
        mut rect: PixelRect,
        icon: char,
        style: &Style,
        color: Rgb,
        alpha: f32,
        can_extend: bool,
        slice: Option<Range<u32>>,
    ) {
        let mut bytes = [0; 4];
        let text = icon.encode_utf8(&mut bytes);
        let rendition = usize::from(style.bold) | (usize::from(style.italic) << 1);
        let cache = ICON_CACHE + rendition;
        let height = rect.size.height.get();
        let sprite = if let Some(sprite) = self.glyphs[cache].get_mut(text as &str) {
            sprite.generation = self.generation;
            *sprite
        } else {
            let Some((width, colored)) = self.font.rasterize_icon(
                self.metrics,
                text,
                rendition,
                ATLAS_SIDE as u32,
                &mut self.image,
            ) else {
                self.glyph(rect, text, rendition, color, alpha);
                return;
            };
            let Some(sprite) = self.upload_sprite(width, height, colored) else {
                return;
            };
            self.glyphs[cache].insert(Box::<str>::from(text as &str), sprite);
            sprite
        };
        let native = sprite.allocation.rectangle;
        let width = native.width() as u32;
        let available = rect.size.width.get() * if can_extend { 2 } else { 1 };
        let draw_width = width.min(available);
        let scale = draw_width as f32 / width as f32;
        let draw_height = (height as f32 * scale).round().max(1.0) as u32;
        let top = (self.metrics.baseline as f32 * (1.0 - scale)).round() as u32;
        let (slice_start, slice_end) = match slice {
            Some(range) => (range.start.min(draw_width), range.end.min(draw_width)),
            None => (0, draw_width),
        };
        let Some(visible_width) = NonZeroU32::new(slice_end.saturating_sub(slice_start)) else {
            return;
        };
        let inv_draw = width as f32 / draw_width as f32;
        let uv_x = native.min.x as f32 + slice_start as f32 * inv_draw;
        let uv_w = visible_width.get() as f32 * inv_draw;
        rect.origin.x += slice_start;
        rect.origin.y += top.min(height - draw_height);
        rect.size.width = visible_width;
        rect.size.height = NonZeroU32::new(draw_height).unwrap();
        self.quad(
            rect,
            color,
            alpha,
            [uv_x, native.min.y as f32, uv_w, height as f32],
            [if sprite.colored { 2 } else { 1 }, sprite.layer as u32],
        );
    }

    fn sized_glyph(
        &mut self,
        area: TextArea,
        text: &str,
        rendition: usize,
        color: Rgb,
        alpha: f32,
    ) {
        if text.trim().is_empty() {
            return;
        }
        let maximum = NonZeroU32::new(ATLAS_SIDE as u32).unwrap();
        let paint = PixelSize {
            width: area.font.cell.width.min(maximum),
            height: area.font.cell.height.min(maximum),
        };
        let Some((rect, offset)) = area.slice(paint.width, 0, paint.height.get()) else {
            return;
        };
        let key = SizedRaster {
            width: paint.width.get(),
            height: paint.height.get(),
            font_size: area.font.size.to_bits(),
            baseline: area.font.baseline,
            rendition,
        };
        let sprite = if let Some(sprite) = self
            .sized_glyphs
            .get_mut(&key)
            .and_then(|cache| cache.get_mut(text))
        {
            sprite.generation = self.generation;
            *sprite
        } else {
            let metrics = FontMetrics {
                cell: paint,
                ..area.font
            };
            let Some(colored) =
                self.font
                    .rasterize(metrics, text, rendition, key.width, false, &mut self.image)
            else {
                return;
            };
            let Some(sprite) = self.upload_sprite(key.width, key.height, colored) else {
                return;
            };
            self.sized_glyphs
                .entry(key)
                .or_default()
                .insert(text.into(), sprite);
            sprite
        };
        let corner = sprite.allocation.rectangle.min;
        self.quad(
            rect,
            color,
            alpha,
            [
                corner.x as f32,
                corner.y as f32 + offset as f32,
                key.width as f32,
                rect.size.height.get() as f32,
            ],
            [if sprite.colored { 2 } else { 1 }, sprite.layer as u32],
        );
    }

    pub(super) fn svg_color(
        &mut self,
        origin: PixelPosition,
        size: u32,
        source: &'static str,
        alpha: f32,
    ) -> bool {
        self.svg_draw(
            origin,
            size,
            size,
            source,
            SvgMode::Color,
            Rgb {
                red: 255,
                green: 255,
                blue: 255,
            },
            alpha,
        )
    }

    pub(super) fn svg_mask(
        &mut self,
        origin: PixelPosition,
        size: u32,
        source: &'static str,
        color: Rgb,
        alpha: f32,
    ) -> bool {
        self.svg_draw(origin, size, size, source, SvgMode::Mask, color, alpha)
    }

    #[allow(clippy::too_many_arguments)]
    fn svg_draw(
        &mut self,
        origin: PixelPosition,
        width: u32,
        height: u32,
        source: &'static str,
        mode: SvgMode,
        color: Rgb,
        alpha: f32,
    ) -> bool {
        let (Some(w_nz), Some(h_nz)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
            return false;
        };
        if width > ATLAS_SIDE as u32 || height > ATLAS_SIDE as u32 {
            return false;
        }
        let colored = mode == SvgMode::Color;
        let key = SvgSpriteKey {
            source: source.as_ptr() as usize,
            width,
            height,
            colored,
        };
        let sprite = if let Some(sprite) = self.svg_sprites.get_mut(&key) {
            sprite.generation = self.generation;
            *sprite
        } else {
            let Ok(raster) = self.svg.rasterize(source, width, height, mode) else {
                return false;
            };
            self.image.clear();
            self.image.extend_from_slice(&raster.rgba);
            let Some(sprite) = self.upload_sprite(raster.width, raster.height, raster.colored)
            else {
                return false;
            };
            self.svg_sprites.insert(key, sprite);
            sprite
        };
        let corner = sprite.allocation.rectangle.min;
        let rect = PixelRect {
            origin,
            size: PixelSize {
                width: w_nz,
                height: h_nz,
            },
        };
        self.quad(
            rect,
            color,
            alpha,
            [
                corner.x as f32,
                corner.y as f32,
                width as f32,
                height as f32,
            ],
            [if sprite.colored { 2 } else { 1 }, sprite.layer as u32],
        );
        true
    }

    fn upload_sprite(&mut self, width: u32, height: u32, colored: bool) -> Option<Sprite> {
        let (layer, allocation) = self.allocate_sprite(size2(width as i32, height as i32))?;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.atlas,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: allocation.rectangle.min.x as u32,
                    y: allocation.rectangle.min.y as u32,
                    z: layer as u32,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &self.image,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        Some(Sprite {
            allocation,
            layer,
            colored,
            generation: self.generation,
        })
    }

    fn allocate_sprite(&mut self, size: guillotiere::Size) -> Option<(usize, Allocation)> {
        if let Some(allocation) = allocate(&mut self.layers, size) {
            return Some(allocation);
        }
        if !self.evicted {
            self.evicted = true;
            for cache in self.glyphs.iter_mut().chain(self.sized_glyphs.values_mut()) {
                cache.retain(|_, sprite| {
                    if sprite.generation == self.generation {
                        return true;
                    }
                    self.layers[sprite.layer].deallocate(sprite.allocation.id);
                    false
                });
            }
            self.sized_glyphs.retain(|_, cache| !cache.is_empty());
            self.svg_sprites.retain(|_, sprite| {
                if sprite.generation == self.generation {
                    return true;
                }
                self.layers[sprite.layer].deallocate(sprite.allocation.id);
                false
            });
            if let Some(allocation) = allocate(&mut self.layers, size) {
                return Some(allocation);
            }
        }

        let layers = self.layers.len() as u32;
        let limit = MAX_ATLAS_LAYERS.min(self.device.limits().max_texture_array_layers);
        let expanded = (layers * 2).min(limit);
        if layers == expanded {
            return None;
        }
        let atlas = atlas_texture(&self.device, expanded);
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_texture(
            self.atlas.as_image_copy(),
            atlas.as_image_copy(),
            self.atlas.size(),
        );
        self.queue.submit([encoder.finish()]);
        self.bind_group = atlas_binding(&self.device, &self.pipeline, &atlas);
        self.atlas = atlas;
        self.layers.resize_with(expanded as usize, || {
            AtlasAllocator::new(size2(ATLAS_SIDE, ATLAS_SIDE))
        });
        let layer = layers as usize;
        self.layers[layer]
            .allocate(size)
            .map(|allocation| (layer, allocation))
    }

    fn selection_background(
        &mut self,
        frame: &PresentationFrame<'_>,
        row: usize,
        span: Range<usize>,
    ) {
        if let Some(rect) = frame.geometry.row_rect(row, span) {
            self.rect(rect, frame.palette.graphics_color(250), 1.0);
        }
    }

    fn row_decorations(
        &mut self,
        frame: &PresentationFrame<'_>,
        row: usize,
        columns: Range<usize>,
        style: PresentationStyle<'_>,
        alpha: f32,
    ) {
        if let Some(rect) = frame.geometry.row_rect(row, columns) {
            let area = TextArea {
                row: rect,
                top: 0,
                font: FontMetrics {
                    cell: rect.size,
                    ..self.metrics
                },
            };
            self.decorations(area, style, alpha);
        }
    }

    fn decorations(&mut self, area: TextArea, style: PresentationStyle<'_>, alpha: f32) {
        let height = area.font.cell.height.get();
        let thickness = ((area.font.size / 14.0).round().max(1.0) as u32).min(height);
        let baseline = area.font.baseline.clamp(0, height as i32 - 1) as u32;
        let mut line = |y, color| {
            if let Some((rect, _)) = area.slice(area.font.cell.width, y, thickness) {
                self.rect(rect, color, alpha);
            }
        };
        if style.style.strikethrough {
            let y = baseline.saturating_sub(height / 4);
            line(y, style.foreground);
        }
        if style.style.underline != Underline::None {
            let y = (baseline + thickness).min(height - thickness);
            match style.style.underline {
                Underline::None => {}
                Underline::Single => line(y, style.underline_color),
                Underline::Double => {
                    line(y, style.underline_color);
                    let second = y.saturating_sub(2 * thickness);
                    if second != y {
                        line(second, style.underline_color);
                    }
                }
                Underline::Curly => {
                    let band_height = (thickness * 3 + 1).min(height).max(3.min(height));
                    let wave_y = (baseline + 1).min(height.saturating_sub(band_height));
                    if let Some((rect, _)) = area.slice(area.font.cell.width, wave_y, band_height) {
                        let period = (thickness * 6).max(6);
                        let packed =
                            (thickness.min(255)) | ((band_height.min(255)) << 8) | (period << 16);
                        let half_height = rect.size.height.get() as f32 * 0.5;
                        self.quad(
                            rect,
                            style.underline_color,
                            alpha,
                            [
                                rect.origin.x as f32,
                                -half_height,
                                rect.size.width.get() as f32,
                                rect.size.height.get() as f32,
                            ],
                            [3, packed],
                        );
                    }
                }
                Underline::Dotted | Underline::Dashed => {
                    if let Some((rect, _)) = area.slice(area.font.cell.width, y, thickness) {
                        let mode = if style.style.underline == Underline::Dotted {
                            4
                        } else {
                            5
                        };
                        self.quad(
                            rect,
                            style.underline_color,
                            alpha,
                            [
                                rect.origin.x as f32,
                                0.0,
                                rect.size.width.get() as f32,
                                rect.size.height.get() as f32,
                            ],
                            [mode, thickness.min(255)],
                        );
                    }
                }
            }
        }
    }

    fn rect(&mut self, rect: PixelRect, color: Rgb, alpha: f32) {
        self.quad(rect, color, alpha, [0.0; 4], [0, 0]);
    }

    fn quad(&mut self, rect: PixelRect, color: Rgb, alpha: f32, uv: [f32; 4], sprite: [u32; 2]) {
        self.instances
            .push(Quad::new(self.area, rect, color, alpha, uv, sprite));
    }
}

fn icon_can_extend(
    frame: &PresentationFrame<'_>,
    source: &lune_terminal::Row,
    visible_end: usize,
    column: usize,
    background: Rgb,
) -> bool {
    let next = column + 1;
    next < visible_end
        && source.sized_text_at(next).is_none()
        && source.cells().get(next).is_some_and(|cell| {
            (cell.is_erased() || (cell.character() == Some(' ') && !cell.has_extended_text()))
                && frame
                    .resolve_style(cell.style())
                    .is_some_and(|next| next.background == background)
        })
}

fn icon_character(unit: DisplayUnit<'_>) -> Option<char> {
    let CellText::Scalar(character) = unit.text else {
        return None;
    };
    // Powerline separators follow the edges of their own cell.
    (unit.cell.width() == 1
        && matches!(character, '\u{e000}'..='\u{f8ff}' | '\u{f0000}'..='\u{ffffd}' | '\u{100000}'..='\u{10fffd}')
        && !matches!(character, '\u{e0a0}'..='\u{e0d7}'))
        .then_some(character)
}

fn aligned_offset(space: u32, alignment: u8) -> u32 {
    match alignment {
        1 => space,
        2 => space / 2,
        _ => 0,
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

fn mix_color(background: Rgb, foreground: Rgb, amount: u16) -> Rgb {
    let channel =
        |a: u8, b: u8| ((u16::from(a) * (255 - amount) + u16::from(b) * amount) / 255) as u8;
    Rgb {
        red: channel(background.red, foreground.red),
        green: channel(background.green, foreground.green),
        blue: channel(background.blue, foreground.blue),
    }
}

fn luminance(color: Rgb) -> u32 {
    u32::from(color.red) * 299 + u32::from(color.green) * 587 + u32::from(color.blue) * 114
}

fn allocate(layers: &mut [AtlasAllocator], size: guillotiere::Size) -> Option<(usize, Allocation)> {
    layers
        .iter_mut()
        .enumerate()
        .find_map(|(layer, atlas)| atlas.allocate(size).map(|allocation| (layer, allocation)))
}

fn atlas_texture(device: &wgpu::Device, layers: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Terminal glyph atlas"),
        size: wgpu::Extent3d {
            width: ATLAS_SIDE as u32,
            height: ATLAS_SIDE as u32,
            depth_or_array_layers: layers,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn atlas_binding(
    device: &wgpu::Device,
    pipeline: &wgpu::RenderPipeline,
    atlas: &wgpu::Texture,
) -> wgpu::BindGroup {
    let view = atlas.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(&view),
        }],
    })
}

fn measure_chrome_metrics(font: &mut Font, base: FontMetrics) -> FontMetrics {
    let size = (base.size * CHROME_FONT_SCALE).max(10.0).min(base.size);
    font.measure(size, 1.0)
}

fn horizontal(rect: PixelRect, y: u32, height: u32) -> PixelRect {
    PixelRect {
        origin: PixelPosition {
            x: rect.origin.x,
            y: rect.origin.y + y,
        },
        size: PixelSize {
            width: rect.size.width,
            height: NonZeroU32::new(height).unwrap(),
        },
    }
}
