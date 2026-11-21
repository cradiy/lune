use std::sync::atomic::{AtomicU64, Ordering};

use crate::Color;

const ENTRIES: usize = 259;
const WORDS: usize = ENTRIES.div_ceil(64);
static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

/// A resolved color with eight bits per channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GraphicsColors {
    pub selection_background: Option<Rgb>,
    pub selection_foreground: Option<Rgb>,
    pub cursor_depends_on_cell: bool,
    pub cursor_text: Option<Rgb>,
    pub url_hover: Option<Rgb>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PaletteEntry {
    Indexed(u8),
    Foreground,
    Background,
    Cursor,
}

impl PaletteEntry {
    #[inline]
    const fn index(self) -> usize {
        match self {
            Self::Indexed(index) => index as usize,
            Self::Foreground => 256,
            Self::Background => 257,
            Self::Cursor => 258,
        }
    }
}

/// Theme colors and retained runtime overrides in fixed storage. Mutators report
/// visible color changes; an override equal to its base is still retained.
#[derive(Clone, Debug)]
pub struct Palette {
    base: [Rgb; ENTRIES],
    current: [Rgb; ENTRIES],
    overrides: [u64; WORDS],
    graphics: GraphicsColors,
    revision: u64,
}

impl Palette {
    /// The caller supplies the full indexed palette and default role colors.
    pub fn new(indexed: [Rgb; 256], foreground: Rgb, background: Rgb, cursor: Rgb) -> Self {
        let base = colors(indexed, foreground, background, cursor);
        Self {
            base,
            current: base,
            overrides: [0; WORDS],
            graphics: GraphicsColors::default(),
            revision: NEXT_REVISION.fetch_add(1, Ordering::Relaxed),
        }
    }

    #[inline]
    pub fn color(&self, entry: PaletteEntry) -> Rgb {
        self.current[entry.index()]
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn graphics_color(&self, index: u8) -> Rgb {
        let foreground = self.color(PaletteEntry::Foreground);
        let background = self.color(PaletteEntry::Background);
        match index {
            0..=247 => self.color(PaletteEntry::Indexed(index)),
            248 => background,
            249 | 255 => foreground,
            250 => self.graphics.selection_background.unwrap_or(foreground),
            251 => self.graphics.selection_foreground.unwrap_or(background),
            252 if self.graphics_cursor_uses_cell_colors() => foreground,
            252 => self.color(PaletteEntry::Cursor),
            253 if self.graphics_cursor_uses_cell_colors() => background,
            253 => self.graphics.cursor_text.unwrap_or(background),
            254 => self.graphics.url_hover.unwrap_or(foreground),
        }
    }

    fn graphics_cursor_uses_cell_colors(&self) -> bool {
        self.graphics.cursor_depends_on_cell && !self.is_overridden(PaletteEntry::Cursor)
    }

    fn cursor_override_changes_graphics_colors(&self, cursor: Rgb) -> bool {
        self.graphics.cursor_depends_on_cell
            && (cursor != self.color(PaletteEntry::Foreground)
                || self
                    .graphics
                    .cursor_text
                    .is_some_and(|text| text != self.color(PaletteEntry::Background)))
    }

    pub fn set_graphics_colors(&mut self, colors: GraphicsColors) -> bool {
        let changed = self.graphics != colors;
        self.graphics = colors;
        self.record_change(changed)
    }

    fn record_change(&mut self, changed: bool) -> bool {
        if changed {
            self.revision = NEXT_REVISION.fetch_add(1, Ordering::Relaxed);
        }
        changed
    }

    /// Compares the resolved indexed colors and default foreground and background.
    pub fn same_text_colors(&self, other: &Self) -> bool {
        let end = PaletteEntry::Cursor.index();
        self.current[..end] == other.current[..end]
    }

    pub fn base_color(&self, entry: PaletteEntry) -> Rgb {
        self.base[entry.index()]
    }

    pub fn is_overridden(&self, entry: PaletteEntry) -> bool {
        let index = entry.index();
        self.overrides[index / 64] & (1 << (index % 64)) != 0
    }

    #[inline]
    pub fn resolve(&self, color: Color, default: PaletteEntry) -> Rgb {
        match color {
            Color::Default => self.color(default),
            Color::Indexed(index) => self.current[usize::from(index)],
            Color::Rgb { red, green, blue } => Rgb { red, green, blue },
        }
    }

    pub fn set_override(&mut self, entry: PaletteEntry, color: Rgb) -> bool {
        let index = entry.index();
        let bit = 1 << (index % 64);
        let word = &mut self.overrides[index / 64];
        let previous = *word;
        *word |= bit;
        let changed = self.current[index] != color
            || (index == PaletteEntry::Cursor.index()
                && previous & bit == 0
                && self.cursor_override_changes_graphics_colors(color));
        self.current[index] = color;
        self.record_change(changed)
    }

    pub fn reset(&mut self, entry: PaletteEntry) -> bool {
        let index = entry.index();
        let bit = 1 << (index % 64);
        let word = &mut self.overrides[index / 64];
        let previous = *word;
        *word &= !bit;
        let changed = self.current[index] != self.base[index]
            || (index == PaletteEntry::Cursor.index()
                && previous & bit != 0
                && self.cursor_override_changes_graphics_colors(self.current[index]));
        self.current[index] = self.base[index];
        self.record_change(changed)
    }

    /// Restores the indexed table while retaining default role overrides.
    pub fn reset_indexed(&mut self) -> bool {
        self.reset_words(4, false)
    }

    pub fn reset_all(&mut self) -> bool {
        let cursor = PaletteEntry::Cursor;
        let cursor_changed = self.is_overridden(cursor)
            && self.cursor_override_changes_graphics_colors(self.color(cursor));
        self.reset_words(WORDS, cursor_changed)
    }

    /// Replaces theme colors while preserving every runtime override.
    pub fn set_base(
        &mut self,
        indexed: [Rgb; 256],
        foreground: Rgb,
        background: Rgb,
        cursor: Rgb,
    ) -> bool {
        self.set_base_colors(colors(indexed, foreground, background, cursor))
    }

    pub(crate) fn set_base_from(&mut self, theme: &Self) -> bool {
        let changed = self.set_base_colors(theme.base);
        self.set_graphics_colors(theme.graphics) || changed
    }

    fn set_base_colors(&mut self, base: [Rgb; ENTRIES]) -> bool {
        let mut changed = false;
        for (index, (&color, current)) in base.iter().zip(&mut self.current).enumerate() {
            if self.overrides[index / 64] & (1 << (index % 64)) == 0 {
                changed |= *current != color;
                *current = color;
            }
        }
        self.base = base;
        self.record_change(changed)
    }

    fn reset_words(&mut self, end: usize, mut changed: bool) -> bool {
        for (word_index, word) in self.overrides[..end].iter_mut().enumerate() {
            let mut bits = std::mem::take(word);
            while bits != 0 {
                let index = word_index * 64 + bits.trailing_zeros() as usize;
                changed |= self.current[index] != self.base[index];
                self.current[index] = self.base[index];
                bits &= bits - 1;
            }
        }
        self.record_change(changed)
    }
}

fn colors(indexed: [Rgb; 256], foreground: Rgb, background: Rgb, cursor: Rgb) -> [Rgb; ENTRIES] {
    let mut colors = [Rgb::default(); ENTRIES];
    colors[..256].copy_from_slice(&indexed);
    colors[256] = foreground;
    colors[257] = background;
    colors[258] = cursor;
    colors
}
