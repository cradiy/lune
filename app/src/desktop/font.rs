use std::num::NonZeroU32;
use std::sync::OnceLock;

use cosmic_text::{
    Attrs, Buffer, Family, FeatureTag, FontFeatures, FontSystem, Metrics, Shaping, Style,
    SwashCache, SwashContent, SwashImage, Weight,
};
use lune_terminal::PixelSize;

pub(super) fn physical_font_size(points: f32, scale_factor: f64) -> f32 {
    const LOGICAL_PIXELS_PER_POINT: f32 = 96.0 / 72.0;
    (points * LOGICAL_PIXELS_PER_POINT * scale_factor as f32).clamp(8.0, 128.0)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum MetricAdjustment {
    Pixels(i32),
    Percent(f32),
}

impl Default for MetricAdjustment {
    fn default() -> Self {
        Self::Pixels(0)
    }
}

impl MetricAdjustment {
    pub(super) fn delta(self, base: f32, scale_factor: f64) -> f32 {
        match self {
            Self::Pixels(px) => px as f32 * scale_factor.max(1.0) as f32,
            Self::Percent(pct) => base * (pct / 100.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FontFeature {
    pub(super) tag: [u8; 4],
    pub(super) value: u32,
}

impl FontFeature {
    pub(super) fn parse(spec: &str) -> Result<Self, &'static str> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("font-feature entry cannot be empty");
        }
        let (sign, rest) = match spec.as_bytes()[0] {
            b'+' => (Some(true), &spec[1..]),
            b'-' => (Some(false), &spec[1..]),
            _ => (None, spec),
        };
        let (tag_str, explicit_value) = match rest.split_once('=') {
            Some((tag, value)) => {
                if sign == Some(false) {
                    return Err("font-feature cannot combine '-' prefix with '=value'");
                }
                let parsed = value
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| "font-feature value must be a non-negative integer")?;
                (tag.trim(), Some(parsed))
            }
            None => (rest.trim(), None),
        };
        let bytes = tag_str.as_bytes();
        if bytes.len() != 4 || !bytes.iter().all(|&b| b.is_ascii_graphic()) {
            return Err("font-feature tag must be 4 printable ASCII characters");
        }
        let tag = [bytes[0], bytes[1], bytes[2], bytes[3]];
        let value = match (sign, explicit_value) {
            (Some(false), _) => 0,
            (_, Some(v)) => v,
            _ => 1,
        };
        Ok(Self { tag, value })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct FontConfig {
    pub(super) family: Option<String>,
    pub(super) family_bold: Option<String>,
    pub(super) family_italic: Option<String>,
    pub(super) family_bold_italic: Option<String>,
    pub(super) ligatures: bool,
    pub(super) features: Vec<FontFeature>,
    pub(super) thicken: bool,
    pub(super) adjust_cell_height: MetricAdjustment,
    pub(super) adjust_cell_width: MetricAdjustment,
    pub(super) adjust_baseline: MetricAdjustment,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: None,
            family_bold: None,
            family_italic: None,
            family_bold_italic: None,
            ligatures: true,
            features: Vec::new(),
            thicken: false,
            adjust_cell_height: MetricAdjustment::default(),
            adjust_cell_width: MetricAdjustment::default(),
            adjust_baseline: MetricAdjustment::default(),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct FontMetrics {
    pub size: f32,
    pub cell: PixelSize,
    pub baseline: i32,
}

pub(super) struct Font {
    system: FontSystem,
    /// Resolved family names indexed by rendition (`0=regular, 1=bold, 2=italic, 3=bold-italic`).
    families: [Option<String>; 4],
    ligatures: bool,
    features: Vec<FontFeature>,
    thicken: bool,
    adjust_cell_height: MetricAdjustment,
    adjust_cell_width: MetricAdjustment,
    adjust_baseline: MetricAdjustment,
    buffer: Buffer,
    swash: SwashCache,
}

impl Font {
    pub(super) fn new(config: &FontConfig) -> Self {
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
        let families = resolve_families(&system, config);
        Self {
            system,
            families,
            ligatures: config.ligatures,
            features: config.features.clone(),
            thicken: config.thicken,
            adjust_cell_height: config.adjust_cell_height,
            adjust_cell_width: config.adjust_cell_width,
            adjust_baseline: config.adjust_baseline,
            buffer: Buffer::new_empty(Metrics::new(16.0, 21.0)),
            swash: SwashCache::new(),
        }
    }

    pub(super) fn reconfigure(&mut self, config: &FontConfig) -> bool {
        let resolved = resolve_families(&self.system, config);
        if self.families == resolved
            && self.ligatures == config.ligatures
            && self.features == config.features
            && self.thicken == config.thicken
            && self.adjust_cell_height == config.adjust_cell_height
            && self.adjust_cell_width == config.adjust_cell_width
            && self.adjust_baseline == config.adjust_baseline
        {
            return false;
        }
        self.families = resolved;
        self.ligatures = config.ligatures;
        self.features = config.features.clone();
        self.thicken = config.thicken;
        self.adjust_cell_height = config.adjust_cell_height;
        self.adjust_cell_width = config.adjust_cell_width;
        self.adjust_baseline = config.adjust_baseline;
        true
    }

    pub(super) fn ligatures(&self) -> bool {
        self.ligatures
    }

    pub(super) fn measure(&mut self, size: f32, scale_factor: f64) -> FontMetrics {
        let fallback_height = (size * 1.30).ceil();
        self.buffer.set_metrics(Metrics::new(size, fallback_height));
        self.shape("M", 0, false);
        let (base_width, base_height, base_baseline) =
            self.buffer.layout_runs().next().map_or_else(
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
        let width_delta = self.adjust_cell_width.delta(base_width, scale_factor);
        let height_delta = self.adjust_cell_height.delta(base_height, scale_factor);
        let baseline_delta = self.adjust_baseline.delta(base_height, scale_factor);

        let width = (base_width + width_delta).round().max(1.0) as u32;
        let height = (base_height + height_delta).round().max(1.0) as u32;
        let applied_height_delta = height as f32 - base_height;
        let baseline = (base_baseline + applied_height_delta * 0.5 + baseline_delta).round() as i32;

        FontMetrics {
            size,
            cell: PixelSize {
                width: NonZeroU32::new(width).unwrap(),
                height: NonZeroU32::new(height).unwrap(),
            },
            baseline,
        }
    }

    fn shape(&mut self, text: &str, rendition: usize, ligature_run: bool) {
        let family = self.families[rendition & 3]
            .as_deref()
            .map_or(Family::Monospace, Family::Name);
        let mut attrs = Attrs::new()
            .family(family)
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
        let apply_ligature_flags =
            ligature_run || (!self.ligatures && text.chars().nth(1).is_some());
        if apply_ligature_flags || !self.features.is_empty() {
            let mut features = FontFeatures::new();
            if apply_ligature_flags {
                let enabled = u32::from(self.ligatures);
                features
                    .set(FeatureTag::STANDARD_LIGATURES, enabled)
                    .set(FeatureTag::CONTEXTUAL_LIGATURES, enabled)
                    .set(FeatureTag::CONTEXTUAL_ALTERNATES, enabled);
                if ligature_run {
                    features.disable(FeatureTag::KERNING);
                }
            }
            for feature in &self.features {
                features.set(FeatureTag::new(&feature.tag), feature.value);
            }
            attrs = attrs.font_features(features);
        }
        self.buffer.set_text(text, &attrs, Shaping::Advanced, None);
        self.buffer.shape_until_scroll(&mut self.system, false);
    }

    pub(super) fn measure_text(&mut self, metrics: FontMetrics, text: &str) -> u32 {
        self.buffer
            .set_metrics(Metrics::new(metrics.size, metrics.cell.height.get() as f32));
        self.shape(text, 0, false);
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
        ligature_run: bool,
        pixels: &mut Vec<u8>,
    ) -> Option<bool> {
        let height = metrics.cell.height.get();
        self.buffer
            .set_metrics(Metrics::new(metrics.size, height as f32));
        self.shape(text, rendition, ligature_run);
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
                self.thicken,
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
        self.shape(text, rendition, false);
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
            self.thicken,
            pixels,
        )
        .then_some((width, image.content == SwashContent::Color))
    }
}

fn resolve_families(system: &FontSystem, config: &FontConfig) -> [Option<String>; 4] {
    let regular = resolve_family(system, config.family.as_deref(), "font-family");
    let bold = resolve_family(system, config.family_bold.as_deref(), "font-family-bold")
        .or_else(|| regular.clone());
    let italic = resolve_family(
        system,
        config.family_italic.as_deref(),
        "font-family-italic",
    )
    .or_else(|| regular.clone());
    let bold_italic = resolve_family(
        system,
        config.family_bold_italic.as_deref(),
        "font-family-bold-italic",
    )
    .or_else(|| regular.clone());
    [regular, bold, italic, bold_italic]
}

fn resolve_family(system: &FontSystem, family: Option<&str>, setting: &str) -> Option<String> {
    family.and_then(|requested| {
        let installed = system
            .db()
            .faces()
            .flat_map(|face| &face.families)
            .find(|(name, _)| name.eq_ignore_ascii_case(requested));
        match installed {
            Some((name, _)) => Some(name.clone()),
            None => {
                log::warn!("Configured {setting} is unavailable. Falling back to default font.");
                None
            }
        }
    })
}

fn thicken_lut() -> &'static [u8; 256] {
    static LUT: OnceLock<[u8; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut table = [0u8; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let a = i as f32 / 255.0;
            *slot = (a.powf(1.0 / 1.35) * 255.0).round().clamp(0.0, 255.0) as u8;
        }
        table
    })
}

fn blend_bitmap(
    image: &SwashImage,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    thicken: bool,
    pixels: &mut [u8],
) -> bool {
    if image.content == SwashContent::SubpixelMask {
        return false;
    }
    let is_color = image.content == SwashContent::Color;
    let lut = thicken.then(thicken_lut);
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
                let raw = image.data[source];
                let alpha = lut.map_or(raw, |table| table[raw as usize]);
                [255, 255, 255, alpha]
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

#[cfg(test)]
mod tests {
    use super::{FontFeature, MetricAdjustment, thicken_lut};

    #[test]
    fn parses_font_feature_specs_and_rejects_invalid_ones() {
        assert_eq!(
            FontFeature::parse("ss01").unwrap(),
            FontFeature {
                tag: *b"ss01",
                value: 1,
            }
        );
        assert_eq!(
            FontFeature::parse("+zero").unwrap(),
            FontFeature {
                tag: *b"zero",
                value: 1,
            }
        );
        assert_eq!(
            FontFeature::parse("-calt").unwrap(),
            FontFeature {
                tag: *b"calt",
                value: 0,
            }
        );
        assert_eq!(
            FontFeature::parse("cv02=3").unwrap(),
            FontFeature {
                tag: *b"cv02",
                value: 3,
            }
        );
        assert!(FontFeature::parse("-cv02=1").is_err());
        assert!(FontFeature::parse("abc").is_err());
        assert!(FontFeature::parse("abcde").is_err());
        assert!(FontFeature::parse("").is_err());
    }

    #[test]
    fn metric_adjustment_scales_pixels_and_percentages() {
        assert_eq!(MetricAdjustment::Pixels(2).delta(20.0, 1.0), 2.0);
        assert_eq!(MetricAdjustment::Pixels(2).delta(20.0, 2.0), 4.0);
        assert_eq!(MetricAdjustment::Percent(15.0).delta(20.0, 2.0), 3.0);
        assert_eq!(MetricAdjustment::Percent(-10.0).delta(20.0, 1.0), -2.0);
    }

    #[test]
    fn thicken_lut_preserves_endpoints_and_boosts_midtones() {
        let lut = thicken_lut();
        assert_eq!(lut[0], 0);
        assert_eq!(lut[255], 255);
        assert!(lut[128] > 128);
        assert!(lut.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}
