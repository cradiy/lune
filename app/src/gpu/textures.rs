use lune_terminal::{GraphicsImage, GraphicsImageFormat, GraphicsPixelBounds};
use wgpu::{
    BindGroup, BindGroupLayout, Device, Extent3d, Queue, Sampler, TexelCopyBufferLayout, Texture,
    TextureDescriptor, TextureDimension, TextureFormat, TextureUsages, TextureView,
};

const FILTER_BORDER: u32 = 1;

#[derive(Clone, Copy)]
pub(super) struct PixelRegion {
    pub origin: [u32; 2],
    pub size: [u32; 2],
}

impl PixelRegion {
    pub fn pixel_count(self) -> u32 {
        self.size[0] * self.size[1]
    }

    pub fn bounding_rect(self, other: Self) -> Self {
        let origin = std::array::from_fn(|axis| self.origin[axis].min(other.origin[axis]));
        let size = std::array::from_fn(|axis| {
            (self.origin[axis] + self.size[axis]).max(other.origin[axis] + other.size[axis])
                - origin[axis]
        });
        Self { origin, size }
    }

    pub fn relative_to(self, origin: [u32; 2]) -> Self {
        Self {
            origin: std::array::from_fn(|axis| self.origin[axis] - origin[axis]),
            size: self.size,
        }
    }

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
        rgba_sampling: &wgpu::Buffer,
        size: [u32; 2],
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
        let [width, height] = size;
        let texture = make_texture(device, width, height, format);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let entries = [
            wgpu::BindGroupEntry {
                binding: image_binding,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: source_binding,
                resource: source,
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: rgba_sampling.as_entire_binding(),
            },
        ];
        let binding = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout,
            entries: if image_binding == 0 {
                &entries
            } else {
                &entries[..2]
            },
        });
        Self { texture, binding }
    }
}

pub(super) enum ImageTextures {
    Single(TextureBinding),
    Tiled(TextureTiles),
}

pub(super) struct TextureTiles {
    pub tiles: Vec<TextureTile>,
    pub layout: u64,
    pub max_tile_pixels: u32,
    step: [u32; 2],
    count: [u32; 2],
}

pub(super) struct TextureTile {
    pub region: TileRegion,
    pub resident: Option<TextureBinding>,
    pub visible_layout: u64,
    pub damage: TileDamage,
}

#[derive(Clone, Copy)]
pub(super) enum TileDamage {
    Clean,
    One(PixelRegion),
    Two([PixelRegion; 2]),
    Full,
}

impl TileDamage {
    fn add(&mut self, region: PixelRegion) {
        *self = match *self {
            Self::Clean => Self::One(region),
            Self::One(first) if first.intersection(region).is_some() => {
                Self::One(first.bounding_rect(region))
            }
            Self::One(first) => Self::Two([first, region]),
            Self::Two([first, second]) if first.intersection(region).is_some() => {
                Self::Two([first.bounding_rect(region), second])
            }
            Self::Two([first, second]) if second.intersection(region).is_some() => {
                Self::Two([first, second.bounding_rect(region)])
            }
            Self::Two([first, second]) => {
                let pair = first.bounding_rect(second);
                let union = pair.bounding_rect(region);
                let (pixels, regions) = [
                    [pair, region],
                    [first.bounding_rect(region), second],
                    [first, second.bounding_rect(region)],
                ]
                .into_iter()
                .map(|[left, right]| (left.pixel_count() + right.pixel_count(), [left, right]))
                .min_by_key(|&(pixels, _)| pixels)
                .expect("Three possible merges");
                if pixels < union.pixel_count() {
                    Self::Two(regions)
                } else {
                    Self::One(union)
                }
            }
            Self::Full => Self::Full,
        };
    }
}

impl TextureTile {
    fn resident_clip(&self, region: PixelRegion) -> Option<PixelRegion> {
        self.resident.as_ref()?;
        self.region.storage.intersection(region)
    }

    pub fn needs_upload(&self) -> bool {
        self.resident.is_none() || !matches!(self.damage, TileDamage::Clean)
    }

    pub fn needs_full_upload(&self) -> bool {
        self.resident.is_none() || matches!(self.damage, TileDamage::Full)
    }

    pub fn upload_regions(&self) -> impl Iterator<Item = PixelRegion> + use<> {
        let regions = match self.damage {
            TileDamage::Clean if self.resident.is_some() => [None; 2],
            TileDamage::Clean | TileDamage::Full => [Some(self.region.storage), None],
            TileDamage::One(region) => [Some(region), None],
            TileDamage::Two(regions) => regions.map(Some),
        };
        regions.into_iter().flatten()
    }
}

impl TextureTiles {
    pub fn for_crop(
        size: [u32; 2],
        limit: u32,
        first: PixelRegion,
        mut rest: impl Iterator<Item = PixelRegion>,
    ) -> Option<Self> {
        let pixels = size[0] * size[1];
        if first.pixel_count() > pixels / 4 {
            return None;
        }
        let tiles = Self::new(size, limit);
        let max_pixels = pixels / 2;
        if let Some(second) = rest.next() {
            let mut visited = vec![false; tiles.tiles.len()];
            let mut covered = 0;
            for region in [first, second].into_iter().chain(rest) {
                for (index, tile) in tiles.candidates(region, 0) {
                    if !std::mem::replace(&mut visited[index], true) {
                        covered += tile.region.storage.pixel_count();
                        if covered > max_pixels {
                            return None;
                        }
                    }
                }
            }
        } else {
            let covered: u32 = tiles
                .candidates(first, 0)
                .map(|(_, tile)| tile.region.storage.pixel_count())
                .sum();
            if covered > max_pixels {
                return None;
            }
        }
        Some(tiles)
    }

    pub fn candidates(
        &self,
        region: PixelRegion,
        border: u32,
    ) -> impl Iterator<Item = (usize, &TextureTile)> {
        self.candidate_indices(region, border)
            .map(|index| (index, &self.tiles[index]))
    }

    fn candidate_indices(
        &self,
        region: PixelRegion,
        border: u32,
    ) -> impl Iterator<Item = usize> + use<> {
        let columns = self.count[0];
        let [first_col, first_row] = std::array::from_fn(|axis| {
            (region.origin[axis].saturating_sub(border) / self.step[axis]).min(self.count[axis])
        });
        let [end_col, end_row] = std::array::from_fn(|axis| {
            region.origin[axis]
                .saturating_add(region.size[axis])
                .saturating_add(border)
                .div_ceil(self.step[axis])
                .min(self.count[axis])
        });
        (first_row..end_row).flat_map(move |row| {
            let start = (row * columns + first_col) as usize;
            let end = (row * columns + end_col) as usize;
            start..end
        })
    }

    pub fn new(size: [u32; 2], limit: u32) -> Self {
        let [width, height] = size;
        // Include a neighboring texel at internal edges for linear filtering.
        let [step_x, step_y] = size.map(|dimension| {
            if dimension > limit {
                limit - 2 * FILTER_BORDER
            } else {
                dimension
            }
        });
        let count = [width.div_ceil(step_x), height.div_ceil(step_y)];
        let mut tiles = Vec::with_capacity((count[0] * count[1]) as usize);
        let mut max_tile_pixels = 0;
        for y in (0..height).step_by(step_y as usize) {
            for x in (0..width).step_by(step_x as usize) {
                let visible = PixelRegion {
                    origin: [x, y],
                    size: [(width - x).min(step_x), (height - y).min(step_y)],
                };
                let origin = visible
                    .origin
                    .map(|edge| edge.saturating_sub(FILTER_BORDER));
                let storage = PixelRegion {
                    origin,
                    size: std::array::from_fn(|axis| {
                        (visible.origin[axis] + visible.size[axis] + FILTER_BORDER).min(size[axis])
                            - origin[axis]
                    }),
                };
                max_tile_pixels = max_tile_pixels.max(storage.pixel_count());
                tiles.push(TextureTile {
                    region: TileRegion { visible, storage },
                    resident: None,
                    visible_layout: 0,
                    damage: TileDamage::Clean,
                });
            }
        }
        Self {
            tiles,
            layout: 0,
            max_tile_pixels,
            step: [step_x, step_y],
            count,
        }
    }

    pub fn mark_damage(&mut self, bounds: Option<[GraphicsPixelBounds; 2]>) {
        if let Some(bounds) = bounds {
            for region in damage_regions(bounds) {
                self.visit_resident_intersections(region, |tile, clip| tile.damage.add(clip));
            }
        } else {
            for tile in &mut self.tiles {
                tile.damage = TileDamage::Full;
            }
        }
    }

    pub fn update_clipped_regions(
        &mut self,
        queue: &Queue,
        image: &GraphicsImage,
        scratch: &mut Vec<u8>,
        bounds: [GraphicsPixelBounds; 2],
    ) -> Option<bool> {
        let mut visible_limit = None;
        let mut visible_pixels = 0;
        let mut largest = 0;
        for region in damage_regions(bounds) {
            for (_, tile) in self.candidates(region, FILTER_BORDER) {
                if tile.visible_layout != self.layout {
                    continue;
                }
                let Some(clip) = tile.resident_clip(region) else {
                    continue;
                };
                if tile.needs_upload() {
                    return Some(false);
                }
                let limit = *visible_limit.get_or_insert_with(|| {
                    let pixels = damage_regions(bounds)
                        .map(PixelRegion::pixel_count)
                        .sum::<u32>();
                    if pixels <= self.max_tile_pixels {
                        pixels * 3 / 4
                    } else {
                        u32::MAX
                    }
                });
                let pixels = clip.pixel_count();
                visible_pixels += pixels;
                // Share small rasterizations when most of their pixels will be used.
                if visible_pixels > limit {
                    return None;
                }
                largest = largest.max(pixels);
            }
        }
        if visible_pixels == 0 {
            self.mark_damage(Some(bounds));
            return Some(true);
        }
        Some(self.rasterize_clipped_regions(queue, image, scratch, bounds, largest))
    }

    #[cold]
    fn rasterize_clipped_regions(
        &mut self,
        queue: &Queue,
        image: &GraphicsImage,
        scratch: &mut Vec<u8>,
        bounds: [GraphicsPixelBounds; 2],
        largest: u32,
    ) -> bool {
        let capacity = largest as usize * image.format().bytes_per_pixel() as usize;
        if scratch
            .try_reserve_exact(capacity.saturating_sub(scratch.len()))
            .is_err()
        {
            return false;
        }
        for region in damage_regions(bounds) {
            for index in self.candidate_indices(region, FILTER_BORDER) {
                let tile = &mut self.tiles[index];
                let Some(clip) = tile.resident_clip(region) else {
                    continue;
                };
                if tile.visible_layout != self.layout {
                    tile.damage.add(clip);
                    continue;
                }
                let Some(update) = image.pixel_region(clip.origin, clip.size, scratch) else {
                    return false;
                };
                let binding = tile
                    .resident
                    .as_ref()
                    .expect("A resident tile has a binding");
                write_region(
                    queue,
                    &binding.texture,
                    clip.relative_to(tile.region.storage.origin),
                    update.bytes,
                    update.bytes_per_row,
                );
            }
        }
        true
    }

    fn visit_resident_intersections(
        &mut self,
        region: PixelRegion,
        mut visit: impl FnMut(&mut TextureTile, PixelRegion),
    ) {
        // Filter borders share texels with neighboring tiles.
        for index in self.candidate_indices(region, FILTER_BORDER) {
            let tile = &mut self.tiles[index];
            if let Some(clip) = tile.resident_clip(region) {
                visit(tile, clip);
            }
        }
    }

    pub fn write(
        &mut self,
        queue: &Queue,
        region: PixelRegion,
        bytes: &[u8],
        bytes_per_row: u32,
        bytes_per_pixel: u32,
    ) {
        let layout = self.layout;
        self.visit_resident_intersections(region, |tile, clip| {
            if tile.visible_layout != layout || !matches!(tile.damage, TileDamage::Clean) {
                tile.damage.add(clip);
                return;
            }
            let binding = tile
                .resident
                .as_ref()
                .expect("A resident tile has a binding");
            let [x, y] = clip.origin;
            let offset = (y - region.origin[1]) as usize * bytes_per_row as usize
                + (x - region.origin[0]) as usize * bytes_per_pixel as usize;
            write_region(
                queue,
                &binding.texture,
                clip.relative_to(tile.region.storage.origin),
                &bytes[offset..],
                bytes_per_row,
            );
        });
    }
}

fn damage_regions(bounds: [GraphicsPixelBounds; 2]) -> impl Iterator<Item = PixelRegion> {
    bounds
        .into_iter()
        .filter_map(|GraphicsPixelBounds { origin, size }| {
            (!size.contains(&0)).then_some(PixelRegion { origin, size })
        })
}

impl ImageTextures {
    pub fn get(&self, index: usize) -> Option<&TextureBinding> {
        match self {
            Self::Single(tile) => Some(tile),
            Self::Tiled(tiles) => tiles.tiles[index].resident.as_ref(),
        }
    }

    pub fn write(
        &mut self,
        queue: &Queue,
        region: PixelRegion,
        bytes: &[u8],
        bytes_per_row: u32,
        bytes_per_pixel: u32,
    ) {
        match self {
            Self::Single(tile) => write_region(queue, &tile.texture, region, bytes, bytes_per_row),
            Self::Tiled(tiles) => tiles.write(queue, region, bytes, bytes_per_row, bytes_per_pixel),
        }
    }

    pub fn rasterize(
        &mut self,
        queue: &Queue,
        image: &GraphicsImage,
        scratch: &mut Vec<u8>,
    ) -> bool {
        let tiles = match self {
            Self::Single(binding) => {
                let Some(bytes) = image.pixels(scratch) else {
                    return false;
                };
                write_texture(
                    queue,
                    &binding.texture,
                    bytes,
                    image.dimensions().0 * image.format().bytes_per_pixel(),
                );
                return true;
            }
            Self::Tiled(tiles) => tiles,
        };
        let largest = tiles
            .tiles
            .iter()
            .filter(|tile| tile.visible_layout == tiles.layout && tile.resident.is_some())
            .map(|tile| tile.region.storage.pixel_count() as usize)
            .max()
            .unwrap_or(0)
            * image.format().bytes_per_pixel() as usize;
        // Reserve before queuing writes to preserve the resident revision on allocation failure.
        if scratch
            .try_reserve_exact(largest.saturating_sub(scratch.len()))
            .is_err()
        {
            return false;
        }
        for tile in &mut tiles.tiles {
            let Some(binding) = &tile.resident else {
                continue;
            };
            if tile.visible_layout != tiles.layout {
                tile.damage = TileDamage::Full;
                continue;
            }
            let region = tile.region.storage;
            let Some(update) = image.pixel_region(region.origin, region.size, scratch) else {
                return false;
            };
            write_texture(queue, &binding.texture, update.bytes, update.bytes_per_row);
            tile.damage = TileDamage::Clean;
        }
        true
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

pub(super) fn write_region(
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
