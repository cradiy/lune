use lune_terminal::{ColorScheme, GraphicsColors, Palette, Rgb};
use winit::window::Theme;

pub(super) fn palette(theme: Theme) -> (Palette, ColorScheme) {
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
    let level = [0, 95, 135, 175, 215, 255];
    let indexed = std::array::from_fn(|index| match index {
        0..16 => rgb(ansi[index]),
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
        selection_foreground: Some(rgb(foreground)),
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
