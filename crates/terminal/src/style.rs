use lune_vt::Parameter;

use crate::HyperlinkId;

/// An unresolved terminal color. Palette indices and defaults retain their
/// identity so palette and theme changes can be applied when rendering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb {
        red: u8,
        green: u8,
        blue: u8,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Underline {
    #[default]
    None = 0,
    Single = 1,
    Double = 2,
    Curly = 3,
    Dotted = 4,
    Dashed = 5,
}

/// Character rendition independent of cell storage and rendering policy.
/// Bold and faint are independent. Inverse retains the original foreground and
/// background; color resolution and visual attribute effects belong to rendering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub foreground: Color,
    pub background: Color,
    /// Default follows the displayed foreground. Explicit colors are independent of inverse video.
    pub underline_color: Color,
    /// Optional OSC 8 hyperlink handle attached to written cells. Zero means none.
    pub hyperlink: HyperlinkId,
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub underline: Underline,
    pub blink: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub strikethrough: bool,
}

impl Style {
    /// Applies SGR parameters atomically without allocating. Empty parameters or
    /// omitted top-level values reset rendition. Returns false for an unsupported
    /// value or malformed group, preserving the complete original style.
    ///
    /// Supports 0-5, 7-9, 21-25, 27-29, 30-37, 39, 40-47, 49, 59, 90-97,
    /// 100-107, and extended colors through 38/48/58. Indexed colors accept
    /// `38;5;n` or `38:5:n`; RGB accepts `38;2;r;g;b`, `38:2:r:g:b`, and
    /// `38:2:space:r:g:b` with an empty or zero color-space identifier.
    /// The same forms apply to background and underline colors. All indices and RGB
    /// components must be explicit values through 255. `4:0` through `4:5`
    /// select underline styles. The caller validates the CSI header.
    #[inline]
    pub fn apply_sgr(&mut self, parameters: &[Parameter]) -> bool {
        let mut next = *self;
        if parameters.is_empty() {
            next = Self::default();
        }
        let mut offset = 0;
        while offset < parameters.len() {
            let parameter = parameters[offset];
            if parameter.is_subparameter {
                return false;
            }
            let code = parameter.value.unwrap_or(0);
            let tail = &parameters[offset + 1..];
            if tail.first().is_some_and(|mode| mode.is_subparameter) {
                let Some(consumed) = next.apply_colon_group(code, tail) else {
                    return false;
                };
                offset += consumed + 1;
                continue;
            }
            offset += 1;
            match code {
                0 => next = Self::default(),
                1 => next.bold = true,
                2 => next.faint = true,
                3 => next.italic = true,
                4 => next.underline = Underline::Single,
                5 => next.blink = true,
                7 => next.inverse = true,
                8 => next.hidden = true,
                9 => next.strikethrough = true,
                21 => next.underline = Underline::Double,
                22 => {
                    next.bold = false;
                    next.faint = false;
                }
                23 => next.italic = false,
                24 => next.underline = Underline::None,
                25 => next.blink = false,
                27 => next.inverse = false,
                28 => next.hidden = false,
                29 => next.strikethrough = false,
                30..=37 => next.foreground = Color::Indexed((code - 30) as u8),
                38 | 48 | 58 => {
                    let Some(color) = semicolon_color(parameters, &mut offset) else {
                        return false;
                    };
                    *next.extended_color(code) = color;
                }
                39 => next.foreground = Color::Default,
                40..=47 => next.background = Color::Indexed((code - 40) as u8),
                49 => next.background = Color::Default,
                59 => next.underline_color = Color::Default,
                90..=97 => next.foreground = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => next.background = Color::Indexed((code - 100 + 8) as u8),
                _ => return false,
            }
        }
        *self = next;
        true
    }

    fn apply_colon_group(&mut self, code: u16, tail: &[Parameter]) -> Option<usize> {
        match code {
            4 => {
                self.underline = match tail[0].value? {
                    0 => Underline::None,
                    1 => Underline::Single,
                    2 => Underline::Double,
                    3 => Underline::Curly,
                    4 => Underline::Dotted,
                    5 => Underline::Dashed,
                    _ => return None,
                };
                Some(1)
            }
            38 | 48 | 58 => {
                let length = tail.iter().take_while(|mode| mode.is_subparameter).count();
                *self.extended_color(code) = colon_color(&tail[..length])?;
                Some(length)
            }
            _ => None,
        }
    }

    fn extended_color(&mut self, code: u16) -> &mut Color {
        match code {
            38 => &mut self.foreground,
            48 => &mut self.background,
            58 => &mut self.underline_color,
            _ => unreachable!(),
        }
    }
}

fn byte(parameter: &Parameter) -> Option<u8> {
    u8::try_from(parameter.value?).ok()
}

fn rgb(parameters: &[Parameter]) -> Option<Color> {
    Some(Color::Rgb {
        red: byte(&parameters[0])?,
        green: byte(&parameters[1])?,
        blue: byte(&parameters[2])?,
    })
}

fn colon_color(parameters: &[Parameter]) -> Option<Color> {
    match parameters {
        [mode, index] if mode.value == Some(5) => Some(Color::Indexed(byte(index)?)),
        [mode, components @ ..] if mode.value == Some(2) && components.len() == 3 => {
            rgb(components)
        }
        [mode, space, components @ ..]
            if mode.value == Some(2)
                && matches!(space.value, None | Some(0))
                && components.len() == 3 =>
        {
            rgb(components)
        }
        _ => None,
    }
}

fn semicolon_color(parameters: &[Parameter], offset: &mut usize) -> Option<Color> {
    let mode = parameters.get(*offset)?;
    let length = match mode.value? {
        5 => 2,
        2 => 4,
        _ => return None,
    };
    let group = parameters.get(*offset..*offset + length)?;
    // A colon attached to the last component also belongs to this color group.
    if group.iter().any(|p| p.is_subparameter)
        || parameters
            .get(*offset + length)
            .is_some_and(|p| p.is_subparameter)
    {
        return None;
    }
    *offset += length;
    if length == 2 {
        Some(Color::Indexed(byte(&group[1])?))
    } else {
        rgb(&group[1..])
    }
}
