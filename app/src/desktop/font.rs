use std::num::NonZeroU32;

use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, SwashCache, SwashContent, Weight,
};
use lune_terminal::PixelSize;

#[derive(Clone, Copy, PartialEq)]
pub(super) struct FontMetrics {
    pub size: f32,
    pub cell: PixelSize,
    pub baseline: i32,
}

pub(super) struct Font {
    system: FontSystem,
    buffer: Buffer,
    swash: SwashCache,
}

impl Font {
    pub(super) fn new() -> Self {
        let mut system = FontSystem::new();
        let database = system.db();
        if database
            .query(&cosmic_text::fontdb::Query {
                families: &[Family::Monospace],
                ..Default::default()
            })
            .is_none()
        {
            let family = database
                .faces()
                .find(|face| face.monospaced)
                .and_then(|face| face.families.first().map(|(name, _)| name.clone()));
            if let Some(family) = family {
                system.db_mut().set_monospace_family(family);
            }
        }
        Self {
            system,
            buffer: Buffer::new_empty(Metrics::new(16.0, 21.0)),
            swash: SwashCache::new(),
        }
    }

    pub(super) fn measure(&mut self, size: f32) -> FontMetrics {
        let size = size.clamp(8.0, 128.0);
        let height = (size * 1.30).ceil() as u32;
        self.buffer.set_metrics(Metrics::new(size, height as f32));
        self.shape("M", 0);
        let (width, baseline) = self.buffer.layout_runs().next().map_or(
            ((size * 0.6).ceil() as u32, (size * 0.95) as i32),
            |run| {
                (
                    run.line_w.round().max(1.0) as u32,
                    run.line_y.round() as i32,
                )
            },
        );
        FontMetrics {
            size,
            cell: PixelSize {
                width: NonZeroU32::new(width).unwrap(),
                height: NonZeroU32::new(height).unwrap(),
            },
            baseline,
        }
    }

    fn shape(&mut self, text: &str, rendition: usize) {
        let attrs = Attrs::new()
            .family(Family::Monospace)
            .weight(if rendition & 1 == 0 {
                Weight::NORMAL
            } else {
                Weight::BOLD
            })
            .style(if rendition & 2 == 0 {
                Style::Normal
            } else {
                Style::Italic
            });
        self.buffer.set_text(text, &attrs, Shaping::Advanced, None);
        self.buffer.shape_until_scroll(&mut self.system, false);
    }

    pub(super) fn measure_text(&mut self, text: &str) -> u32 {
        self.shape(text, 0);
        self.buffer
            .layout_runs()
            .next()
            .map_or(0, |run| run.line_w.ceil() as u32)
    }

    pub(super) fn rasterize(
        &mut self,
        metrics: FontMetrics,
        text: &str,
        rendition: usize,
        width: u32,
        pixels: &mut Vec<u8>,
    ) -> Option<bool> {
        let height = metrics.cell.height.get();
        self.buffer
            .set_metrics(Metrics::new(metrics.size, height as f32));
        self.shape(text, rendition);
        let run = self.buffer.layout_runs().next()?;
        let offset = ((width as f32 - run.line_w) * 0.5).max(0.0).floor();
        pixels.clear();
        pixels.resize(width as usize * height as usize * 4, 0);

        let mut colored = false;
        let mut visible = false;
        for glyph in run.glyphs {
            let physical = glyph.physical((offset, metrics.baseline as f32), 1.0);
            let Some(image) = self
                .swash
                .get_image_uncached(&mut self.system, physical.cache_key)
            else {
                continue;
            };
            let is_color = image.content == SwashContent::Color;
            if image.content == SwashContent::SubpixelMask {
                continue;
            }
            let placement = image.placement;
            let x = physical.x + placement.left;
            let y = physical.y - placement.top;
            let left = x.max(0);
            let right = (x + placement.width as i32).min(width as i32);
            for row in y.max(0)..(y + placement.height as i32).min(height as i32) {
                for column in left..right {
                    let source =
                        ((row - y) as u32 * placement.width + (column - x) as u32) as usize;
                    let rgba = if is_color {
                        image.data[source * 4..][..4].try_into().unwrap()
                    } else {
                        [255, 255, 255, image.data[source]]
                    };
                    if rgba[3] != 0 {
                        colored |= is_color;
                        visible = true;
                        let index = (row as usize * width as usize + column as usize) * 4;
                        composite(&mut pixels[index..][..4], rgba);
                    }
                }
            }
        }
        visible.then_some(colored)
    }
}

fn composite(destination: &mut [u8], source: [u8; 4]) {
    let source_alpha = u32::from(source[3]);
    let destination_weight = u32::from(destination[3]) * (255 - source_alpha);
    let source_weight = source_alpha * 255;
    let weight = source_weight + destination_weight;
    for channel in 0..3 {
        destination[channel] = ((u32::from(source[channel]) * source_weight
            + u32::from(destination[channel]) * destination_weight)
            / weight) as u8;
    }
    destination[3] = weight.div_ceil(255) as u8;
}
