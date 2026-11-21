use std::sync::OnceLock;

use lune_terminal::{Palette, PaletteEntry, Row, RowRevision, StyleError};
use wgpu::{Buffer, Device, Queue, RenderPipeline, TextureFormat};

use super::{Instance, create_quad_pipeline, write_instances};
use crate::{GridGeometry, PresentationFrame};

#[derive(Debug)]
pub enum BackgroundPrepareError {
    Style(StyleError),
    BufferLimit,
}

pub struct CellBackgroundRenderer {
    pipeline: RenderPipeline,
    buffer: Option<Buffer>,
    instances: Vec<Instance>,
    count: u32,
    geometry: Option<GridGeometry>,
    reverse_video: bool,
    rows: Vec<RowRevision>,
    palette: Option<Palette>,
}

impl CellBackgroundRenderer {
    pub fn new(device: &Device, format: TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(include_str!("image.wgsl").into()),
        });
        Self {
            pipeline: create_quad_pipeline(
                device,
                &module,
                format,
                "solid_vertex",
                "solid_fragment",
                None,
            ),
            buffer: None,
            instances: Vec::new(),
            count: 0,
            geometry: None,
            reverse_video: false,
            rows: Vec::new(),
            palette: None,
        }
    }

    /// Prepares opaque runs that differ from the frame's default background.
    pub fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        frame: &PresentationFrame<'_>,
    ) -> Result<(), BackgroundPrepareError> {
        let same_colors = self.palette.as_ref().is_some_and(|palette| {
            palette.revision() == frame.palette.revision()
                || palette.same_text_colors(frame.palette)
        });
        if self.geometry == Some(frame.geometry)
            && self.reverse_video == frame.reverse_video
            && same_colors
            && frame
                .viewport
                .iter_rows()
                .map(Row::revision)
                .eq(self.rows.iter().copied())
        {
            return Ok(());
        }

        self.geometry = None;
        self.count = 0;
        self.instances.clear();
        let Some(area) = frame
            .geometry
            .text_area_pixels()
            .filter(|_| frame.geometry.cell_pixels().is_some())
        else {
            return Ok(());
        };
        let default = frame.palette.color(if frame.reverse_video {
            PaletteEntry::Foreground
        } else {
            PaletteEntry::Background
        });
        let scale_x = 2.0 / area.width.get() as f32;
        let scale_y = 2.0 / area.height.get() as f32;
        let max_instances = device.limits().max_buffer_size / size_of::<Instance>() as u64;
        let linear = linear_colors();
        let size = frame.geometry.pty_size();
        for row in 0..usize::from(size.rows).min(frame.viewport.rows()) {
            for run in frame
                .row_backgrounds(row, 0..size.columns.into())
                .expect("The row has pixel geometry")
            {
                let run = run.map_err(BackgroundPrepareError::Style)?;
                if run.color == default {
                    continue;
                }
                if self.instances.len() as u64 == max_instances {
                    return Err(BackgroundPrepareError::BufferLimit);
                }
                let rect = run.rect;
                self.instances.push([
                    rect.origin.x as f32 * scale_x - 1.0,
                    1.0 - rect.origin.y as f32 * scale_y,
                    (rect.origin.x + rect.size.width.get()) as f32 * scale_x - 1.0,
                    1.0 - (rect.origin.y + rect.size.height.get()) as f32 * scale_y,
                    linear[usize::from(run.color.red)],
                    linear[usize::from(run.color.green)],
                    linear[usize::from(run.color.blue)],
                    1.0,
                ]);
            }
        }
        write_instances(device, queue, &mut self.buffer, &self.instances);
        self.count = self.instances.len() as u32;
        self.rows.clear();
        self.rows
            .extend(frame.viewport.iter_rows().map(Row::revision));
        self.geometry = Some(frame.geometry);
        self.reverse_video = frame.reverse_video;
        if !same_colors {
            self.palette = Some(frame.palette.clone());
        }
        Ok(())
    }

    /// Draws cell backgrounds between the BehindBackground and BehindText image layers.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.count == 0 {
            return;
        }
        let buffer = self.buffer.as_ref().expect("Instances have a buffer");
        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, buffer.slice(..));
        pass.draw(0..4, 0..self.count);
    }
}

fn linear_colors() -> &'static [f32; 256] {
    static LINEAR: OnceLock<[f32; 256]> = OnceLock::new();
    LINEAR.get_or_init(|| {
        std::array::from_fn(|channel| {
            let value = channel as f32 / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        })
    })
}
