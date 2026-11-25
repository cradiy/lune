mod blocks;
mod chrome;
mod modals;
mod overlays;

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::ops::Range;

use guillotiere::{Allocation, AtlasAllocator, size2};
use lune_terminal::{
    CellPosition, CellText, Color, CursorShape, DisplayUnit, HyperlinkId, PaletteEntry,
    PixelPosition, PixelSize, Rgb, SizedTextRow, Style, Underline,
};

use crate::gpu::{create_quad_pipeline, linear_colors, write_instances};
use crate::{PixelRect, PresentationFrame, PresentationStyle, PresentedCursor};

use super::blocks::CommandBlock;
use super::box_drawing;
use super::contrast;
use super::font::{Font, FontConfig, FontMetrics};
use super::ime::Preedit;
use super::ligature;
use super::paint::PaneOverlay;
use super::selection::SelectionKind;
use super::svg::{SvgCache, SvgMode};
use super::url::HoveredLink;

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
        Self::new_float(
            area,
            rect.origin.x as f32,
            rect.origin.y as f32,
            rect.size.width.get() as f32,
            rect.size.height.get() as f32,
            color,
            alpha,
            uv,
            sprite,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_float(
        area: PixelSize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
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
                x * scale_x - 1.0,
                1.0 - y * scale_y,
                width * scale_x,
                -height * scale_y,
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

    pub(super) fn prepare_pane(
        &mut self,
        frame: &PresentationFrame<'_>,
        cursor: Option<PresentedCursor>,
        overlay: &PaneOverlay<'_>,
        primary_blocks: &[CommandBlock],
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
                primary_blocks,
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
        let mut boundaries: Vec<usize> = text.char_indices().map(|(idx, _)| idx).collect();
        boundaries.push(text.len());
        let mut lo = 0usize;
        let mut hi = boundaries.len() - 1;
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            if self.font.measure_text(metrics, &text[..boundaries[mid]]) <= budget {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let end = boundaries[lo];
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

    pub(super) fn prepare_moving_cursor(
        &mut self,
        area: PixelSize,
        origin: [f32; 2],
        size: PixelSize,
        color: Rgb,
    ) {
        let quad = Quad::new_float(
            area,
            origin[0],
            origin[1],
            size.width.get() as f32,
            size.height.get() as f32,
            color,
            1.0,
            [0.0; 4],
            [0, 0],
        );
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
