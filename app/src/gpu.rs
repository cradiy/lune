use std::collections::HashMap;
use std::ops::Range;

use lune_terminal::{
    GraphicsImage, GraphicsImageFormat, GraphicsImageKey, GraphicsPixelRevision, GraphicsPlacement,
    PixelSize, Row, RowRevision, ScreenBuffer, graphics_parent_origins,
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

type ImageInstance = [f32; 12];

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
    revision: GraphicsPixelRevision,
    format: GraphicsImageFormat,
    textures: ImageTextures,
}

impl CachedImage {
    fn update_pixels(
        &mut self,
        queue: &Queue,
        image: &GraphicsImage,
        scratch: &mut Vec<u8>,
    ) -> bool {
        if self.revision.id() != image.revision() {
            let Some(updates) = image.pixel_updates(Some(&self.revision), scratch) else {
                return false;
            };
            for update in updates
                .as_slice()
                .iter()
                .filter(|update| !update.size.contains(&0))
            {
                self.textures.write(
                    queue,
                    PixelRegion {
                        origin: update.origin,
                        size: update.size,
                    },
                    update.bytes,
                    update.bytes_per_row,
                    self.format.bytes_per_pixel(),
                );
            }
            self.revision = image.pixel_revision();
        }
        true
    }
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
    rgba_sampling: [Buffer; 2],
    palette: Texture,
    palette_view: TextureView,
    palette_revision: Option<u64>,
    has_indexed: bool,
    images: HashMap<GraphicsImageKey, CachedImage>,
    inactive_images: HashMap<GraphicsImageKey, CachedImage>,
    inactive_storage_revision: Option<u64>,
    graphics_buffer: ScreenBuffer,
    buffer: Option<Buffer>,
    instances: Vec<ImageInstance>,
    draws: Vec<Draw>,
    visible_images: Vec<GraphicsImageKey>,
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
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4],
                },
            )
        };
        Self {
            pipeline: pipeline(
                "fragment_main",
                wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
            ),
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
            rgba_sampling: [0u32, 1].map(|mode| {
                use wgpu::util::{BufferInitDescriptor, DeviceExt};
                device.create_buffer_init(&BufferInitDescriptor {
                    label: None,
                    contents: bytemuck::bytes_of(&mode),
                    usage: wgpu::BufferUsages::UNIFORM,
                })
            }),
            palette,
            palette_view,
            palette_revision: None,
            has_indexed: false,
            images: HashMap::new(),
            inactive_images: HashMap::new(),
            inactive_storage_revision: None,
            graphics_buffer: ScreenBuffer::Primary,
            buffer: None,
            instances: Vec::new(),
            draws: Vec::new(),
            visible_images: Vec::new(),
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
        if self.graphics_buffer != frame.graphics_buffer() {
            self.graphics_buffer = frame.graphics_buffer();
            std::mem::swap(&mut self.images, &mut self.inactive_images);
            self.inactive_storage_revision = self.prepared.take().map(|p| p.storage_revision);
        }
        let inactive_store = frame.inactive_image_store();
        let inactive_revision = Some(inactive_store.storage_revision());
        if self.inactive_storage_revision != inactive_revision {
            self.inactive_images.retain(|&id, cached| {
                inactive_store.source_revision(id) == Some(cached.source_revision)
            });
            self.inactive_storage_revision = inactive_revision;
        }
        let stamp = frame.graphics_stamp();
        let storage_revision = frame.image_store().storage_revision();
        if let Some(prepared) = &self.prepared
            && prepared.geometry == frame.geometry
            && (prepared.stamp == stamp
                || (prepared.rejected == 0
                    && prepared
                        .stamp
                        .zip(stamp)
                        .is_some_and(|(previous, current)| previous.same_layout(current))))
            && (!frame.has_virtual_graphics()
                || frame
                    .viewport
                    .iter_rows()
                    .map(Row::revision)
                    .chain(frame.offscreen_graphics_rows())
                    .eq(self.placeholder_rows.iter().copied()))
            && (prepared.stamp == stamp || self.refresh_visible_pixels(queue, frame))
        {
            let prepared = self.prepared.as_mut().expect("A layout was prepared");
            if prepared.storage_revision != storage_revision {
                self.images.retain(|&id, cached| {
                    frame.image_store().source_revision(id) == Some(cached.source_revision)
                });
                prepared.storage_revision = storage_revision;
            }
            prepared.stamp = stamp;
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

    fn refresh_visible_pixels(&mut self, queue: &Queue, frame: &PresentationFrame<'_>) -> bool {
        for &key in &self.visible_images {
            let Some((image, cached)) = frame
                .image_store()
                .get(key.id())
                .zip(self.images.get_mut(&key))
            else {
                return false;
            };
            if cached.source_revision != image.source_revision()
                || !cached.update_pixels(queue, image, &mut self.pixel_scratch)
            {
                return false;
            }
        }
        true
    }

    fn prepare_changed(
        &mut self,
        device: &Device,
        queue: &Queue,
        frame: &PresentationFrame<'_>,
    ) -> usize {
        self.draws.clear();
        self.instances.clear();
        self.visible_images.clear();
        self.has_indexed = false;
        self.images.retain(|&id, cached| {
            frame.image_store().source_revision(id) == Some(cached.source_revision)
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
            let previous_instances = self.instances.len();
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
                tile_instances.extend(tiles.candidates(quad.source_region(), 0).filter_map(
                    |(index, tile)| {
                        quad.tile_instance(tile.region)
                            .map(|vertices| (index as u32, vertices))
                    },
                ));
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
            } else if let Some(origin) = placement
                .origin_in_view(frame.viewport.offset(), |parent| {
                    parents.get(&parent).copied().flatten()
                })
            {
                let unbounded = i64::MIN..i64::MAX;
                push(&ImageLocation {
                    origin,
                    clip: [unbounded.clone(), unbounded],
                });
            }
            if image_id.id() != 0 && self.instances.len() != previous_instances {
                self.visible_images.push(image_id);
            }
        }
        self.visible_images.sort_unstable();
        self.visible_images.dedup();
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
        if let Some(cached) = self.images.get_mut(&id)
            && cached.source_revision == image.source_revision()
        {
            return cached.update_pixels(queue, image, &mut self.pixel_scratch);
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
        let gather_first = format == GraphicsImageFormat::Rgba && prefer_alpha_gather(pixels);
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
                    &self.rgba_sampling[usize::from(gather_first)],
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
                revision: image.pixel_revision(),
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
    sample_bounds: [[f64; 2]; 2],
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
            sample_bounds: layout.sample_bounds,
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

    fn source_region(&self) -> PixelRegion {
        let origin = self.source.map(|[start, _]| start.floor() as u32);
        PixelRegion {
            origin,
            size: std::array::from_fn(|axis| {
                (self.source[axis][1].ceil() as u32).saturating_sub(origin[axis])
            }),
        }
    }

    fn instance(&self, dimensions: [u32; 2]) -> ImageInstance {
        let [[left, right], [top, bottom]] = self.bounds;
        let [[x0, x1], [y0, y1]] = self.source;
        let [u, v] = dimensions.map(|extent| f64::from(extent).recip());
        let [[u_min, v_min], [u_max, v_max]] = self
            .sample_bounds
            .map(|[x, y]| [(x * u).clamp(0.0, 1.0), (y * v).clamp(0.0, 1.0)]);
        [
            left,
            top,
            right,
            bottom,
            x0 * u,
            y0 * v,
            x1 * u,
            y1 * v,
            u_min,
            v_min,
            u_max,
            v_max,
        ]
        .map(|v| v as f32)
    }

    fn tile_instance(&self, tile: TileRegion) -> Option<ImageInstance> {
        let storage_origin = tile.storage.origin.map(f64::from);
        let mut quad = Self {
            bounds: [[0.0; 2]; 2],
            source: [[0.0; 2]; 2],
            sample_bounds: self
                .sample_bounds
                .map(|[x, y]| [x - storage_origin[0], y - storage_origin[1]]),
        };
        for (axis, storage_start) in storage_origin.into_iter().enumerate() {
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
            quad.source[axis] = [first - storage_start, last - storage_start];
        }
        Some(quad.instance(tile.storage.size))
    }
}

fn prefer_alpha_gather(bytes: &[u8]) -> bool {
    let pixels = bytes.as_chunks::<4>().0;
    let mut compared = 0;
    let mut different = 0;
    // Compare neighbors in short spans spread across the texture.
    for span in pixels.chunks(pixels.len().div_ceil(8).max(4)) {
        for adjacent in span[..span.len().min(4)].windows(2) {
            compared += 1;
            different += usize::from(adjacent[0][3] != adjacent[1][3]);
        }
    }
    different != 0 && different * 2 >= compared
}
