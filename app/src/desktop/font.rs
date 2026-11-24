use std::num::NonZeroU32;

use cosmic_text::{
    Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, SwashCache, SwashContent,
    SwashImage, Weight,
};
use lune_terminal::PixelSize;

pub(super) fn physical_font_size(points: f32, scale_factor: f64) -> f32 {
    const LOGICAL_PIXELS_PER_POINT: f32 = 96.0 / 72.0;
    (points * LOGICAL_PIXELS_PER_POINT * scale_factor as f32).clamp(8.0, 128.0)
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct FontMetrics {
    pub size: f32,
    pub cell: PixelSize,
    pub baseline: i32,
}

pub(super) struct Font {
    system: FontSystem,
    family: Option<String>,
    buffer: Buffer,
    swash: SwashCache,
}

impl Font {
    pub(super) fn new(family: Option<&str>) -> Self {
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
        let family = family.and_then(|requested| {
            let installed = system
                .db()
                .faces()
                .flat_map(|face| &face.families)
                .find(|(name, _)| name.eq_ignore_ascii_case(requested));
            match installed {
                Some((name, _)) => Some(name.clone()),
                None => {
                    log::warn!(
                        "Configured font family is unavailable. Using the default monospace font."
                    );
                    None
                }
            }
        });
        Self {
            system,
            family,
            buffer: Buffer::new_empty(Metrics::new(16.0, 21.0)),
            swash: SwashCache::new(),
        }
    }

    pub(super) fn measure(&mut self, size: f32) -> FontMetrics {
        let fallback_height = (size * 1.30).ceil();
        self.buffer.set_metrics(Metrics::new(size, fallback_height));
        self.shape("M", 0);
        let (width, height, baseline) = self.buffer.layout_runs().next().map_or_else(
            || ((size * 0.6).ceil(), fallback_height, size * 0.95),
            |run| {
                let height = run
                    .glyphs
                    .first()
                    .and_then(|glyph| self.system.get_font(glyph.font_id, glyph.font_weight))
                    .map(|font| {
                        let metrics = font.as_swash().metrics(&[]).scale(size);
                        metrics.ascent + metrics.descent + metrics.leading.max(0.0)
                    })
                    .filter(|height| height.is_finite() && *height > 0.0)
                    .map_or(fallback_height, |height| height.round().max(1.0));
                (
                    run.line_w.round().max(1.0),
                    height,
                    run.line_y + (height - fallback_height) * 0.5,
                )
            },
        );
        FontMetrics {
            size,
            cell: PixelSize {
                width: NonZeroU32::new(width as u32).unwrap(),
                height: NonZeroU32::new(height as u32).unwrap(),
            },
            baseline: baseline.round() as i32,
        }
    }

    fn shape(&mut self, text: &str, rendition: usize) {
        let attrs = Attrs::new()
            .family(
                self.family
                    .as_deref()
                    .map_or(Family::Monospace, Family::Name),
            )
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

    pub(super) fn measure_text(&mut self, metrics: FontMetrics, text: &str) -> u32 {
        self.buffer
            .set_metrics(Metrics::new(metrics.size, metrics.cell.height.get() as f32));
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
            if blend_bitmap(
                &image,
                physical.x + image.placement.left,
                physical.y - image.placement.top,
                width,
                height,
                pixels,
            ) {
                visible = true;
                colored |= image.content == SwashContent::Color;
            }
        }
        visible.then_some(colored)
    }

    pub(super) fn rasterize_icon(
        &mut self,
        metrics: FontMetrics,
        text: &str,
        rendition: usize,
        max_width: u32,
        pixels: &mut Vec<u8>,
    ) -> Option<(u32, bool)> {
        let height = metrics.cell.height.get();
        self.buffer
            .set_metrics(Metrics::new(metrics.size, height as f32));
        self.shape(text, rendition);
        let run = self.buffer.layout_runs().next()?;
        let glyph = run.glyphs.first()?;
        let offset = ((metrics.cell.width.get() as f32 - run.line_w) * 0.5)
            .max(0.0)
            .floor();
        let physical = glyph.physical((offset, metrics.baseline as f32), 1.0);
        let image = self
            .swash
            .get_image_uncached(&mut self.system, physical.cache_key)?;
        let x = (physical.x + image.placement.left).max(0);
        let width = x as u32 + image.placement.width;
        if width == 0 || width > max_width {
            return None;
        }
        pixels.clear();
        pixels.resize(width as usize * height as usize * 4, 0);
        blend_bitmap(
            &image,
            x,
            physical.y - image.placement.top,
            width,
            height,
            pixels,
        )
        .then_some((width, image.content == SwashContent::Color))
    }
}

fn blend_bitmap(
    image: &SwashImage,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    pixels: &mut [u8],
) -> bool {
    if image.content == SwashContent::SubpixelMask {
        return false;
    }
    let is_color = image.content == SwashContent::Color;
    let placement = image.placement;
    let left = x.max(0);
    let right = (x + placement.width as i32).min(width as i32);
    let mut visible = false;
    for row in y.max(0)..(y + placement.height as i32).min(height as i32) {
        for column in left..right {
            let source = ((row - y) as u32 * placement.width + (column - x) as u32) as usize;
            let rgba = if is_color {
                image.data[source * 4..][..4].try_into().unwrap()
            } else {
                [255, 255, 255, image.data[source]]
            };
            if rgba[3] != 0 {
                visible = true;
                let index = (row as usize * width as usize + column as usize) * 4;
                composite(&mut pixels[index..][..4], rgba);
            }
        }
    }
    visible
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
