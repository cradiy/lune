use std::collections::HashMap;
use std::num::NonZeroU32;
use std::ops::Range;

use guillotiere::{Allocation, AtlasAllocator, size2};
use lune_terminal::{
    CellPosition, CellText, Color, CursorShape, DisplayUnit, PixelPosition, PixelSize, Rgb,
    SizedTextRow, Style, Underline,
};

use crate::gpu::{create_quad_pipeline, linear_colors, write_instances};
use crate::{PixelRect, PresentationFrame, PresentationStyle, PresentedCursor};

use super::font::{Font, FontMetrics};

const ATLAS_SIDE: i32 = 1024;
const MAX_ATLAS_LAYERS: u32 = 32;
const LABEL_CACHE: usize = 8;
const ICON_CACHE: usize = 9;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Quad {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 4],
    sprite: [u32; 2],
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
    glyphs: [HashMap<Box<str>, Sprite>; ICON_CACHE + 4],
    sized_glyphs: HashMap<SizedRaster, HashMap<Box<str>, Sprite>>,
    font: Font,
    metrics: FontMetrics,
    generation: u64,
    evicted: bool,
    image: Vec<u8>,
    instances: Vec<Quad>,
    buffer: Option<wgpu::Buffer>,
    area: PixelSize,
}

impl TextRenderer {
    pub(super) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        font: Font,
        metrics: FontMetrics,
    ) -> Self {
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
            font,
            metrics,
            generation: 0,
            evicted: false,
            image: Vec::new(),
            instances: Vec::new(),
            buffer: None,
            area: metrics.cell,
        }
    }

    pub(super) fn metrics(&self) -> FontMetrics {
        self.metrics
    }

    pub(super) fn measure(&mut self, size: f32) -> FontMetrics {
        self.font.measure(size)
    }

    pub(super) fn set_metrics(&mut self, metrics: FontMetrics) {
        if self.metrics == metrics {
            return;
        }
        self.metrics = metrics;
        for layer in &mut self.layers {
            layer.clear();
        }
        for cache in &mut self.glyphs {
            cache.clear();
        }
        self.sized_glyphs.clear();
    }

    pub(super) fn prepare(
        &mut self,
        frame: &PresentationFrame<'_>,
        cursor: Option<PresentedCursor>,
        selection: Option<&Range<CellPosition>>,
        preedit: &str,
        vi: Option<&super::vi::Mode>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.area = frame.geometry.text_area_pixels().unwrap();
        self.generation += 1;
        self.evicted = false;
        self.instances.clear();
        let columns = usize::from(frame.geometry.pty_size().columns);
        let first_row = frame.viewport.range().start;
        let text_selection =
            selection.and_then(|range| frame.viewport.source().selection_text(range.clone()).ok());
        let sized_cursor = cursor.and_then(|cursor| {
            let pos = cursor.position;
            frame
                .viewport
                .row(pos.row)?
                .sized_text_at(pos.column)
                .map(|part| (cursor, part))
        });
        for (row, source) in frame.viewport.iter_rows().enumerate() {
            let mut sized = source.sized_text_rows().peekable();
            let selected = text_selection.and_then(|range| range.columns_on_row(first_row + row));
            let cursor_column = cursor
                .filter(|cursor| cursor.position.row == row && cursor.appearance.visible)
                .map(|cursor| cursor.position.column);
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
            if let Some(runs) = frame.row_runs(row, 0..columns) {
                for run in runs {
                    let run = run?;
                    let style = run.style;
                    if style.style.hidden {
                        continue;
                    }
                    let alpha = if style.style.faint { 0.6 } else { 1.0 };
                    for unit in run.units() {
                        if unit.cell.is_erased() || unit.text == CellText::Scalar(' ') {
                            continue;
                        }
                        let unit_selected = selected
                            .as_ref()
                            .is_some_and(|span| span.contains(&unit.column));
                        let color = if unit_selected {
                            frame.palette.graphics_color(251)
                        } else {
                            style.foreground
                        };
                        if let Some(rect) = frame
                            .geometry
                            .row_rect(row, unit.column..unit.column + unit.cell.width())
                        {
                            if let Some(icon) = icon_character(unit) {
                                let next = unit.column + 1;
                                let can_extend = next < columns
                                    && next < source.wrap_end().unwrap_or(source.cells().len())
                                    && cursor_column != Some(unit.column)
                                    && cursor_column != Some(next)
                                    && source.sized_text_at(next).is_none()
                                    && selected.as_ref().is_some_and(|span| span.contains(&next))
                                        == unit_selected
                                    && source.cells().get(next).is_some_and(|cell| {
                                        (cell.is_erased()
                                            || (cell.character() == Some(' ')
                                                && !cell.has_extended_text()))
                                            && frame.resolve_style(cell.style()).is_some_and(
                                                |next| next.background == style.background,
                                            )
                                    });
                                self.icon(rect, icon, style.style, color, alpha, can_extend);
                            } else {
                                self.unit(rect, unit, style.style, color, alpha);
                            }
                        }
                    }
                    if style.style.strikethrough || style.style.underline != Underline::None {
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
                                    style,
                                    alpha,
                                );
                            }
                            columns.start = columns.start.max(part.end);
                            if part.end > columns.end {
                                break;
                            }
                            sized.next();
                        }
                        self.row_decorations(frame, row, columns, style, alpha);
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
                    }
                    let alpha = if style.style.faint { 0.6 } else { 1.0 };
                    let rendition =
                        usize::from(style.style.bold) | (usize::from(style.style.italic) << 1);
                    self.sized_glyph(area, part.text(), rendition, style.foreground, alpha);
                    self.decorations(area, style, alpha);
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
        if let Some(cursor) = cursor
            && sized_cursor.is_none()
            && let Some(paint) = frame.cursor_paint(cursor, NonZeroU32::new(2).unwrap())?
        {
            self.rect(paint.rect, paint.color, 1.0);
            if let Some(glyph) = paint.glyph {
                if let Some(icon) = icon_character(glyph.unit) {
                    self.icon(paint.rect, icon, glyph.style, glyph.color, 1.0, false);
                } else {
                    self.unit(paint.rect, glyph.unit, glyph.style, glyph.color, 1.0);
                }
            }
        }
        if let Some(vi) = vi {
            if let Some(prompt) = &vi.prompt {
                let text = if preedit.is_empty() {
                    std::borrow::Cow::Borrowed(prompt.text())
                } else {
                    std::borrow::Cow::Owned(format!("{}{preedit}", prompt.text()))
                };
                if let Some(rect) = frame
                    .geometry
                    .row_rect(frame.viewport.rows() - 1, 0..columns)
                {
                    self.label(rect, &text, frame, true);
                }
            } else {
                let text = match columns {
                    1 => "V",
                    2..=3 => "VI",
                    _ => " VI ",
                };
                if let Some(rect) = frame.geometry.row_rect(0, columns - text.len()..columns) {
                    self.label(rect, text, frame, false);
                }
            }
        } else if !preedit.is_empty()
            && let Some(cursor) = frame.viewport.cursor()
            && let Some(rect) = frame.geometry.row_rect(cursor.row, cursor.column..columns)
        {
            self.label(rect, preedit, frame, false);
        }
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

    fn label(
        &mut self,
        mut rect: PixelRect,
        text: &str,
        frame: &PresentationFrame<'_>,
        fill: bool,
    ) {
        let width = self
            .font
            .measure_text(self.metrics, text)
            .min(rect.size.width.get())
            .min(ATLAS_SIDE as u32);
        if let Some(width) = NonZeroU32::new(width) {
            let row = rect;
            rect.size.width = width;
            self.rect(
                if fill { row } else { rect },
                frame.palette.graphics_color(250),
                1.0,
            );
            self.glyph(
                rect,
                text,
                LABEL_CACHE,
                frame.palette.graphics_color(251),
                1.0,
            );
        }
    }

    pub(super) fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if let Some(buffer) = &self.buffer
            && !self.instances.is_empty()
        {
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..4, 0..self.instances.len() as u32);
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

    fn glyph(&mut self, rect: PixelRect, text: &str, cache_index: usize, color: Rgb, alpha: f32) {
        let width = rect.size.width.get();
        let cache = &mut self.glyphs[cache_index];
        if cache_index == LABEL_CACHE
            && cache
                .get(text)
                .is_some_and(|sprite| sprite.allocation.rectangle.width() != width as i32)
        {
            let sprite = cache.remove(text).unwrap();
            self.layers[sprite.layer].deallocate(sprite.allocation.id);
        }
        let sprite = if let Some(sprite) = cache.get_mut(text) {
            sprite.generation = self.generation;
            *sprite
        } else {
            let Some(colored) =
                self.font
                    .rasterize(self.metrics, text, cache_index & 3, width, &mut self.image)
            else {
                return;
            };
            let height = rect.size.height.get();
            let Some(sprite) = self.upload_sprite(width, height, colored) else {
                return;
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
    }

    fn icon(
        &mut self,
        mut rect: PixelRect,
        icon: char,
        style: &Style,
        color: Rgb,
        alpha: f32,
        can_extend: bool,
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
        rect.origin.y += top.min(height - draw_height);
        rect.size.width = NonZeroU32::new(draw_width).unwrap();
        rect.size.height = NonZeroU32::new(draw_height).unwrap();
        self.quad(
            rect,
            color,
            alpha,
            [
                native.min.x as f32,
                native.min.y as f32,
                width as f32,
                height as f32,
            ],
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
                    .rasterize(metrics, text, rendition, key.width, &mut self.image)
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
            line(y, style.underline_color);
            if style.style.underline == Underline::Double {
                let second = y.saturating_sub(2 * thickness);
                if second != y {
                    line(second, style.underline_color);
                }
            }
        }
    }

    fn rect(&mut self, rect: PixelRect, color: Rgb, alpha: f32) {
        self.quad(rect, color, alpha, [0.0; 4], [0, 0]);
    }

    fn quad(&mut self, rect: PixelRect, color: Rgb, alpha: f32, uv: [f32; 4], sprite: [u32; 2]) {
        let scale_x = 2.0 / self.area.width.get() as f32;
        let scale_y = 2.0 / self.area.height.get() as f32;
        let lut = linear_colors();
        self.instances.push(Quad {
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
        });
    }
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
