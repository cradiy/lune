//! GPU background image loader and renderer supporting PNG, JPEG, and WebP.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use image::ImageReader;
use image::imageops::FilterType;
use lune_terminal::Rgb;
use wgpu::{
    AddressMode, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, Buffer,
    BufferBindingType, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites, Device,
    Extent3d, FilterMode, FragmentState, MipmapFilterMode, MultisampleState, Origin3d,
    PipelineCompilationOptions, PipelineLayoutDescriptor, PrimitiveState, PrimitiveTopology, Queue,
    RenderPass, RenderPipeline, RenderPipelineDescriptor, Sampler, SamplerBindingType,
    SamplerDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages, TexelCopyBufferLayout,
    TexelCopyTextureInfo, Texture, TextureAspect, TextureDescriptor, TextureDimension,
    TextureFormat, TextureSampleType, TextureUsages, TextureViewDescriptor, TextureViewDimension,
    VertexState,
};

pub(super) const DEFAULT_BACKGROUND_IMAGE_OPACITY: f32 = 0.25;
const MAX_IMAGE_DIMENSION: u32 = 8192;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum BackgroundImageFit {
    #[default]
    Cover,
    Contain,
    Stretch,
    Tile,
    None,
}

impl BackgroundImageFit {
    pub(super) fn parse(value: &str) -> Result<Self, &'static str> {
        if value.eq_ignore_ascii_case("cover") {
            Ok(Self::Cover)
        } else if value.eq_ignore_ascii_case("contain") {
            Ok(Self::Contain)
        } else if value.eq_ignore_ascii_case("stretch") {
            Ok(Self::Stretch)
        } else if value.eq_ignore_ascii_case("tile") || value.eq_ignore_ascii_case("repeat") {
            Ok(Self::Tile)
        } else if value.eq_ignore_ascii_case("none") {
            Ok(Self::None)
        } else {
            Err("background-image-fit must be cover, contain, stretch, tile, or none")
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum BackgroundImagePosition {
    #[default]
    Center,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl BackgroundImagePosition {
    pub(super) fn parse(value: &str) -> Result<Self, &'static str> {
        let normalized = value.to_ascii_lowercase().replace('_', "-");
        match normalized.as_str() {
            "center" => Ok(Self::Center),
            "top" | "top-center" => Ok(Self::Top),
            "bottom" | "bottom-center" => Ok(Self::Bottom),
            "left" | "center-left" => Ok(Self::Left),
            "right" | "center-right" => Ok(Self::Right),
            "top-left" | "left-top" => Ok(Self::TopLeft),
            "top-right" | "right-top" => Ok(Self::TopRight),
            "bottom-left" | "left-bottom" => Ok(Self::BottomLeft),
            "bottom-right" | "right-bottom" => Ok(Self::BottomRight),
            _ => Err(
                "background-image-position must be center, top, bottom, left, right, top-left, top-right, bottom-left, or bottom-right",
            ),
        }
    }

    const fn factors(self) -> (f32, f32) {
        match self {
            Self::Center => (0.5, 0.5),
            Self::Top => (0.5, 0.0),
            Self::Bottom => (0.5, 1.0),
            Self::Left => (0.0, 0.5),
            Self::Right => (1.0, 0.5),
            Self::TopLeft => (0.0, 0.0),
            Self::TopRight => (1.0, 0.0),
            Self::BottomLeft => (0.0, 1.0),
            Self::BottomRight => (1.0, 1.0),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct BackgroundImageConfig {
    pub(super) path: Option<PathBuf>,
    pub(super) opacity: f32,
    pub(super) fit: BackgroundImageFit,
    pub(super) position: BackgroundImagePosition,
}

impl Default for BackgroundImageConfig {
    fn default() -> Self {
        Self {
            path: None,
            opacity: DEFAULT_BACKGROUND_IMAGE_OPACITY,
            fit: BackgroundImageFit::Cover,
            position: BackgroundImagePosition::Center,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    rect: [f32; 4],
    uv: [f32; 4],
    bg_srgb: [f32; 4],
    params: [f32; 4],
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl FileStamp {
    fn read(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        Some(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        })
    }
}

struct LoadedImage {
    path: PathBuf,
    stamp: Option<FileStamp>,
    _texture: Texture,
    size: (u32, u32),
    clamp_bind_group: BindGroup,
    repeat_bind_group: BindGroup,
}

pub(super) struct BackgroundImageRenderer {
    pipeline: RenderPipeline,
    bind_group_layout: BindGroupLayout,
    clamp_sampler: Sampler,
    repeat_sampler: Sampler,
    uniform_buffer: Buffer,
    config: BackgroundImageConfig,
    loaded: Option<LoadedImage>,
}

impl BackgroundImageRenderer {
    pub(super) fn new(device: &Device, format: TextureFormat) -> Self {
        let module = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("Background image"),
            source: ShaderSource::Wgsl(include_str!("bg_image.wgsl").into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("Background image bind group layout"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        sample_type: TextureSampleType::Float { filterable: true },
                        view_dimension: TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Filtering),
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 2,
                    visibility: ShaderStages::VERTEX_FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("Background image pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("Background image pipeline"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &module,
                entry_point: Some("vertex"),
                compilation_options: PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: PrimitiveState {
                topology: PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: MultisampleState::default(),
            fragment: Some(FragmentState {
                module: &module,
                entry_point: Some("fragment"),
                compilation_options: PipelineCompilationOptions::default(),
                targets: &[Some(ColorTargetState {
                    format,
                    blend: None,
                    write_mask: ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let clamp_sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("Background image clamp sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            address_mode_w: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            mipmap_filter: MipmapFilterMode::Linear,
            ..Default::default()
        });
        let repeat_sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("Background image repeat sampler"),
            address_mode_u: AddressMode::Repeat,
            address_mode_v: AddressMode::Repeat,
            address_mode_w: AddressMode::Repeat,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            mipmap_filter: MipmapFilterMode::Linear,
            ..Default::default()
        });
        let uniform_buffer = device.create_buffer(&BufferDescriptor {
            label: Some("Background image uniforms"),
            size: size_of::<Uniforms>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            bind_group_layout,
            clamp_sampler,
            repeat_sampler,
            uniform_buffer,
            config: BackgroundImageConfig::default(),
            loaded: None,
        }
    }

    pub(super) fn configure(
        &mut self,
        device: &Device,
        queue: &Queue,
        config: BackgroundImageConfig,
        force_reload: bool,
    ) -> Result<bool, String> {
        let Some(path) = config.path.clone() else {
            let changed = self.loaded.take().is_some() || self.config != config;
            self.config = config;
            return Ok(changed);
        };

        let stamp = FileStamp::read(&path);
        let needs_load = match &self.loaded {
            Some(loaded) => {
                loaded.path != path || (force_reload && (stamp.is_none() || loaded.stamp != stamp))
            }
            None => true,
        };
        let style_changed = self.config != config;
        self.config = config;

        if !needs_load {
            return Ok(style_changed);
        }

        let max_dim = device
            .limits()
            .max_texture_dimension_2d
            .min(MAX_IMAGE_DIMENSION);
        match decode_image(&path, max_dim) {
            Ok((width, height, pixels)) => {
                let texture = device.create_texture(&TextureDescriptor {
                    label: Some("Background image texture"),
                    size: Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: TextureDimension::D2,
                    format: TextureFormat::Rgba8UnormSrgb,
                    usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                queue.write_texture(
                    TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: 0,
                        origin: Origin3d::ZERO,
                        aspect: TextureAspect::All,
                    },
                    &pixels,
                    TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(width * 4),
                        rows_per_image: Some(height),
                    },
                    Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                );
                let view = texture.create_view(&TextureViewDescriptor::default());
                let clamp_bind_group = self.make_bind_group(device, &view, &self.clamp_sampler);
                let repeat_bind_group = self.make_bind_group(device, &view, &self.repeat_sampler);
                self.loaded = Some(LoadedImage {
                    path,
                    stamp,
                    _texture: texture,
                    size: (width, height),
                    clamp_bind_group,
                    repeat_bind_group,
                });
                Ok(true)
            }
            Err(error) => {
                self.loaded = None;
                Err(error)
            }
        }
    }

    fn make_bind_group(
        &self,
        device: &Device,
        view: &wgpu::TextureView,
        sampler: &Sampler,
    ) -> BindGroup {
        device.create_bind_group(&BindGroupDescriptor {
            label: None,
            layout: &self.bind_group_layout,
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: BindingResource::TextureView(view),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::Sampler(sampler),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: self.uniform_buffer.as_entire_binding(),
                },
            ],
        })
    }

    pub(super) fn draw(
        &self,
        pass: &mut RenderPass<'_>,
        queue: &Queue,
        viewport: (u32, u32),
        background: Rgb,
        clear_scale: f32,
        clear_alpha: f32,
    ) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        let opacity = self.config.opacity.clamp(0.0, 1.0);
        if opacity <= 0.0 || viewport.0 == 0 || viewport.1 == 0 {
            return;
        }
        let (rect, uv) = compute_quad(viewport, loaded.size, self.config.fit, self.config.position);
        let uniforms = Uniforms {
            rect,
            uv,
            bg_srgb: [
                f32::from(background.red) / 255.0,
                f32::from(background.green) / 255.0,
                f32::from(background.blue) / 255.0,
                1.0,
            ],
            params: [opacity, clear_scale, clear_alpha, 0.0],
        };
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));
        let bind_group = if self.config.fit == BackgroundImageFit::Tile {
            &loaded.repeat_bind_group
        } else {
            &loaded.clamp_bind_group
        };
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..4, 0..1);
    }
}

fn decode_image(path: &Path, max_dim: u32) -> Result<(u32, u32, Vec<u8>), String> {
    let bytes = fs::read(path).map_err(|error| format!("Background image read failed: {error}"))?;
    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|error| format!("Background image format error: {error}"))?;
    let decoded = reader
        .decode()
        .map_err(|error| format!("Background image decode failed: {error}"))?;
    let mut rgba = decoded.into_rgba8();
    let (mut width, mut height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return Err("Background image has zero dimensions".into());
    }
    if width > max_dim || height > max_dim {
        let scale = (max_dim as f64 / f64::from(width)).min(max_dim as f64 / f64::from(height));
        width = ((f64::from(width) * scale).round() as u32).clamp(1, max_dim);
        height = ((f64::from(height) * scale).round() as u32).clamp(1, max_dim);
        rgba = image::imageops::resize(&rgba, width, height, FilterType::Triangle);
    }
    Ok((width, height, rgba.into_raw()))
}

fn compute_quad(
    viewport: (u32, u32),
    image: (u32, u32),
    fit: BackgroundImageFit,
    position: BackgroundImagePosition,
) -> ([f32; 4], [f32; 4]) {
    let vw = viewport.0 as f32;
    let vh = viewport.1 as f32;
    let iw = image.0 as f32;
    let ih = image.1 as f32;
    let (ax, ay) = position.factors();

    let ndc_x = |x: f32| x / vw * 2.0 - 1.0;
    let ndc_y = |y: f32| 1.0 - y / vh * 2.0;

    match fit {
        BackgroundImageFit::Stretch => ([-1.0, 1.0, 1.0, -1.0], [0.0, 0.0, 1.0, 1.0]),
        BackgroundImageFit::Cover => {
            let scale = (vw / iw).max(vh / ih);
            let dw = iw * scale;
            let dh = ih * scale;
            let u_span = (vw / dw).min(1.0);
            let v_span = (vh / dh).min(1.0);
            let u0 = (1.0 - u_span) * ax;
            let v0 = (1.0 - v_span) * ay;
            ([-1.0, 1.0, 1.0, -1.0], [u0, v0, u0 + u_span, v0 + v_span])
        }
        BackgroundImageFit::Contain => {
            let scale = (vw / iw).min(vh / ih);
            let dw = iw * scale;
            let dh = ih * scale;
            let x0 = (vw - dw) * ax;
            let y0 = (vh - dh) * ay;
            (
                [ndc_x(x0), ndc_y(y0), ndc_x(x0 + dw), ndc_y(y0 + dh)],
                [0.0, 0.0, 1.0, 1.0],
            )
        }
        BackgroundImageFit::None => {
            let (rx0, rx1, u0, u1) = if iw <= vw {
                let x0 = (vw - iw) * ax;
                (ndc_x(x0), ndc_x(x0 + iw), 0.0, 1.0)
            } else {
                let span = vw / iw;
                let u0 = (1.0 - span) * ax;
                (-1.0, 1.0, u0, u0 + span)
            };
            let (ry0, ry1, v0, v1) = if ih <= vh {
                let y0 = (vh - ih) * ay;
                (ndc_y(y0), ndc_y(y0 + ih), 0.0, 1.0)
            } else {
                let span = vh / ih;
                let v0 = (1.0 - span) * ay;
                (1.0, -1.0, v0, v0 + span)
            };
            ([rx0, ry0, rx1, ry1], [u0, v0, u1, v1])
        }
        BackgroundImageFit::Tile => {
            let u_span = vw / iw;
            let v_span = vh / ih;
            let u0 = -((vw - iw) * ax) / iw;
            let v0 = -((vh - ih) * ay) / ih;
            ([-1.0, 1.0, 1.0, -1.0], [u0, v0, u0 + u_span, v0 + v_span])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BackgroundImageFit, BackgroundImagePosition, compute_quad};

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn cover_and_contain_preserve_aspect_ratio_and_alignment() {
        // 200x100 landscape image in a 100x100 square viewport.
        let (rect, uv) = compute_quad(
            (100, 100),
            (200, 100),
            BackgroundImageFit::Cover,
            BackgroundImagePosition::Center,
        );
        assert_eq!(rect, [-1.0, 1.0, 1.0, -1.0]);
        assert!(approx(uv[0], 0.25) && approx(uv[2], 0.75));
        assert!(approx(uv[1], 0.0) && approx(uv[3], 1.0));

        let (_, uv_left) = compute_quad(
            (100, 100),
            (200, 100),
            BackgroundImageFit::Cover,
            BackgroundImagePosition::Left,
        );
        assert!(approx(uv_left[0], 0.0) && approx(uv_left[2], 0.5));

        let (rect_c, uv_c) = compute_quad(
            (100, 100),
            (200, 100),
            BackgroundImageFit::Contain,
            BackgroundImagePosition::Top,
        );
        assert_eq!(uv_c, [0.0, 0.0, 1.0, 1.0]);
        // Top-aligned 100x50 strip occupies NDC y from +1.0 down to 0.0.
        assert!(approx(rect_c[0], -1.0) && approx(rect_c[2], 1.0));
        assert!(approx(rect_c[1], 1.0) && approx(rect_c[3], 0.0));
    }

    #[test]
    fn tile_and_none_modes_compute_exact_native_uvs() {
        let (rect_t, uv_t) = compute_quad(
            (300, 200),
            (100, 100),
            BackgroundImageFit::Tile,
            BackgroundImagePosition::TopLeft,
        );
        assert_eq!(rect_t, [-1.0, 1.0, 1.0, -1.0]);
        assert!(approx(uv_t[0], 0.0) && approx(uv_t[2], 3.0));
        assert!(approx(uv_t[1], 0.0) && approx(uv_t[3], 2.0));

        let (rect_n, uv_n) = compute_quad(
            (200, 100),
            (100, 200),
            BackgroundImageFit::None,
            BackgroundImagePosition::Center,
        );
        // Width fits (100px centered in 200px => NDC -0.5..0.5), height crops (0.25..0.75).
        assert!(approx(rect_n[0], -0.5) && approx(rect_n[2], 0.5));
        assert!(approx(rect_n[1], 1.0) && approx(rect_n[3], -1.0));
        assert!(approx(uv_n[0], 0.0) && approx(uv_n[2], 1.0));
        assert!(approx(uv_n[1], 0.25) && approx(uv_n[3], 0.75));
    }

    #[test]
    fn decodes_png_jpeg_and_webp_in_memory() {
        use image::{ImageFormat, Rgba, RgbaImage};
        use std::io::Cursor;

        let mut img = RgbaImage::new(4, 3);
        img.put_pixel(0, 0, Rgba([200, 100, 50, 255]));
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
            let mut buf = Vec::new();
            let dyn_img = if format == ImageFormat::Jpeg {
                image::DynamicImage::ImageRgb8(
                    image::DynamicImage::ImageRgba8(img.clone()).to_rgb8(),
                )
            } else {
                image::DynamicImage::ImageRgba8(img.clone())
            };
            dyn_img
                .write_to(&mut Cursor::new(&mut buf), format)
                .unwrap();
            let decoded = image::ImageReader::new(Cursor::new(&buf))
                .with_guessed_format()
                .unwrap()
                .decode()
                .unwrap()
                .into_rgba8();
            assert_eq!(decoded.dimensions(), (4, 3));
        }
    }
}
