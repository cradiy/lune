use std::collections::HashMap;
use std::ops::Range;

use lune_terminal::{
    GraphicsImage, GraphicsImageFormat, GraphicsImageKey, GraphicsPlacement, PixelSize, Row,
    RowRevision, graphics_parent_origins,
};
use wgpu::{Buffer, Device, Queue, RenderPipeline, Sampler, Texture, TextureFormat, TextureView};

use crate::{GridGeometry, PresentationFrame};

mod backgrounds;
mod placeholders;
mod textures;

pub use backgrounds::{BackgroundPrepareError, CellBackgroundRenderer};
use textures::{
    ImageTextures, PixelRegion, TextureBinding, TileRegion, make_texture, write_texture,
};

type ImageInstance = [f32; 10];

struct ImageLocation {
    origin: [i64; 2],
    clip: [Range<i64>; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GraphicsLayer {
    BehindBackground,
    BehindText,
    AboveText,
}

impl GraphicsLayer {
    fn from_z(z: i32) -> Self {
        if z < i32::MIN / 2 {
            Self::BehindBackground
        } else if z < 0 {
            Self::BehindText
        } else {
            Self::AboveText
        }
    }
}

struct CachedImage {
    source_revision: u64,
    revision: u64,
    format: GraphicsImageFormat,
    textures: ImageTextures,
}

struct Draw {
    image_id: GraphicsImageKey,
    layer: GraphicsLayer,
    tile: u32,
    instances: Range<u32>,
}

struct PreparedImages {
    geometry: GridGeometry,
    stamp: Option<crate::session::GraphicsStamp>,
    storage_revision: u64,
    rejected: usize,
}

pub struct ImageRenderer {
    pipeline: RenderPipeline,
    indexed: RenderPipeline,
    indexed_alpha: RenderPipeline,
    sampler: Sampler,
    palette: Texture,
    palette_view: TextureView,
    palette_revision: Option<u64>,
    has_indexed: bool,
    images: HashMap<GraphicsImageKey, CachedImage>,
    buffer: Option<Buffer>,
    instances: Vec<ImageInstance>,
    draws: Vec<Draw>,
    prepared: Option<PreparedImages>,
    placeholder_rows: Vec<RowRevision>,
    pixel_scratch: Vec<u8>,
}

impl ImageRenderer {
    pub fn new(device: &Device, format: TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu/image.wgsl").into()),
        });
        let palette = make_texture(device, 256, 1, TextureFormat::Rgba8UnormSrgb);
        let palette_view = palette.create_view(&wgpu::TextureViewDescriptor::default());
        let pipeline = |fragment, blend| {
            create_quad_pipeline(
                device,
                &module,
                format,
                "vertex_main",
                fragment,
                Some(blend),
                wgpu::VertexBufferLayout {
                    array_stride: size_of::<ImageInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x2],
                },
            )
        };
        Self {
            pipeline: pipeline("fragment_main", wgpu::BlendState::ALPHA_BLENDING),
            indexed: pipeline(
                "indexed_main",
                wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
            ),
            indexed_alpha: pipeline(
                "indexed_alpha_main",
                wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
            ),
            sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }),
            palette,
            palette_view,
            palette_revision: None,
            has_indexed: false,
            images: HashMap::new(),
            buffer: None,
            instances: Vec::new(),
            draws: Vec::new(),
            prepared: None,
            placeholder_rows: Vec::new(),
            pixel_scratch: Vec::new(),
        }
    }

    fn pipeline(&self, format: GraphicsImageFormat) -> &RenderPipeline {
        match format {
            GraphicsImageFormat::Rgba => &self.pipeline,
            GraphicsImageFormat::Indexed => &self.indexed,
            GraphicsImageFormat::IndexedAlpha => &self.indexed_alpha,
        }
    }

    #[cfg(test)]
    pub(crate) fn has_cached_image(&self, key: GraphicsImageKey) -> bool {
        self.images.contains_key(&key)
    }

    #[cfg(test)]
    pub(crate) fn cached_texture(&self, key: GraphicsImageKey) -> Option<&Texture> {
        self.images
            .get(&key)
            .map(|entry| &entry.textures.get(0).texture)
    }

    /// Builds the image pass for a viewport. Returns the number of placements
    /// whose pixels could not be uploaded. Submit the pass before preparing
    /// another frame with the same renderer.
    pub fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        frame: &PresentationFrame<'_>,
    ) -> usize {
        let stamp = frame.graphics_stamp();
        let storage_revision = frame.graphics_storage_revision();
        if let Some(prepared) = &mut self.prepared
            && prepared.geometry == frame.geometry
            && prepared.stamp == stamp
            && (!frame.has_virtual_graphics()
                || frame
                    .viewport
                    .iter_rows()
                    .map(Row::revision)
                    .chain(frame.offscreen_graphics_rows())
                    .eq(self.placeholder_rows.iter().copied()))
        {
            if prepared.storage_revision != storage_revision {
                self.images.retain(|&id, cached| {
                    frame.graphics_image_source_revision(id) == Some(cached.source_revision)
                });
                prepared.storage_revision = storage_revision;
            }
            let rejected = prepared.rejected;
            self.update_palette(queue, frame);
            return rejected;
        }
        let rejected = self.prepare_changed(device, queue, frame);
        self.placeholder_rows.clear();
        if frame.has_virtual_graphics() {
            self.placeholder_rows.extend(
                frame
                    .viewport
                    .iter_rows()
                    .map(Row::revision)
                    .chain(frame.offscreen_graphics_rows()),
            );
        }
        self.update_palette(queue, frame);
        self.prepared = Some(PreparedImages {
            geometry: frame.geometry,
            stamp,
            storage_revision,
            rejected,
        });
        rejected
    }

    fn prepare_changed(
        &mut self,
        device: &Device,
        queue: &Queue,
        frame: &PresentationFrame<'_>,
    ) -> usize {
        self.draws.clear();
        self.instances.clear();
        self.has_indexed = false;
        self.images.retain(|&id, cached| {
            frame.graphics_image_source_revision(id) == Some(cached.source_revision)
        });
        let Some((cell, area)) = frame
            .geometry
            .cell_pixels()
            .zip(frame.geometry.text_area_pixels())
        else {
            return 0;
        };
        let mut rejected = 0;
        let placeholders = placeholders::collect(frame);
        let parents = if frame.has_virtual_parents() {
            graphics_parent_origins(
                frame.graphics_placements(),
                frame.viewport.source(),
                frame.styles,
            )
            .unwrap_or_default()
        } else {
            HashMap::new()
        };
        let limits = device.limits();
        let buffer_limit = limits.max_buffer_size;
        let max_instances =
            (buffer_limit / size_of::<ImageInstance>() as u64).min(u64::from(u32::MAX)) as usize;
        let mut tile_instances = Vec::new();
        for placement in frame.graphics_placements() {
            let image_id = placement.placement.image;
            let Some(image) = frame.graphics_image(placement) else {
                continue;
            };
            let mut uploaded = None;
            let mut push = |location: &ImageLocation| {
                let Some(quad) = ImageQuad::new(placement, cell, area, location) else {
                    return;
                };
                if self.instances.len() >= max_instances
                    || !*uploaded.get_or_insert_with(|| self.cache(device, queue, image_id, image))
                {
                    rejected += 1;
                    return;
                }
                let (width, height) = image.dimensions();
                if width.max(height) <= limits.max_texture_dimension_2d {
                    self.push_instance(
                        placement,
                        image.format(),
                        0,
                        quad.instance([width, height]),
                    );
                    return;
                }
                let ImageTextures::Tiled(tiles) = &self.images[&image_id].textures else {
                    unreachable!("Oversized images are tiled");
                };
                tile_instances.clear();
                tile_instances.extend(tiles.iter().enumerate().filter_map(|(index, tile)| {
                    quad.tile_instance(tile.region)
                        .map(|vertices| (index as u32, vertices))
                }));
                if tile_instances.len() > max_instances - self.instances.len() {
                    rejected += 1;
                    return;
                }
                for &(tile, vertices) in &tile_instances {
                    self.push_instance(placement, image.format(), tile, vertices);
                }
            };
            if placement.is_virtual() {
                if let Some(runs) = placeholders.get(&placement.serial()) {
                    for location in runs {
                        push(location);
                    }
                }
                continue;
            }
            let offset = frame.viewport.offset();
            let Some(origin) =
                placement.origin_in_view(offset, |parent| parents.get(&parent).copied().flatten())
            else {
                continue;
            };
            let unbounded = i64::MIN..i64::MAX;
            push(&ImageLocation {
                origin,
                clip: [unbounded.clone(), unbounded],
            });
        }
        write_instances(device, queue, &mut self.buffer, &self.instances);
        rejected
    }

    fn push_instance(
        &mut self,
        placement: &GraphicsPlacement,
        format: GraphicsImageFormat,
        tile: u32,
        instance: ImageInstance,
    ) {
        self.has_indexed |= format != GraphicsImageFormat::Rgba;
        let image_id = placement.placement.image;
        let layer = GraphicsLayer::from_z(placement.placement.z);
        let first = self.instances.len() as u32;
        self.instances.push(instance);
        if let Some(last) = self.draws.last_mut()
            && last.layer == layer
            && last.image_id == image_id
            && last.tile == tile
        {
            last.instances.end += 1;
        } else {
            self.draws.push(Draw {
                layer,
                image_id,
                tile,
                instances: first..first + 1,
            });
        }
    }

    fn update_palette(&mut self, queue: &Queue, frame: &PresentationFrame<'_>) {
        let revision = frame.palette.revision();
        if !self.has_indexed || self.palette_revision == Some(revision) {
            return;
        }
        let colors: [[u8; 4]; 256] = std::array::from_fn(|index| {
            let color = frame.palette.graphics_color(index as u8);
            [color.red, color.green, color.blue, 255]
        });
        write_texture(queue, &self.palette, bytemuck::cast_slice(&colors), 1024);
        self.palette_revision = Some(revision);
    }

    fn cache(
        &mut self,
        device: &Device,
        queue: &Queue,
        id: GraphicsImageKey,
        image: &GraphicsImage,
    ) -> bool {
        let revision = image.revision();
        if let Some(cached) = self.images.get_mut(&id)
            && cached.source_revision == image.source_revision()
        {
            if cached.revision != revision {
                let Some(update) = image.pixel_update(cached.revision, &mut self.pixel_scratch)
                else {
                    return false;
                };
                if !update.size.contains(&0) {
                    cached.textures.write(
                        queue,
                        PixelRegion {
                            origin: update.origin,
                            size: update.size,
                        },
                        update.bytes,
                        update.bytes_per_row,
                        cached.format.bytes_per_pixel(),
                    );
                }
                cached.revision = revision;
            }
            return true;
        }
        let Some(pixels) = image.pixels(&mut self.pixel_scratch) else {
            return false;
        };
        let (width, height) = image.dimensions();
        let format = image.format();
        let pipeline = match format {
            GraphicsImageFormat::Rgba => &self.pipeline,
            GraphicsImageFormat::Indexed => &self.indexed,
            GraphicsImageFormat::IndexedAlpha => &self.indexed_alpha,
        };
        let layout = pipeline.get_bind_group_layout(0);
        let textures = ImageTextures::new(
            [width, height],
            device.limits().max_texture_dimension_2d,
            |region| {
                TextureBinding::new(
                    device,
                    &layout,
                    format,
                    &self.sampler,
                    &self.palette_view,
                    region,
                )
            },
        );
        textures.write(
            queue,
            PixelRegion {
                origin: [0, 0],
                size: [width, height],
            },
            pixels,
            width * format.bytes_per_pixel(),
            format.bytes_per_pixel(),
        );
        self.images.insert(
            id,
            CachedImage {
                source_revision: image.source_revision(),
                revision,
                format,
                textures,
            },
        );
        true
    }

    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, layer: GraphicsLayer) {
        let start = self.draws.partition_point(|draw| draw.layer < layer);
        let draws = &self.draws[start..];
        if draws.first().is_none_or(|draw| draw.layer != layer) {
            return;
        }
        let buffer = self.buffer.as_ref().expect("Instances have a buffer");
        pass.set_vertex_buffer(0, buffer.slice(..));
        let mut format = None;
        for draw in draws.iter().take_while(|draw| draw.layer == layer) {
            let image = &self.images[&draw.image_id];
            if format != Some(image.format) {
                pass.set_pipeline(self.pipeline(image.format));
                format = Some(image.format);
            }
            pass.set_bind_group(0, &image.textures.get(draw.tile as usize).binding, &[]);
            pass.draw(0..4, draw.instances.clone());
        }
    }
}

fn create_quad_pipeline(
    device: &Device,
    module: &wgpu::ShaderModule,
    format: TextureFormat,
    vertex: &str,
    fragment: &str,
    blend: Option<wgpu::BlendState>,
    buffer_layout: wgpu::VertexBufferLayout<'_>,
) -> RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: None,
        layout: None,
        vertex: wgpu::VertexState {
            module,
            entry_point: Some(vertex),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[Some(buffer_layout)],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fragment),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn write_instances<T: bytemuck::Pod>(
    device: &Device,
    queue: &Queue,
    buffer: &mut Option<Buffer>,
    instances: &[T],
) {
    let bytes = bytemuck::cast_slice(instances);
    if bytes.is_empty() {
        return;
    }
    let needed = bytes.len() as u64;
    if buffer.as_ref().is_none_or(|buffer| buffer.size() < needed) {
        *buffer = Some(
            device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: needed
                    .next_power_of_two()
                    .min(device.limits().max_buffer_size),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
        );
    }
    queue.write_buffer(buffer.as_ref().expect("Instances have a buffer"), 0, bytes);
}

struct ImageQuad {
    bounds: [[f64; 2]; 2],
    source: [[f64; 2]; 2],
    sample_rows: [f64; 2],
}

impl ImageQuad {
    fn new(
        placement: &GraphicsPlacement,
        cell: PixelSize,
        area: PixelSize,
        location: &ImageLocation,
    ) -> Option<Self> {
        let layout = placement.layout(cell);
        let cell = [f64::from(cell.width.get()), f64::from(cell.height.get())];
        let screen = [f64::from(area.width.get()), f64::from(area.height.get())];
        let mut quad = Self {
            bounds: [[0.0; 2]; 2],
            source: [[0.0; 2]; 2],
            sample_rows: layout.sample_rows,
        };
        for axis in 0..2 {
            let origin = location.origin[axis] as f64 * cell[axis] + layout.offset[axis];
            let start = origin.max((location.clip[axis].start as f64 * cell[axis]).max(0.0));
            let end = (origin + layout.size[axis])
                .min((location.clip[axis].end as f64 * cell[axis]).min(screen[axis]));
            if start >= end {
                return None;
            }
            let direction = if axis == 0 { 1.0 } else { -1.0 };
            let source_origin = layout.source_origin[axis];
            quad.bounds[axis] =
                [start, end].map(|pixel| direction * (pixel / screen[axis] * 2.0 - 1.0));
            quad.source[axis] = [start, end]
                .map(|pixel| source_origin + (pixel - origin) * layout.source_per_pixel);
        }
        Some(quad)
    }

    fn instance(&self, dimensions: [u32; 2]) -> ImageInstance {
        let [[left, right], [top, bottom]] = self.bounds;
        let [[x0, x1], [y0, y1]] = self.source;
        let [width, height] = dimensions.map(f64::from);
        let [first_row, last_row] = self.sample_rows.map(|row| row.clamp(0.0, height) / height);
        [
            left,
            top,
            right,
            bottom,
            x0 / width,
            y0 / height,
            x1 / width,
            y1 / height,
            first_row,
            last_row,
        ]
        .map(|v| v as f32)
    }

    fn tile_instance(&self, tile: TileRegion) -> Option<ImageInstance> {
        let mut quad = Self {
            bounds: [[0.0; 2]; 2],
            source: [[0.0; 2]; 2],
            sample_rows: self
                .sample_rows
                .map(|row| row - f64::from(tile.storage.origin[1])),
        };
        for axis in 0..2 {
            let tile_origin = f64::from(tile.visible.origin[axis]);
            let [start, end] = self.source[axis];
            let first = start.max(tile_origin);
            let last = end.min(tile_origin + f64::from(tile.visible.size[axis]));
            if first >= last {
                return None;
            }
            let [lower, upper] = self.bounds[axis];
            let scale = (upper - lower) / (end - start);
            quad.bounds[axis] = [first, last].map(|pixel| lower + (pixel - start) * scale);
            let storage_origin = f64::from(tile.storage.origin[axis]);
            quad.source[axis] = [first - storage_origin, last - storage_origin];
        }
        Some(quad.instance(tile.storage.size))
    }
}
