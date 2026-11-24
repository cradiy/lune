use std::fs;
use std::path::Path;

use lune_terminal::{ColorScheme, GraphicsColors, Palette, Rgb};
use winit::window::Theme;

use super::config::parse_settings;

pub(super) mod system;

#[derive(PartialEq, Eq)]
pub(super) enum Preference {
    System,
    Fixed(Theme),
    Custom(Theme, Box<Colors>),
}

#[derive(Default, PartialEq, Eq)]
pub(super) struct Colors {
    ansi: [Option<u32>; 16],
    foreground: Option<u32>,
    background: Option<u32>,
    cursor: Option<u32>,
    selection_background: Option<u32>,
    selection_foreground: Option<u32>,
}

impl Preference {
    pub(super) fn load(
        directory: &Path,
        name: &str,
        errors: &mut Vec<String>,
    ) -> Result<Self, &'static str> {
        match name {
            "system" => Ok(Self::System),
            "dark" => Ok(Self::Fixed(Theme::Dark)),
            "light" => Ok(Self::Fixed(Theme::Light)),
            name if !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) =>
            {
                let file = format!("{name}.conf");
                let text =
                    fs::read_to_string(directory.join("themes").join(&file)).map_err(|error| {
                        log::debug!("Could not read {file}: {error}");
                        "could not read the selected theme file"
                    })?;
                Ok(Self::parse(&file, &text, errors))
            }
            _ => Err("theme names must contain only letters, digits, hyphens, or underscores"),
        }
    }

    pub(super) fn resolve(&self, system: Theme) -> Theme {
        match self {
            Self::System => system,
            Self::Fixed(theme) | Self::Custom(theme, _) => *theme,
        }
    }

    pub(super) fn palette(&self, selected: Theme) -> (Palette, ColorScheme) {
        match self {
            Self::Custom(_, colors) => make_palette(selected, Some(colors)),
            _ => palette(selected),
        }
    }

    fn parse(file: &str, text: &str, errors: &mut Vec<String>) -> Self {
        let mut variant = Theme::Dark;
        let mut colors = Colors::default();
        parse_settings(file, text, errors, |key, value| {
            if key == "variant" {
                variant = match value {
                    "dark" => Theme::Dark,
                    "light" => Theme::Light,
                    _ => return Err("variant must be light or dark"),
                };
            } else {
                colors.set(key, value)?;
            }
            Ok(())
        });
        Self::Custom(variant, Box::new(colors))
    }
}

impl Colors {
    fn set(&mut self, key: &str, value: &str) -> Result<(), &'static str> {
        let target = match key {
            "foreground" => &mut self.foreground,
            "background" => &mut self.background,
            "cursor" => &mut self.cursor,
            "selection-background" => &mut self.selection_background,
            "selection-foreground" => &mut self.selection_foreground,
            key => {
                let index = key
                    .strip_prefix("color")
                    .and_then(|index| index.parse::<usize>().ok())
                    .ok_or("unknown theme setting")?;
                self.ansi
                    .get_mut(index)
                    .ok_or("ANSI color index must be between 0 and 15")?
            }
        };
        *target = Some(
            value
                .strip_prefix('#')
                .filter(|hex| hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .ok_or("expected a color in #RRGGBB format")?,
        );
        Ok(())
    }
}

pub(super) fn palette(theme: Theme) -> (Palette, ColorScheme) {
    make_palette(theme, None)
}

fn make_palette(theme: Theme, colors: Option<&Colors>) -> (Palette, ColorScheme) {
    let (ansi, foreground, background, cursor, selection, scheme) = match theme {
        Theme::Dark => (
            [
                0x25272c, 0xe58385, 0x98c99a, 0xe1bf85, 0x8eb5e6, 0xc39bd8, 0x88c5c7, 0xd4d1cb,
                0x727681, 0xffabab, 0xb6e0b7, 0xf8d698, 0xb2cefa, 0xe0b7f2, 0xa4e0df, 0xf5f1e9,
            ],
            0xe9e6df,
            0x181a1f,
            0xb1c7eb,
            0x384b66,
            ColorScheme::Dark,
        ),
        Theme::Light => (
            [
                0x303239, 0xae3d44, 0x466b40, 0x826214, 0x345f9b, 0x784b90, 0x326d71, 0xd4d1cb,
                0x666b76, 0xa93d47, 0x477242, 0x7e611e, 0x365f9b, 0x814b92, 0x286d72, 0xedeae2,
            ],
            0x252832,
            0xf6f4ee,
            0x395e8e,
            0xc6d6ee,
            ColorScheme::Light,
        ),
    };
    let foreground = colors
        .and_then(|colors| colors.foreground)
        .unwrap_or(foreground);
    let background = colors
        .and_then(|colors| colors.background)
        .unwrap_or(background);
    let cursor = colors.and_then(|colors| colors.cursor).unwrap_or(cursor);
    let selection = colors
        .and_then(|colors| colors.selection_background)
        .unwrap_or(selection);
    let selection_foreground = colors
        .and_then(|colors| colors.selection_foreground)
        .unwrap_or(foreground);
    let level = [0, 95, 135, 175, 215, 255];
    let indexed = std::array::from_fn(|index| match index {
        0..16 => rgb(colors
            .and_then(|colors| colors.ansi[index])
            .unwrap_or(ansi[index])),
        16..232 => {
            let n = index - 16;
            Rgb {
                red: level[n / 36],
                green: level[n / 6 % 6],
                blue: level[n % 6],
            }
        }
        _ => {
            let value = 8 + (index as u8 - 232) * 10;
            Rgb {
                red: value,
                green: value,
                blue: value,
            }
        }
    });
    let mut palette = Palette::new(indexed, rgb(foreground), rgb(background), rgb(cursor));
    palette.set_graphics_colors(GraphicsColors {
        selection_background: Some(rgb(selection)),
        selection_foreground: Some(rgb(selection_foreground)),
        ..GraphicsColors::default()
    });
    (palette, scheme)
}

fn rgb(hex: u32) -> Rgb {
    Rgb {
        red: (hex >> 16) as u8,
        green: (hex >> 8) as u8,
        blue: hex as u8,
    }
}

#[cfg(test)]
mod tests {
    use lune_terminal::PaletteEntry;

    use super::*;

    #[test]
    fn custom_theme_inherits_the_variant_and_keeps_valid_color_overrides() {
        let mut errors = Vec::new();
        let preference = Preference::parse(
            "fixture.conf",
            "# Theme fixture\nbackground = #d0e0f0\ncolor1 = #A123b4\ncolor1 = #+12345\nvariant = light\n",
            &mut errors,
        );
        assert_eq!(errors.len(), 1);
        let (palette, scheme) = preference.palette(preference.resolve(Theme::Dark));
        let (light, _) = super::palette(Theme::Light);
        assert_eq!(scheme, ColorScheme::Light);
        assert_eq!(palette.color(PaletteEntry::Background), rgb(0xd0e0f0));
        assert_eq!(palette.color(PaletteEntry::Indexed(1)), rgb(0xa123b4));
        assert_eq!(
            palette.color(PaletteEntry::Indexed(6)),
            light.color(PaletteEntry::Indexed(6))
        );
        assert_eq!(palette.graphics_color(251), light.graphics_color(251));
    }
}
