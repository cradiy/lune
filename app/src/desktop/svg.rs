//! Cached CPU SVG rasterization via `resvg` for crisp UI icons and logos.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

pub(super) const MAX_SVG_SIZE: u32 = 512;
const TREE_CACHE_CAPACITY: usize = 32;
const RASTER_CACHE_CAPACITY: usize = 64;

#[allow(dead_code)]
pub(super) mod icons {
    pub(in crate::desktop) const APP_LOGO: &str =
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../assets/lune.svg"));
    pub(in crate::desktop) const SEARCH: &str = include_str!("icons/search.svg");
    pub(in crate::desktop) const PLUS: &str = include_str!("icons/plus.svg");
    pub(in crate::desktop) const CLOSE: &str = include_str!("icons/close.svg");
    pub(in crate::desktop) const MINIMIZE: &str = include_str!("icons/minimize.svg");
    pub(in crate::desktop) const MAXIMIZE: &str = include_str!("icons/maximize.svg");
    pub(in crate::desktop) const RESTORE: &str = include_str!("icons/restore.svg");
    pub(in crate::desktop) const TERMINAL: &str = include_str!("icons/terminal.svg");
    pub(in crate::desktop) const COPY: &str = include_str!("icons/copy.svg");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum SvgMode {
    /// Monochrome alpha mask (`[255, 255, 255, a]`) tinted dynamically by the GPU shader.
    Mask,
    /// Full-color straight-RGBA image preserving original SVG colors and gradients.
    Color,
}

#[derive(Debug)]
pub(super) enum SvgError {
    InvalidSize,
    Parse(resvg::usvg::Error),
    Allocation,
}

impl fmt::Display for SvgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSize => write!(f, "SVG size must be between 1 and {MAX_SVG_SIZE} pixels"),
            Self::Parse(error) => write!(f, "invalid SVG: {error}"),
            Self::Allocation => write!(f, "failed to allocate SVG pixel buffer"),
        }
    }
}

impl std::error::Error for SvgError {}

impl From<resvg::usvg::Error> for SvgError {
    fn from(error: resvg::usvg::Error) -> Self {
        Self::Parse(error)
    }
}

#[derive(Clone)]
pub(super) struct SvgRaster {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) colored: bool,
    pub(super) rgba: Arc<[u8]>,
}

#[derive(Default)]
pub(super) struct SvgCache {
    trees: VecDeque<(&'static str, resvg::usvg::Tree)>,
    rasters: VecDeque<(&'static str, u32, u32, SvgMode, SvgRaster)>,
}

impl SvgCache {
    pub(super) fn rasterize(
        &mut self,
        source: &'static str,
        width: u32,
        height: u32,
        mode: SvgMode,
    ) -> Result<SvgRaster, SvgError> {
        if !(1..=MAX_SVG_SIZE).contains(&width) || !(1..=MAX_SVG_SIZE).contains(&height) {
            return Err(SvgError::InvalidSize);
        }
        if let Some(index) = self.rasters.iter().position(|(svg, w, h, m, _)| {
            *svg == source && *w == width && *h == height && *m == mode
        }) {
            let entry = self.rasters.remove(index).expect("cache entry exists");
            let raster = entry.4.clone();
            self.rasters.push_back(entry);
            return Ok(raster);
        }

        if !self.trees.iter().any(|(svg, _)| *svg == source) {
            let options = resvg::usvg::Options {
                image_href_resolver: resvg::usvg::ImageHrefResolver {
                    resolve_data: Box::new(|_, _, _| None),
                    resolve_string: Box::new(|_, _| None),
                },
                ..Default::default()
            };
            let tree = resvg::usvg::Tree::from_str(source, &options)?;
            if self.trees.len() == TREE_CACHE_CAPACITY {
                self.trees.pop_front();
            }
            self.trees.push_back((source, tree));
        }
        let tree = &self
            .trees
            .iter()
            .find(|(svg, _)| *svg == source)
            .expect("parsed SVG tree exists")
            .1;

        let mut pixmap =
            resvg::tiny_skia::Pixmap::new(width, height).ok_or(SvgError::Allocation)?;
        let tree_size = tree.size();
        let scale = (width as f32 / tree_size.width()).min(height as f32 / tree_size.height());
        let transform = resvg::tiny_skia::Transform::from_row(
            scale,
            0.0,
            0.0,
            scale,
            (width as f32 - tree_size.width() * scale) * 0.5,
            (height as f32 - tree_size.height() * scale) * 0.5,
        );
        resvg::render(tree, transform, &mut pixmap.as_mut());

        let mut bytes = pixmap.take();
        // tiny-skia emits premultiplied RGBA; convert to straight RGBA or a pure white alpha mask.
        for pixel in bytes.as_chunks_mut::<4>().0 {
            let alpha = u32::from(pixel[3]);
            if alpha == 0 {
                continue;
            }
            match mode {
                SvgMode::Mask => {
                    pixel[0] = 255;
                    pixel[1] = 255;
                    pixel[2] = 255;
                }
                SvgMode::Color => {
                    for channel in &mut pixel[..3] {
                        *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
                    }
                }
            }
        }

        let raster = SvgRaster {
            width,
            height,
            colored: mode == SvgMode::Color,
            rgba: bytes.into(),
        };
        if self.rasters.len() == RASTER_CACHE_CAPACITY {
            self.rasters.pop_front();
        }
        self.rasters
            .push_back((source, width, height, mode, raster.clone()));
        Ok(raster)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{SvgCache, SvgMode, icons};

    const SAMPLE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><path fill="#ff0000" opacity="0.5" d="M0 0H20V10H0Z"/></svg>"##;

    #[test]
    fn rasterizes_color_and_mask_with_aspect_preservation_and_cache_reuse() {
        let mut cache = SvgCache::default();
        let color = cache.rasterize(SAMPLE, 20, 20, SvgMode::Color).unwrap();
        assert!(color.colored);
        // Top row is letterboxed empty space.
        assert_eq!(&color.rgba[..4], &[0, 0, 0, 0]);
        let center = (10 * 20 + 10) * 4;
        assert_eq!(color.rgba[center], 255);
        assert_eq!(color.rgba[center + 1], 0);
        assert_eq!(color.rgba[center + 2], 0);
        assert!((127..=128).contains(&color.rgba[center + 3]));

        let cached = cache.rasterize(SAMPLE, 20, 20, SvgMode::Color).unwrap();
        assert!(Arc::ptr_eq(&color.rgba, &cached.rgba));

        let mask = cache.rasterize(SAMPLE, 20, 20, SvgMode::Mask).unwrap();
        assert!(!mask.colored);
        assert_eq!(&mask.rgba[center..center + 3], &[255, 255, 255]);
        assert!((127..=128).contains(&mask.rgba[center + 3]));
        assert_eq!(cache.trees.len(), 1);

        assert!(cache.rasterize(SAMPLE, 0, 20, SvgMode::Mask).is_err());
        assert!(cache.rasterize(SAMPLE, 513, 20, SvgMode::Mask).is_err());
        assert!(cache.rasterize("not-svg", 20, 20, SvgMode::Mask).is_err());
    }

    #[test]
    fn builtin_svg_icons_and_logo_rasterize_cleanly() {
        let mut cache = SvgCache::default();
        let logo = cache
            .rasterize(icons::APP_LOGO, 32, 32, SvgMode::Color)
            .unwrap();
        assert!(logo.rgba.as_chunks::<4>().0.iter().any(|px| px[3] > 0));

        for svg in [
            icons::SEARCH,
            icons::PLUS,
            icons::CLOSE,
            icons::MINIMIZE,
            icons::MAXIMIZE,
            icons::RESTORE,
            icons::TERMINAL,
            icons::COPY,
        ] {
            let raster = cache.rasterize(svg, 24, 24, SvgMode::Mask).unwrap();
            assert!(raster.rgba.as_chunks::<4>().0.iter().any(|px| px[3] > 0));
        }
    }
}
