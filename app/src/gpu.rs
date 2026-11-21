use std::collections::HashMap;
use std::ops::Range;

use lune_terminal::{
    GraphicsImage, GraphicsImageFormat, GraphicsImageKey, GraphicsPlacement, PixelSize, Row,
    RowRevision,
};
use wgpu::{
    BindGroup, Buffer, Device, Extent3d, Queue, RenderPipeline, Sampler, TexelCopyBufferLayout,
    Texture, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages, TextureView,
};

use crate::{GridGeometry, PresentationFrame};

mod placeholders;

type Instance = [f32; 8];

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
    revision: u64,
    format: GraphicsImageFormat,
    binding: BindGroup,
}

struct Draw {
    image_id: GraphicsImageKey,
    layer: GraphicsLayer,
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
    instances: Vec<Instance>,
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
        Self {
            pipeline: Self::create_pipeline(device, &module, format, "fragment_main"),
            indexed: Self::create_pipeline(device, &module, format, "indexed_main"),
            indexed_alpha: Self::create_pipeline(device, &module, format, "indexed_alpha_main"),
            sampler: device.create_sampler(&wgpu::SamplerDescriptor::default()),
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

    fn create_pipeline(
        device: &Device,
        module: &wgpu::ShaderModule,
        format: TextureFormat,
        fragment: &str,
    ) -> RenderPipeline {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: None,
            layout: None,
            vertex: wgpu::VertexState {
                module,
                entry_point: Some("vertex_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4],
                })],
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
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        })
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

    /// Builds the image pass for a viewport. Returns the number of placements
    /// whose textures exceeded the device limits.
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
                    .eq(self.placeholder_rows.iter().copied()))
        {
            if prepared.storage_revision != storage_revision {
                self.images.retain(|&id, cached| {
                    frame.graphics_image_revision(id) == Some(cached.revision)
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
            self.placeholder_rows
                .extend(frame.viewport.iter_rows().map(Row::revision));
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
        self.images
            .retain(|&id, cached| frame.graphics_image_revision(id) == Some(cached.revision));
        let Some((cell, area)) = frame
            .geometry
            .cell_pixels()
            .zip(frame.geometry.text_area_pixels())
        else {
            return 0;
        };
        let mut rejected = 0;
        let placeholders = placeholders::collect(frame);
        let buffer_limit = device.limits().max_buffer_size;
        let max_instances = (buffer_limit / size_of::<Instance>() as u64) as usize;
        for placement in frame.graphics_placements() {
            let image_id = placement.placement.image;
            let Some(image) = frame.graphics_image(placement) else {
                continue;
            };
            let mut uploaded = None;
            let mut push = |location: &ImageLocation| {
                let Some(vertices) = instance(placement, image, cell, area, location) else {
                    return;
                };
                if self.instances.len() >= max_instances
                    || !*uploaded.get_or_insert_with(|| self.cache(device, queue, image_id, image))
                {
                    rejected += 1;
                    return;
                }
                self.push_instance(placement, image.format(), vertices);
            };
            if placement.is_virtual() {
                if let Some(runs) = placeholders.get(&placement.serial()) {
                    for location in &runs.locations {
                        push(location);
                    }
                }
                continue;
            }
            let offset = frame.viewport.offset();
            let Some(origin) = placement.origin_in_view(offset, |parent| {
                placeholders.get(&parent).map(|runs| runs.top_left)
            }) else {
                continue;
            };
            let unbounded = i64::MIN..i64::MAX;
            let rows = if placement.virtual_parent().is_some() {
                unbounded.clone()
            } else {
                let offset = offset as i64;
                let rows = placement.row_clip();
                rows.start.saturating_add(offset)..rows.end.saturating_add(offset)
            };
            push(&ImageLocation {
                origin,
                clip: [unbounded, rows],
            });
        }
        let bytes = bytemuck::cast_slice(self.instances.as_slice());
        if !bytes.is_empty() {
            let needed = bytes.len() as u64;
            if self
                .buffer
                .as_ref()
                .is_none_or(|buffer| buffer.size() < needed)
            {
                self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: needed.next_power_of_two().min(buffer_limit),
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }));
            }
            queue.write_buffer(
                self.buffer.as_ref().expect("Instances have a buffer"),
                0,
                bytes,
            );
        }
        rejected
    }

    fn push_instance(
        &mut self,
        placement: &GraphicsPlacement,
        format: GraphicsImageFormat,
        instance: Instance,
    ) {
        self.has_indexed |= format != GraphicsImageFormat::Rgba;
        let image_id = placement.placement.image;
        let layer = GraphicsLayer::from_z(placement.placement.z);
        let first = self.instances.len() as u32;
        self.instances.push(instance);
        if let Some(last) = self.draws.last_mut()
            && last.layer == layer
            && last.image_id == image_id
        {
            last.instances.end += 1;
        } else {
            self.draws.push(Draw {
                layer,
                image_id,
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
        if self.images.contains_key(&id) {
            return true;
        }
        let Some(texture) = upload_graphics_image(device, queue, image, &mut self.pixel_scratch)
        else {
            return false;
        };
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let (image_binding, source_binding, source) = if image.format() == GraphicsImageFormat::Rgba
        {
            (0, 1, wgpu::BindingResource::Sampler(&self.sampler))
        } else {
            (2, 3, wgpu::BindingResource::TextureView(&self.palette_view))
        };
        let binding = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.pipeline(image.format()).get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: image_binding,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: source_binding,
                    resource: source,
                },
            ],
        });
        self.images.insert(
            id,
            CachedImage {
                revision: image.revision(),
                format: image.format(),
                binding,
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
            pass.set_bind_group(0, &image.binding, &[]);
            pass.draw(0..4, draw.instances.clone());
        }
    }
}

fn instance(
    placement: &GraphicsPlacement,
    image: &GraphicsImage,
    cell: PixelSize,
    area: PixelSize,
    location: &ImageLocation,
) -> Option<Instance> {
    let layout = placement.layout(cell);
    let cell = [f64::from(cell.width.get()), f64::from(cell.height.get())];
    let screen = [f64::from(area.width.get()), f64::from(area.height.get())];
    let dimensions = image.dimensions();
    let mut data = [0.0; 8];
    for (axis, dimension) in [dimensions.0, dimensions.1].into_iter().enumerate() {
        let origin = location.origin[axis] as f64 * cell[axis] + layout.offset[axis];
        let start = origin.max((location.clip[axis].start as f64 * cell[axis]).max(0.0));
        let end = (origin + layout.size[axis])
            .min((location.clip[axis].end as f64 * cell[axis]).min(screen[axis]));
        if start >= end {
            return None;
        }
        let direction = if axis == 0 { 1.0 } else { -1.0 };
        let source_origin = f64::from(placement.placement.source_origin[axis]);
        let source_scale = f64::from(placement.placement.source_size[axis]) / layout.size[axis];
        for (edge, pixel) in [start, end].into_iter().enumerate() {
            data[edge * 2 + axis] = (direction * (pixel / screen[axis] * 2.0 - 1.0)) as f32;
            data[4 + edge * 2 + axis] =
                ((source_origin + (pixel - origin) * source_scale) / f64::from(dimension)) as f32;
        }
    }
    Some(data)
}

fn upload_graphics_image(
    device: &Device,
    queue: &Queue,
    image: &GraphicsImage,
    scratch: &mut Vec<u8>,
) -> Option<Texture> {
    let (width, height) = image.dimensions();
    if width.max(height) > device.limits().max_texture_dimension_2d {
        return None;
    }
    let bytes_per_row = width.checked_mul(image.format().bytes_per_pixel())?;
    let format = match image.format() {
        GraphicsImageFormat::Rgba => TextureFormat::Rgba8UnormSrgb,
        GraphicsImageFormat::Indexed => TextureFormat::R8Uint,
        GraphicsImageFormat::IndexedAlpha => TextureFormat::Rg8Uint,
    };
    let pixels = image.pixels(scratch)?;
    let texture = make_texture(device, width, height, format);
    write_texture(queue, &texture, pixels, bytes_per_row);
    Some(texture)
}

fn make_texture(device: &Device, width: u32, height: u32, format: TextureFormat) -> Texture {
    let size = Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    device.create_texture(&TextureDescriptor {
        label: None,
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::COPY_DST | TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

fn write_texture(queue: &Queue, texture: &Texture, bytes: &[u8], bytes_per_row: u32) {
    queue.write_texture(
        texture.as_image_copy(),
        bytes,
        TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: None,
        },
        texture.size(),
    );
}
