use std::num::NonZeroU32;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lune_terminal::{
    CellPosition, CursorAppearance, HyperlinkId, PaletteEntry, PixelPosition, Progress,
    ScreenBuffer,
};
use winit::dpi::PhysicalSize;
use winit::window::Window;

use crate::gpu::linear_colors;
use crate::{
    CellBackgroundRenderer, ImageRenderer, PresentationFrame, PresentationTracker, PresentedCursor,
    RowDamage,
};

use super::Result;
use super::cursor::CursorMotion;
use super::font::{Font, FontMetrics};
use super::tab_bar;
use super::text::TextRenderer;
use super::vi;

const RETRY_DELAY: Duration = Duration::from_millis(16);

pub(super) struct Overlay<'a> {
    pub(super) selection: Option<&'a Range<CellPosition>>,
    pub(super) hovered_hyperlink: Option<HyperlinkId>,
    pub(super) progress: Progress,
    pub(super) progress_phase: f32,
    pub(super) preedit: &'a str,
    pub(super) vi: Option<&'a vi::Mode>,
    pub(super) focused: bool,
    pub(super) tab_bar: tab_bar::View<'a>,
}

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
    cursor_motion: CursorMotion,
    cursor_buffer: ScreenBuffer,
    progress_animation: Option<Instant>,
    retry: Option<Instant>,
    force_present: bool,
    dirty: bool,
    graphics_prepare_failed: bool,
}

impl Painter {
    pub(super) async fn new(
        window: Arc<Window>,
        mut font: Font,
        physical_font_size: f32,
    ) -> Result<Self> {
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
        let metrics = font.measure(physical_font_size);
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
            cursor_motion: CursorMotion::default(),
            cursor_buffer: ScreenBuffer::Primary,
            progress_animation: None,
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
        self.reset_cursor_motion();
        self.dirty = true;
    }

    pub(super) fn metrics(&self) -> FontMetrics {
        self.text.metrics()
    }

    pub(super) fn size(&self) -> PhysicalSize<u32> {
        PhysicalSize::new(self.config.width, self.config.height)
    }

    pub(super) fn measure(&mut self, size: f32) -> FontMetrics {
        self.text.measure(size)
    }

    pub(super) fn set_metrics(&mut self, metrics: FontMetrics) {
        if self.text.metrics() != metrics {
            self.reset_cursor_motion();
        }
        self.text.set_metrics(metrics);
        self.dirty = true;
    }

    pub(super) fn invalidate_overlay(&mut self) {
        self.dirty = true;
    }

    pub(super) fn switch_session(&mut self) {
        self.tracker.invalidate();
        self.images.reset_session();
        self.reset_cursor_motion();
        self.retry = None;
        self.graphics_prepare_failed = false;
        self.dirty = true;
    }

    pub(super) fn reset_cursor_motion(&mut self) -> bool {
        let moving = self.cursor_motion.moving();
        self.cursor_motion = CursorMotion::default();
        self.dirty |= moving;
        moving
    }

    pub(super) fn restart_blink(&mut self, now: Instant) -> bool {
        let changed = self.tracker.restart_cursor_blink(now).is_some();
        self.dirty |= changed;
        changed
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        let progress_deadline = self
            .progress_animation
            .map(|_| Instant::now() + RETRY_DELAY);
        self.retry
            .into_iter()
            .chain(self.tracker.cursor_deadline())
            .chain(progress_deadline)
            .min()
    }

    pub(super) fn tick(&mut self, now: Instant) -> bool {
        let retry = self.retry.is_some_and(|retry| retry <= now);
        if retry {
            self.retry = None;
            self.force_present = true;
        }
        let cursor_changed = self.tracker.tick_cursor(now).is_some();
        let progress_changed = self.progress_animation.is_some();
        self.dirty |= cursor_changed || progress_changed;
        retry || cursor_changed || progress_changed
    }

    pub(super) fn draw(
        &mut self,
        window: &Arc<Window>,
        frame: &PresentationFrame<'_>,
        mut overlay: Overlay<'_>,
    ) -> Result<()> {
        let now = Instant::now();
        if overlay.progress == Progress::Indeterminate {
            let start = *self.progress_animation.get_or_insert(now);
            overlay.progress_phase = (now.duration_since(start).as_secs_f32() * 0.75).fract();
        } else {
            self.progress_animation = None;
            overlay.progress_phase = 0.0;
        }
        let Overlay {
            preedit,
            vi,
            focused,
            ..
        } = overlay;
        let bar_height = overlay.tab_bar.layout.height;
        let area = frame.geometry.text_area_pixels().unwrap();
        let (width, height) = (area.width.get(), area.height.get());
        if width > self.config.width || height > self.config.height.saturating_sub(bar_height) {
            return Ok(());
        }

        let update = self.tracker.prepare(frame, now)?;
        let damage = update.damage();
        let rows_changed = match damage.rows {
            RowDamage::All => true,
            RowDamage::Rows(rows) => !rows.is_empty(),
        };
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
        let buffer = frame.graphics_buffer();
        let mut motion = if self.cursor_buffer == buffer {
            self.cursor_motion
        } else {
            CursorMotion::default()
        };
        let target =
            if focused && vi.is_none() && preedit.is_empty() && frame.viewport.offset() == 0 {
                cursor.and_then(|cursor| cursor_origin(frame, cursor.position))
            } else {
                None
            };
        let position = motion.advance(target, now);
        let moving = position.is_some();
        let text_changed = rows_changed
            || self.dirty
            || moving != self.cursor_motion.moving()
            || (!moving && damage.cursor.is_some());
        let prepare_layers = text_changed || damage.graphics_changed || self.force_present;
        if !prepare_layers && !moving {
            update.commit();
            self.cursor_motion = motion;
            self.cursor_buffer = buffer;
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
            self.text
                .prepare(frame, cursor.filter(|_| !moving), &overlay)?;
        }
        let mut draw_moving_cursor = false;
        if let Some(position) = position
            && let Some(target) = target
            && let Some(cursor) = cursor
            && let Some(paint) = frame.cursor_paint(cursor, NonZeroU32::new(2).unwrap())?
        {
            let mut rect = paint.rect;
            rect.origin.x = position.x + (rect.origin.x - target.x);
            rect.origin.y = position.y + (rect.origin.y - target.y);
            self.text.prepare_moving_cursor(rect, paint.color);
            draw_moving_cursor = true;
        }
        let failures = if prepare_layers {
            self.backgrounds
                .prepare(&self.device, &self.queue, frame)
                .map_err(|error| {
                    std::io::Error::other(format!(
                        "Failed to prepare terminal backgrounds: {error:?}"
                    ))
                })?;
            Some(self.images.prepare(&self.device, &self.queue, frame))
        } else {
            None
        };

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
                .draw_frame(&mut pass, &self.backgrounds, |pass| {
                    self.text.draw(pass, draw_moving_cursor)
                });
            if bar_height != 0 {
                let top = overlay.tab_bar.layout.top;
                pass.set_viewport(
                    0.0,
                    top as f32,
                    self.config.width as f32,
                    bar_height as f32,
                    0.0,
                    1.0,
                );
                pass.set_scissor_rect(0, top, self.config.width, bar_height);
                self.text.draw_tab_bar(&mut pass);
            }
        }
        self.queue.submit([encoder.finish()]);
        window.pre_present_notify();
        self.queue.present(texture);
        update.commit();
        self.cursor_motion = motion;
        self.cursor_buffer = buffer;
        self.force_present = false;
        self.dirty = false;
        if suboptimal {
            self.surface.configure(&self.device, &self.config);
        }
        if let Some(failures) = failures {
            if failures != 0 {
                if !self.graphics_prepare_failed {
                    log::warn!("Failed to prepare {failures} terminal graphics placements");
                }
                self.retry = Some(Instant::now() + Duration::from_millis(100));
            }
            self.graphics_prepare_failed = failures != 0;
        }
        if moving || self.progress_animation.is_some() {
            window.request_redraw();
        }
        Ok(())
    }
}

fn cursor_origin(frame: &PresentationFrame<'_>, position: CellPosition) -> Option<PixelPosition> {
    let row = frame.viewport.row(position.row)?;
    if row.sized_text_at(position.column).is_some() {
        return None;
    }
    let cell = row.cells().get(position.column)?;
    let leading = position.column - usize::from(cell.is_continuation());
    frame
        .geometry
        .row_rect(position.row, leading..leading + 1)
        .map(|rect| rect.origin)
}
