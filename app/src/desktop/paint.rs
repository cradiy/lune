use std::sync::Arc;
use std::time::{Duration, Instant};

use lune_terminal::{CellPosition, CursorAppearance, PaletteEntry};
use winit::dpi::PhysicalSize;
use winit::window::Window;

use crate::gpu::linear_colors;
use crate::{
    CellBackgroundRenderer, ImageRenderer, PresentationFrame, PresentationTracker, PresentedCursor,
    RowDamage,
};

use super::Result;
use super::font::{Font, FontMetrics};
use super::text::TextRenderer;
use super::vi;

const RETRY_DELAY: Duration = Duration::from_millis(16);

pub(super) struct Painter {
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    device: wgpu::Device,
    queue: wgpu::Queue,
    backgrounds: CellBackgroundRenderer,
    images: ImageRenderer,
    text: TextRenderer,
    tracker: PresentationTracker,
    retry: Option<Instant>,
    force_present: bool,
    dirty: bool,
    graphics_prepare_failed: bool,
}

impl Painter {
    pub(super) async fn new(window: Arc<Window>, mut font: Font, font_size: f32) -> Result<Self> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(descriptor);
        let surface = instance.create_surface(window.clone())?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                ..Default::default()
            })
            .await?;
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| std::io::Error::other("No compatible Wayland surface format"))?;
        config.format = surface
            .get_capabilities(&adapter)
            .formats
            .into_iter()
            .find(|format| format.is_srgb())
            .ok_or_else(|| std::io::Error::other("The Wayland surface has no sRGB format"))?;
        surface.configure(&device, &config);
        let backgrounds = CellBackgroundRenderer::new(&device, config.format);
        let images = ImageRenderer::new(&device, config.format);
        let metrics = font.measure(font_size);
        let text = TextRenderer::new(&device, &queue, config.format, font, metrics);
        log::debug!(
            "Vulkan renderer ready with surface format {:?}",
            config.format
        );
        let mut tracker = PresentationTracker::default();
        tracker.set_cursor_blink_interval(Duration::from_millis(530), Instant::now());
        Ok(Self {
            instance,
            surface,
            config,
            device,
            queue,
            backgrounds,
            images,
            text,
            tracker,
            retry: None,
            force_present: false,
            dirty: true,
            graphics_prepare_failed: false,
        })
    }

    pub(super) fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == self.config.width && size.height == self.config.height {
            return;
        }
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.dirty = true;
    }

    pub(super) fn metrics(&self) -> FontMetrics {
        self.text.metrics()
    }

    pub(super) fn measure(&mut self, size: f32) -> FontMetrics {
        self.text.measure(size)
    }

    pub(super) fn set_metrics(&mut self, metrics: FontMetrics) {
        self.text.set_metrics(metrics);
        self.dirty = true;
    }

    pub(super) fn invalidate_overlay(&mut self) {
        self.dirty = true;
    }

    pub(super) fn restart_blink(&mut self, now: Instant) -> bool {
        let changed = self.tracker.restart_cursor_blink(now).is_some();
        self.dirty |= changed;
        changed
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.retry
            .into_iter()
            .chain(self.tracker.cursor_deadline())
            .min()
    }

    pub(super) fn tick(&mut self, now: Instant) -> bool {
        let retry = self.retry.is_some_and(|retry| retry <= now);
        if retry {
            self.retry = None;
            self.force_present = true;
        }
        let cursor_changed = self.tracker.tick_cursor(now).is_some();
        self.dirty |= cursor_changed;
        retry || cursor_changed
    }

    pub(super) fn draw(
        &mut self,
        window: &Arc<Window>,
        frame: &PresentationFrame<'_>,
        selection: Option<&std::ops::Range<CellPosition>>,
        preedit: &str,
        vi: Option<&vi::Mode>,
    ) -> Result<()> {
        let area = frame.geometry.text_area_pixels().unwrap();
        let (width, height) = (area.width.get(), area.height.get());
        if width > self.config.width || height > self.config.height {
            return Ok(());
        }

        let update = self.tracker.prepare(frame, Instant::now())?;
        let damage = update.damage();
        let rows_changed = match damage.rows {
            RowDamage::All => true,
            RowDamage::Rows(rows) => !rows.is_empty(),
        };
        let text_changed = rows_changed || self.dirty || damage.cursor.is_some();
        if !text_changed && !damage.graphics_changed && !self.force_present {
            update.commit();
            return Ok(());
        }

        use wgpu::CurrentSurfaceTexture as Acquired;
        let (texture, suboptimal) = match self.surface.get_current_texture() {
            Acquired::Success(texture) => (texture, false),
            Acquired::Suboptimal(texture) => (texture, true),
            Acquired::Timeout => {
                self.retry = Some(Instant::now() + RETRY_DELAY);
                return Ok(());
            }
            Acquired::Occluded => return Ok(()),
            Acquired::Outdated => {
                self.surface.configure(&self.device, &self.config);
                self.retry = Some(Instant::now() + RETRY_DELAY);
                return Ok(());
            }
            Acquired::Lost => {
                log::debug!("Recreating the lost Wayland surface");
                self.surface = self.instance.create_surface(window.clone())?;
                self.surface.configure(&self.device, &self.config);
                self.retry = Some(Instant::now() + RETRY_DELAY);
                return Ok(());
            }
            Acquired::Validation => {
                return Err(std::io::Error::other("Invalid Wayland surface").into());
            }
        };

        if text_changed {
            let cursor = if let Some(vi) = vi {
                let rows = frame.viewport.range();
                rows.contains(&vi.cursor.row).then(|| PresentedCursor {
                    position: CellPosition {
                        row: vi.cursor.row - rows.start,
                        column: vi.cursor.column,
                    },
                    appearance: CursorAppearance {
                        blinking: false,
                        ..CursorAppearance::default()
                    },
                    color: frame.palette.color(PaletteEntry::Cursor),
                })
            } else {
                damage.displayed_cursor
            };
            self.text.prepare(frame, cursor, selection, preedit, vi)?;
        }
        self.backgrounds
            .prepare(&self.device, &self.queue, frame)
            .map_err(|error| {
                std::io::Error::other(format!("Failed to prepare terminal backgrounds: {error:?}"))
            })?;
        let failures = self.images.prepare(&self.device, &self.queue, frame);

        let view = texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let background = frame.palette.color(if frame.reverse_video {
            PaletteEntry::Foreground
        } else {
            PaletteEntry::Background
        });
        let lut = linear_colors();
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: lut[background.red as usize].into(),
                            g: lut[background.green as usize].into(),
                            b: lut[background.blue as usize].into(),
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_viewport(0.0, 0.0, width as f32, height as f32, 0.0, 1.0);
            pass.set_scissor_rect(0, 0, width, height);
            self.images
                .draw_frame(&mut pass, &self.backgrounds, |pass| self.text.draw(pass));
        }
        self.queue.submit([encoder.finish()]);
        window.pre_present_notify();
        self.queue.present(texture);
        update.commit();
        self.force_present = false;
        self.dirty = false;
        if suboptimal {
            self.surface.configure(&self.device, &self.config);
        }
        if failures != 0 {
            if !self.graphics_prepare_failed {
                log::warn!("Failed to prepare {failures} terminal graphics placements");
            }
            self.retry = Some(Instant::now() + Duration::from_millis(100));
        }
        self.graphics_prepare_failed = failures != 0;
        Ok(())
    }
}
