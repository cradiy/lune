use lune_terminal::GraphicsImageFormat;
use wgpu::{
    BindGroup, BindGroupLayout, Device, Extent3d, Queue, Sampler, TexelCopyBufferLayout, Texture,
    TextureDescriptor, TextureDimension, TextureFormat, TextureUsages, TextureView,
};

#[derive(Clone, Copy)]
pub(super) struct PixelRegion {
    pub origin: [u32; 2],
    pub size: [u32; 2],
}

impl PixelRegion {
    fn intersection(self, other: Self) -> Option<Self> {
        let origin = std::array::from_fn(|axis| self.origin[axis].max(other.origin[axis]));
        let size = std::array::from_fn(|axis| {
            (self.origin[axis] + self.size[axis])
                .min(other.origin[axis] + other.size[axis])
                .saturating_sub(origin[axis])
        });
        (!size.contains(&0)).then_some(Self { origin, size })
    }
}

pub(super) struct TextureBinding {
    pub region: TileRegion,
    pub texture: Texture,
    pub binding: BindGroup,
}

#[derive(Clone, Copy)]
pub(super) struct TileRegion {
    pub visible: PixelRegion,
    pub storage: PixelRegion,
}

impl TextureBinding {
    pub fn new(
        device: &Device,
        layout: &BindGroupLayout,
        format: GraphicsImageFormat,
        sampler: &Sampler,
        palette_view: &TextureView,
        region: TileRegion,
    ) -> Self {
        let (format, image_binding, source_binding, source) = match format {
            GraphicsImageFormat::Rgba => (
                TextureFormat::Rgba8UnormSrgb,
                0,
                1,
                wgpu::BindingResource::Sampler(sampler),
            ),
            GraphicsImageFormat::Indexed => (
                TextureFormat::R8Uint,
                2,
                3,
                wgpu::BindingResource::TextureView(palette_view),
            ),
            GraphicsImageFormat::IndexedAlpha => (
                TextureFormat::Rg8Uint,
                2,
                3,
                wgpu::BindingResource::TextureView(palette_view),
            ),
        };
        let [width, height] = region.storage.size;
        let texture = make_texture(device, width, height, format);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let binding = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout,
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
        Self {
            region,
            texture,
            binding,
        }
    }
}

pub(super) enum ImageTextures {
    Single(TextureBinding),
    Tiled(Vec<TextureBinding>),
}

impl ImageTextures {
    pub fn new(
        size: [u32; 2],
        limit: u32,
        mut create_tile: impl FnMut(TileRegion) -> TextureBinding,
    ) -> Self {
        let [width, height] = size;
        if width.max(height) <= limit {
            let region = PixelRegion {
                origin: [0, 0],
                size,
            };
            return Self::Single(create_tile(TileRegion {
                visible: region,
                storage: region,
            }));
        }
        // Include a neighboring texel at internal edges for linear filtering.
        let [step_x, step_y] = size.map(|dimension| {
            if dimension > limit {
                limit - 2
            } else {
                dimension
            }
        });
        let count = width.div_ceil(step_x) * height.div_ceil(step_y);
        let mut tiles = Vec::with_capacity(count as usize);
        for y in (0..height).step_by(step_y as usize) {
            for x in (0..width).step_by(step_x as usize) {
                let visible = PixelRegion {
                    origin: [x, y],
                    size: [(width - x).min(step_x), (height - y).min(step_y)],
                };
                let origin = visible.origin.map(|edge| edge.saturating_sub(1));
                let storage = PixelRegion {
                    origin,
                    size: std::array::from_fn(|axis| {
                        (visible.origin[axis] + visible.size[axis] + 1).min(size[axis])
                            - origin[axis]
                    }),
                };
                tiles.push(create_tile(TileRegion { visible, storage }));
            }
        }
        Self::Tiled(tiles)
    }

    pub fn get(&self, index: usize) -> &TextureBinding {
        match self {
            Self::Single(tile) => tile,
            Self::Tiled(tiles) => &tiles[index],
        }
    }

    pub fn write(
        &self,
        queue: &Queue,
        region: PixelRegion,
        bytes: &[u8],
        bytes_per_row: u32,
        bytes_per_pixel: u32,
    ) {
        match self {
            Self::Single(tile) => write_region(queue, &tile.texture, region, bytes, bytes_per_row),
            Self::Tiled(tiles) => {
                for tile in tiles {
                    let storage = tile.region.storage;
                    let Some(mut clip) = storage.intersection(region) else {
                        continue;
                    };
                    let [x, y] = clip.origin;
                    let offset = (y - region.origin[1]) as usize * bytes_per_row as usize
                        + (x - region.origin[0]) as usize * bytes_per_pixel as usize;
                    clip.origin = [x - storage.origin[0], y - storage.origin[1]];
                    write_region(queue, &tile.texture, clip, &bytes[offset..], bytes_per_row);
                }
            }
        }
    }
}

pub(super) fn make_texture(
    device: &Device,
    width: u32,
    height: u32,
    format: TextureFormat,
) -> Texture {
    device.create_texture(&TextureDescriptor {
        label: None,
        size: Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::COPY_DST | TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

pub(super) fn write_texture(queue: &Queue, texture: &Texture, bytes: &[u8], bytes_per_row: u32) {
    write_region(
        queue,
        texture,
        PixelRegion {
            origin: [0, 0],
            size: [texture.width(), texture.height()],
        },
        bytes,
        bytes_per_row,
    );
}

fn write_region(
    queue: &Queue,
    texture: &Texture,
    region: PixelRegion,
    bytes: &[u8],
    bytes_per_row: u32,
) {
    let [x, y] = region.origin;
    let [width, height] = region.size;
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            origin: wgpu::Origin3d { x, y, z: 0 },
            ..texture.as_image_copy()
        },
        bytes,
        TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: None,
        },
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}
