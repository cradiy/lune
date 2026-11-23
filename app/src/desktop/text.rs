use std::collections::HashMap;
use std::num::NonZeroU32;
use std::ops::Range;

use guillotiere::{Allocation, AtlasAllocator, size2};
use lune_terminal::{
    CellPosition, CellText, DisplayUnit, PixelPosition, PixelSize, Rgb, Style, Underline,
};

use crate::gpu::{create_quad_pipeline, linear_colors, write_instances};
use crate::{PixelRect, PresentationFrame, PresentedCursor};

use super::font::{Font, FontMetrics};

const ATLAS_SIDE: i32 = 1024;
const MAX_ATLAS_LAYERS: u32 = 32;

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

pub(super) struct TextRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    atlas: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    layers: Vec<AtlasAllocator>,
    glyphs: [HashMap<Box<str>, Sprite>; 9],
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
    }

    pub(super) fn prepare(
        &mut self,
        frame: &PresentationFrame<'_>,
        cursor: Option<PresentedCursor>,
        selection: Option<&Range<CellPosition>>,
        preedit: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.area = frame.geometry.text_area_pixels().unwrap();
        self.generation += 1;
        self.evicted = false;
        self.instances.clear();
        let columns = usize::from(frame.geometry.pty_size().columns);
        for row in 0..frame.viewport.rows() {
            let selected =
                selection.and_then(|range| super::selection::columns(range, row, columns));
            if let Some(rect) = selected
                .clone()
                .and_then(|span| frame.geometry.row_rect(row, span))
            {
                self.rect(rect, frame.palette.graphics_color(250), 1.0);
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
                        let color = if selected
                            .as_ref()
                            .is_some_and(|span| span.contains(&unit.column))
                        {
                            frame.palette.graphics_color(251)
                        } else {
                            style.foreground
                        };
                        if let Some(rect) = frame
                            .geometry
                            .row_rect(row, unit.column..unit.column + unit.cell.width())
                        {
                            self.unit(rect, unit, style.style, color, alpha);
                        }
                    }
                    if let Some(rect) = frame.geometry.row_rect(row, run.columns()) {
                        self.decorations(
                            rect,
                            style.style,
                            style.foreground,
                            style.underline_color,
                            alpha,
                        );
                    }
                }
            }
        }
        if let Some(cursor) = cursor
            && let Some(paint) = frame.cursor_paint(cursor, NonZeroU32::new(2).unwrap())?
        {
            self.rect(paint.rect, paint.color, 1.0);
            if let Some(glyph) = paint.glyph {
                self.unit(paint.rect, glyph.unit, glyph.style, glyph.color, 1.0);
            }
        }
        if !preedit.is_empty()
            && let Some(cursor) = frame.viewport.cursor()
            && let Some(mut rect) = frame.geometry.row_rect(cursor.row, cursor.column..columns)
        {
            let width = self
                .font
                .measure_text(preedit)
                .min(rect.size.width.get())
                .min(ATLAS_SIDE as u32);
            if let Some(width) = NonZeroU32::new(width) {
                rect.size.width = width;
                self.rect(rect, frame.palette.graphics_color(250), 1.0);
                self.glyph(rect, preedit, 8, frame.palette.graphics_color(251), 1.0);
            }
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
        if cache_index == 8
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
            let Some((layer, allocation)) =
                self.allocate_sprite(size2(width as i32, height as i32))
            else {
                return;
            };
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
            let sprite = Sprite {
                allocation,
                layer,
                colored,
                generation: self.generation,
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

    fn allocate_sprite(&mut self, size: guillotiere::Size) -> Option<(usize, Allocation)> {
        if let Some(allocation) = allocate(&mut self.layers, size) {
            return Some(allocation);
        }
        if !self.evicted {
            self.evicted = true;
            for cache in &mut self.glyphs {
                cache.retain(|_, sprite| {
                    if sprite.generation == self.generation {
                        return true;
                    }
                    self.layers[sprite.layer].deallocate(sprite.allocation.id);
                    false
                });
            }
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

    fn decorations(&mut self, rect: PixelRect, style: &Style, fg: Rgb, underline: Rgb, alpha: f32) {
        let height = rect.size.height.get();
        let thickness = (self.metrics.size / 14.0).round().max(1.0) as u32;
        let baseline = self.metrics.baseline.max(0) as u32;
        if style.strikethrough {
            let y = baseline.saturating_sub(height / 4);
            self.rect(horizontal(rect, y, thickness), fg, alpha);
        }
        if style.underline != Underline::None {
            let y = (baseline + thickness).min(height - thickness);
            self.rect(horizontal(rect, y, thickness), underline, alpha);
            if style.underline == Underline::Double {
                let second = y.saturating_sub(2 * thickness);
                self.rect(horizontal(rect, second, thickness), underline, alpha);
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
