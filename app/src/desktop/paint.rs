use std::collections::HashMap;
use std::num::NonZeroU32;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lune_terminal::{
    CellPosition, CursorAppearance, Palette, PaletteEntry, PixelPosition, PixelSize, Progress,
    ScreenBuffer,
};
use winit::dpi::PhysicalSize;
use winit::window::Window;

use crate::gpu::linear_colors;
use crate::{
    CellBackgroundRenderer, ImageRenderer, PixelRect, PresentationFrame, PresentationTracker,
    PresentedCursor, RowDamage,
};

use super::cursor::CursorMotion;
use super::font::{Font, FontMetrics};
use super::split::Divider;
use super::text::TextRenderer;
use super::url::HoveredLink;
use super::{PaneId, Result, tab_bar, vi};

const RETRY_DELAY: Duration = Duration::from_millis(16);
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(530);

pub(super) struct PaneOverlay<'a> {
    pub(super) selection: Option<&'a Range<CellPosition>>,
    pub(super) hovered_link: Option<&'a HoveredLink>,
    pub(super) progress: Progress,
    pub(super) progress_phase: f32,
    pub(super) preedit: &'a str,
    pub(super) vi: Option<&'a vi::Mode>,
    pub(super) zoomed: bool,
}

pub(super) struct PaneDraw<'a> {
    pub(super) id: PaneId,
    pub(super) rect: PixelRect,
    pub(super) frame: PresentationFrame<'a>,
    pub(super) selection: Option<&'a Range<CellPosition>>,
    pub(super) hovered_link: Option<&'a HoveredLink>,
    pub(super) progress: Progress,
    pub(super) preedit: &'a str,
    pub(super) vi: Option<&'a vi::Mode>,
    pub(super) focused: bool,
    pub(super) zoomed: bool,
}

pub(super) struct WindowChrome<'a> {
    pub(super) palette: &'a Palette,
    pub(super) reverse_video: bool,
    pub(super) dividers: &'a [Divider],
    pub(super) focused_rect: Option<PixelRect>,
    pub(super) notice: Option<&'a str>,
    pub(super) tab_bar: tab_bar::View<'a>,
}

struct PaneGpu {
    backgrounds: CellBackgroundRenderer,
    images: ImageRenderer,
    graphics_prepare_failed: bool,
}

pub(super) struct Painter {
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    alpha_modes: Vec<wgpu::CompositeAlphaMode>,
    background_opacity: f32,
    device: wgpu::Device,
    queue: wgpu::Queue,
    gpus: HashMap<PaneId, PaneGpu>,
    trackers: Vec<(PaneId, PresentationTracker)>,
    text: TextRenderer,
    pane_ranges: Vec<(PaneId, Range<u32>)>,
    chrome_range: Range<u32>,
    tab_bar_range: Range<u32>,
    cursor_pane: Option<PaneId>,
    cursor_motion: CursorMotion,
    cursor_buffer: ScreenBuffer,
    progress_animation: Option<Instant>,
    retry: Option<Instant>,
    force_present: bool,
    dirty: bool,
}

impl Painter {
    pub(super) async fn new(
        window: Arc<Window>,
        mut font: Font,
        physical_font_size: f32,
        background_opacity: f32,
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
        let capabilities = surface.get_capabilities(&adapter);
        config.format = capabilities
            .formats
            .into_iter()
            .find(|format| format.is_srgb())
            .ok_or_else(|| std::io::Error::other("The Wayland surface has no sRGB format"))?;
        let background_opacity = background_opacity.clamp(0.0, 1.0);
        let alpha_modes = capabilities.alpha_modes;
        config.alpha_mode = choose_alpha_mode(&alpha_modes, background_opacity);
        if background_opacity < 1.0 && config.alpha_mode == wgpu::CompositeAlphaMode::Opaque {
            log::warn!(
                "Wayland surface does not support alpha compositing; rendering opaque background"
            );
        }
        surface.configure(&device, &config);
        let metrics = font.measure(physical_font_size);
        let text = TextRenderer::new(&device, &queue, config.format, font, metrics);
        log::debug!(
            "Vulkan renderer ready with surface format {:?} and alpha mode {:?}",
            config.format,
            config.alpha_mode
        );
        Ok(Self {
            instance,
            surface,
            config,
            alpha_modes,
            background_opacity,
            device,
            queue,
            gpus: HashMap::new(),
            trackers: Vec::new(),
            text,
            pane_ranges: Vec::new(),
            chrome_range: 0..0,
            tab_bar_range: 0..0,
            cursor_pane: None,
            cursor_motion: CursorMotion::default(),
            cursor_buffer: ScreenBuffer::Primary,
            progress_animation: None,
            retry: None,
            force_present: false,
            dirty: true,
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

    pub(super) fn set_font(&mut self, family: Option<&str>, ligatures: bool, size: f32) -> bool {
        if !self.text.set_font(family, ligatures) {
            return false;
        }
        let metrics = self.text.measure(size);
        self.text.set_metrics(metrics);
        self.reset_cursor_motion();
        self.dirty = true;
        self.force_present = true;
        true
    }

    pub(super) fn set_background_opacity(&mut self, opacity: f32) {
        let opacity = opacity.clamp(0.0, 1.0);
        if self.background_opacity == opacity {
            return;
        }
        self.background_opacity = opacity;
        let alpha_mode = choose_alpha_mode(&self.alpha_modes, opacity);
        if opacity < 1.0 && alpha_mode == wgpu::CompositeAlphaMode::Opaque {
            log::warn!(
                "Wayland surface does not support alpha compositing; rendering opaque background"
            );
        }
        if alpha_mode != self.config.alpha_mode {
            self.config.alpha_mode = alpha_mode;
            self.surface.configure(&self.device, &self.config);
        }
        self.dirty = true;
        self.force_present = true;
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

    pub(super) fn remove_pane(&mut self, id: PaneId) {
        self.gpus.remove(&id);
        self.trackers.retain(|(pane_id, _)| *pane_id != id);
        if self.cursor_pane == Some(id) {
            self.cursor_pane = None;
            self.reset_cursor_motion();
        }
        self.dirty = true;
        self.force_present = true;
    }

    pub(super) fn switch_session(&mut self) {
        self.cursor_pane = None;
        self.reset_cursor_motion();
        self.retry = None;
        self.dirty = true;
        self.force_present = true;
    }

    pub(super) fn reset_cursor_motion(&mut self) -> bool {
        let moving = self.cursor_motion.moving();
        self.cursor_motion = CursorMotion::default();
        self.dirty |= moving;
        moving
    }

    pub(super) fn restart_blink(&mut self, id: PaneId, now: Instant) -> bool {
        let changed = self
            .trackers
            .iter_mut()
            .find(|(pane_id, _)| *pane_id == id)
            .is_some_and(|(_, tracker)| tracker.restart_cursor_blink(now).is_some());
        self.dirty |= changed;
        changed
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        let progress_deadline = self
            .progress_animation
            .map(|_| Instant::now() + RETRY_DELAY);
        let cursor_deadline = self.cursor_pane.and_then(|id| {
            self.trackers
                .iter()
                .find(|(pane_id, _)| *pane_id == id)
                .and_then(|(_, tracker)| tracker.cursor_deadline())
        });
        self.retry
            .into_iter()
            .chain(cursor_deadline)
            .chain(progress_deadline)
            .min()
    }

    pub(super) fn tick(&mut self, now: Instant) -> bool {
        let retry = self.retry.is_some_and(|retry| retry <= now);
        if retry {
            self.retry = None;
            self.force_present = true;
        }
        let cursor_changed = self.cursor_pane.is_some_and(|id| {
            self.trackers
                .iter_mut()
                .find(|(pane_id, _)| *pane_id == id)
                .is_some_and(|(_, tracker)| tracker.tick_cursor(now).is_some())
        });
        let progress_changed = self.progress_animation.is_some();
        self.dirty |= cursor_changed || progress_changed;
        retry || cursor_changed || progress_changed
    }

    fn ensure_panes(&mut self, panes: &[PaneDraw<'_>], now: Instant) {
        for pane in panes {
            self.gpus.entry(pane.id).or_insert_with(|| PaneGpu {
                backgrounds: CellBackgroundRenderer::new(&self.device, self.config.format),
                images: ImageRenderer::new(&self.device, self.config.format),
                graphics_prepare_failed: false,
            });
            if !self.trackers.iter().any(|(id, _)| *id == pane.id) {
                let mut tracker = PresentationTracker::default();
                tracker.set_cursor_blink_interval(CURSOR_BLINK_INTERVAL, now);
                self.trackers.push((pane.id, tracker));
            }
        }
    }

    pub(super) fn draw(
        &mut self,
        window: &Arc<Window>,
        panes: &[PaneDraw<'_>],
        chrome: WindowChrome<'_>,
    ) -> Result<()> {
        if panes.is_empty() {
            return Ok(());
        }
        let now = Instant::now();
        let background_opacity = if self.config.alpha_mode == wgpu::CompositeAlphaMode::Opaque {
            1.0
        } else {
            self.background_opacity
        };
        let indeterminate = panes
            .iter()
            .any(|pane| pane.progress == Progress::Indeterminate)
            || (chrome.tab_bar.layout.height != 0
                && chrome
                    .tab_bar
                    .tabs
                    .iter()
                    .any(|tab| tab.progress() == Progress::Indeterminate));
        let progress_phase = if indeterminate {
            let start = *self.progress_animation.get_or_insert(now);
            (now.duration_since(start).as_secs_f32() * 0.75).fract()
        } else {
            self.progress_animation = None;
            0.0
        };

        let bar_height = chrome.tab_bar.layout.height;
        let content_height = self.config.height.saturating_sub(bar_height);
        for pane in panes {
            let area = pane.frame.geometry.text_area_pixels().unwrap();
            if pane.rect.origin.x + area.width.get() > self.config.width
                || pane.rect.origin.y + area.height.get() > content_height
            {
                return Ok(());
            }
        }

        self.ensure_panes(panes, now);

        // Prepare each pane's presentation tracker in a disjoint mutable pass over `self.trackers`.
        let mut updates = Vec::with_capacity(panes.len());
        let mut any_rows_changed = false;
        let mut any_cursor_changed = false;
        let mut any_graphics_changed = false;
        let mut focused_index: Option<usize> = None;

        for (tracker_id, tracker) in &mut self.trackers {
            let Some(pane_index) = panes.iter().position(|pane| pane.id == *tracker_id) else {
                continue;
            };
            let pane = &panes[pane_index];
            let update = tracker.prepare(&pane.frame, now)?;
            let damage = update.damage();
            let rows_changed = match damage.rows {
                RowDamage::All => true,
                RowDamage::Rows(rows) => !rows.is_empty(),
            };
            let cursor = if !pane.focused {
                None
            } else if let Some(vi) = pane.vi {
                let rows = pane.frame.viewport.range();
                rows.contains(&vi.cursor.row).then(|| PresentedCursor {
                    position: CellPosition {
                        row: vi.cursor.row - rows.start,
                        column: vi.cursor.column,
                    },
                    appearance: CursorAppearance {
                        blinking: false,
                        ..CursorAppearance::default()
                    },
                    color: pane.frame.palette.color(PaletteEntry::Cursor),
                })
            } else {
                damage.displayed_cursor
            };
            any_rows_changed |= rows_changed;
            any_cursor_changed |= damage.cursor.is_some();
            any_graphics_changed |= damage.graphics_changed;
            if pane.focused {
                focused_index = Some(pane_index);
            }
            updates.push((pane_index, cursor, update));
        }
        updates.sort_unstable_by_key(|(pane_index, _, _)| *pane_index);

        let focused_pane = focused_index.map(|index| &panes[index]);
        let focused_cursor = focused_index.and_then(|index| updates[index].1);
        let focused_id = focused_pane.map(|pane| pane.id);
        let buffer = focused_pane
            .map(|pane| pane.frame.graphics_buffer())
            .unwrap_or(ScreenBuffer::Primary);
        let mut motion = if self.cursor_pane == focused_id && self.cursor_buffer == buffer {
            self.cursor_motion
        } else {
            CursorMotion::default()
        };
        let target = if let Some(pane) = focused_pane
            && pane.focused
            && pane.vi.is_none()
            && pane.preedit.is_empty()
            && pane.frame.viewport.offset() == 0
        {
            focused_cursor.and_then(|cursor| cursor_origin(&pane.frame, cursor.position))
        } else {
            None
        };
        let position = motion.advance(target, now);
        let moving = position.is_some();

        let panes_layout_changed = self.pane_ranges.len() != panes.len()
            || self
                .pane_ranges
                .iter()
                .zip(panes)
                .any(|((id, _), pane)| *id != pane.id);
        let text_changed = any_rows_changed
            || self.dirty
            || panes_layout_changed
            || moving != self.cursor_motion.moving()
            || (!moving && any_cursor_changed);
        let prepare_layers = text_changed || any_graphics_changed || self.force_present;
        if !prepare_layers && !moving {
            for (_, _, update) in updates {
                update.commit();
            }
            self.cursor_pane = focused_id;
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
            self.text.begin_frame();
            self.pane_ranges.clear();
            for (index, pane) in panes.iter().enumerate() {
                let cursor = updates[index].1;
                let pane_moving = moving && focused_index == Some(index);
                let overlay = PaneOverlay {
                    selection: pane.selection,
                    hovered_link: pane.hovered_link,
                    progress: pane.progress,
                    progress_phase,
                    preedit: pane.preedit,
                    vi: pane.vi,
                    zoomed: pane.zoomed,
                };
                let range = self.text.prepare_pane(
                    &pane.frame,
                    cursor.filter(|_| !pane_moving),
                    &overlay,
                )?;
                self.pane_ranges.push((pane.id, range));
            }
            if let (Some(width), Some(height)) = (
                NonZeroU32::new(self.config.width),
                NonZeroU32::new(content_height),
            ) {
                self.chrome_range = self.text.prepare_chrome(
                    PixelSize { width, height },
                    chrome.palette,
                    chrome.dividers,
                    chrome.focused_rect,
                    chrome.notice,
                );
            } else {
                self.chrome_range = 0..0;
            }
            self.tab_bar_range =
                self.text
                    .prepare_tab_bar(&chrome.tab_bar, progress_phase, background_opacity);
            self.text.finish_frame()?;
        }

        let mut draw_moving_cursor = false;
        if let Some(position) = position
            && let Some(target) = target
            && let Some(pane) = focused_pane
            && let Some(cursor) = focused_cursor
            && let Some(paint) = pane
                .frame
                .cursor_paint(cursor, NonZeroU32::new(2).unwrap())?
        {
            let mut rect = paint.rect;
            rect.origin.x = position.x + (rect.origin.x - target.x);
            rect.origin.y = position.y + (rect.origin.y - target.y);
            let area = pane.frame.geometry.text_area_pixels().unwrap();
            self.text.prepare_moving_cursor(area, rect, paint.color);
            draw_moving_cursor = true;
        }

        let mut total_failures = 0usize;
        if prepare_layers {
            for pane in panes {
                let gpu = self.gpus.get_mut(&pane.id).unwrap();
                gpu.backgrounds
                    .prepare(&self.device, &self.queue, &pane.frame)
                    .map_err(|error| {
                        std::io::Error::other(format!(
                            "Failed to prepare terminal backgrounds: {error:?}"
                        ))
                    })?;
                let failures = gpu.images.prepare(&self.device, &self.queue, &pane.frame);
                if failures != 0 {
                    if !gpu.graphics_prepare_failed {
                        log::warn!("Failed to prepare {failures} terminal graphics placements");
                    }
                    total_failures += failures;
                }
                gpu.graphics_prepare_failed = failures != 0;
            }
        }

        let view = texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let background = chrome.palette.color(if chrome.reverse_video {
            PaletteEntry::Foreground
        } else {
            PaletteEntry::Background
        });
        let lut = linear_colors();
        let clear_alpha = if self.config.alpha_mode == wgpu::CompositeAlphaMode::Opaque {
            1.0
        } else {
            f64::from(self.background_opacity)
        };
        let clear_scale = if self.config.alpha_mode == wgpu::CompositeAlphaMode::PreMultiplied {
            clear_alpha
        } else {
            1.0
        };
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(lut[background.red as usize]) * clear_scale,
                            g: f64::from(lut[background.green as usize]) * clear_scale,
                            b: f64::from(lut[background.blue as usize]) * clear_scale,
                            a: clear_alpha,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            for (index, pane) in panes.iter().enumerate() {
                let Some(gpu) = self.gpus.get(&pane.id) else {
                    continue;
                };
                let range = self.pane_ranges[index].1.clone();
                let area = pane.frame.geometry.text_area_pixels().unwrap();
                let (x, y) = (pane.rect.origin.x, pane.rect.origin.y);
                let (width, height) = (area.width.get(), area.height.get());
                let is_moving = draw_moving_cursor && focused_index == Some(index);
                pass.set_viewport(x as f32, y as f32, width as f32, height as f32, 0.0, 1.0);
                pass.set_scissor_rect(x, y, width, height);
                gpu.images.draw_frame(&mut pass, &gpu.backgrounds, |pass| {
                    self.text.draw_range(pass, range, is_moving)
                });
            }
            if !self.chrome_range.is_empty() && content_height != 0 {
                pass.set_viewport(
                    0.0,
                    0.0,
                    self.config.width as f32,
                    content_height as f32,
                    0.0,
                    1.0,
                );
                pass.set_scissor_rect(0, 0, self.config.width, content_height);
                self.text
                    .draw_range(&mut pass, self.chrome_range.clone(), false);
            }
            if bar_height != 0 && !self.tab_bar_range.is_empty() {
                let top = chrome.tab_bar.layout.top;
                pass.set_viewport(
                    0.0,
                    top as f32,
                    self.config.width as f32,
                    bar_height as f32,
                    0.0,
                    1.0,
                );
                pass.set_scissor_rect(0, top, self.config.width, bar_height);
                self.text
                    .draw_range(&mut pass, self.tab_bar_range.clone(), false);
            }
        }
        self.queue.submit([encoder.finish()]);
        window.pre_present_notify();
        self.queue.present(texture);
        for (_, _, update) in updates {
            update.commit();
        }
        self.cursor_pane = focused_id;
        self.cursor_motion = motion;
        self.cursor_buffer = buffer;
        self.force_present = false;
        self.dirty = false;
        if suboptimal {
            self.surface.configure(&self.device, &self.config);
        }
        if total_failures != 0 {
            self.retry = Some(Instant::now() + Duration::from_millis(100));
        }
        if moving || self.progress_animation.is_some() {
            window.request_redraw();
        }
        Ok(())
    }
}

fn choose_alpha_mode(modes: &[wgpu::CompositeAlphaMode], opacity: f32) -> wgpu::CompositeAlphaMode {
    use wgpu::CompositeAlphaMode as Mode;
    let preferred: &[Mode] = if opacity >= 1.0 {
        &[
            Mode::Opaque,
            Mode::PreMultiplied,
            Mode::PostMultiplied,
            Mode::Auto,
        ]
    } else {
        &[
            Mode::PreMultiplied,
            Mode::PostMultiplied,
            Mode::Auto,
            Mode::Opaque,
        ]
    };
    preferred
        .iter()
        .copied()
        .find(|mode| modes.contains(mode))
        .or_else(|| modes.first().copied())
        .unwrap_or(Mode::Auto)
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

#[cfg(test)]
mod tests {
    use wgpu::CompositeAlphaMode as Mode;

    use super::choose_alpha_mode;

    #[test]
    fn alpha_mode_prefers_opaque_at_full_opacity_and_premultiplied_when_translucent() {
        let all = [Mode::Opaque, Mode::PreMultiplied, Mode::PostMultiplied];
        assert_eq!(choose_alpha_mode(&all, 1.0), Mode::Opaque);
        assert_eq!(choose_alpha_mode(&all, 0.85), Mode::PreMultiplied);
        assert_eq!(
            choose_alpha_mode(&[Mode::Opaque, Mode::PostMultiplied], 0.5),
            Mode::PostMultiplied
        );
        assert_eq!(choose_alpha_mode(&[Mode::Opaque], 0.5), Mode::Opaque);
    }
}
