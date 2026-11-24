//! WCAG 2.1 minimum contrast enforcement for terminal foreground colors.

use lune_terminal::Rgb;

use crate::gpu::linear_colors;

use super::box_drawing;

pub(super) const MIN_CONTRAST: f32 = 1.0;
pub(super) const MAX_CONTRAST: f32 = 21.0;

/// Returns `true` for geometric and Powerline characters whose exact foreground
/// color must match adjacent cell backgrounds.
pub(super) fn is_exempt_character(character: char) -> bool {
    box_drawing::is_supported(character) || matches!(character, '\u{e0a0}'..='\u{e0d7}')
}

/// Computes WCAG 2.1 relative luminance in `[0.0, 1.0]`.
pub(super) fn relative_luminance(color: Rgb) -> f32 {
    let lut = linear_colors();
    linear_luminance([
        lut[color.red as usize],
        lut[color.green as usize],
        lut[color.blue as usize],
    ])
}

/// Computes WCAG 2.1 contrast ratio in `[1.0, 21.0]`.
pub(super) fn contrast_ratio(a: Rgb, b: Rgb) -> f32 {
    contrast_from_luminance(relative_luminance(a), relative_luminance(b))
}

/// Adjusts `fg` so its WCAG 2.1 contrast ratio against `bg` is at least
/// `min_contrast`, preserving hue and saturation whenever possible.
///
/// Leaves `fg` unchanged when `min_contrast <= 1.0`, when `fg == bg`, or when
/// the pair already meets the threshold.
pub(super) fn enforce(fg: Rgb, bg: Rgb, min_contrast: f32) -> Rgb {
    if min_contrast <= MIN_CONTRAST || fg == bg {
        return fg;
    }
    let lut = linear_colors();
    let fg_lin = [
        lut[fg.red as usize],
        lut[fg.green as usize],
        lut[fg.blue as usize],
    ];
    let l_fg = linear_luminance(fg_lin);
    let l_bg = relative_luminance(bg);
    if contrast_from_luminance(l_fg, l_bg) >= min_contrast {
        return fg;
    }

    let cr_white = contrast_from_luminance(1.0, l_bg);
    let cr_black = contrast_from_luminance(0.0, l_bg);
    let can_lighten = cr_white >= min_contrast;
    let can_darken = cr_black >= min_contrast;

    let lighten = if l_fg >= l_bg {
        can_lighten || (!can_darken && cr_white >= cr_black)
    } else {
        !can_darken && (can_lighten || cr_white >= cr_black)
    };

    if lighten {
        if !can_lighten {
            return Rgb {
                red: 255,
                green: 255,
                blue: 255,
            };
        }
        let target_l = (min_contrast * (l_bg + 0.05) - 0.05).clamp(0.0, 1.0);
        let adjusted = adjust_lighter(fg_lin, l_fg, target_l);
        nudge_for_rounding(linear_to_rgb(adjusted), bg, min_contrast, true)
    } else {
        if !can_darken {
            return Rgb {
                red: 0,
                green: 0,
                blue: 0,
            };
        }
        let target_l = ((l_bg + 0.05) / min_contrast - 0.05).clamp(0.0, 1.0);
        let adjusted = adjust_darker(fg_lin, l_fg, target_l);
        nudge_for_rounding(linear_to_rgb(adjusted), bg, min_contrast, false)
    }
}

fn adjust_lighter(color: [f32; 3], l_color: f32, target_l: f32) -> [f32; 3] {
    if target_l >= 1.0 {
        return [1.0, 1.0, 1.0];
    }
    let max_ch = color[0].max(color[1]).max(color[2]);
    if max_ch <= 0.0 || l_color <= 0.0 {
        return [target_l, target_l, target_l];
    }
    // Scale up to full saturation first so dark hues stay vivid before mixing toward white.
    let vivid_scale = 1.0 / max_ch;
    let l_vivid = (l_color * vivid_scale).min(1.0);
    if target_l <= l_vivid {
        let scale = target_l / l_color;
        return [
            (color[0] * scale).min(1.0),
            (color[1] * scale).min(1.0),
            (color[2] * scale).min(1.0),
        ];
    }
    let vivid = [
        (color[0] * vivid_scale).min(1.0),
        (color[1] * vivid_scale).min(1.0),
        (color[2] * vivid_scale).min(1.0),
    ];
    let denom = (1.0 - l_vivid).max(1e-6);
    let t = ((target_l - l_vivid) / denom).clamp(0.0, 1.0);
    [
        vivid[0] + t * (1.0 - vivid[0]),
        vivid[1] + t * (1.0 - vivid[1]),
        vivid[2] + t * (1.0 - vivid[2]),
    ]
}

fn adjust_darker(color: [f32; 3], l_color: f32, target_l: f32) -> [f32; 3] {
    if target_l <= 0.0 || l_color <= 0.0 {
        return [0.0, 0.0, 0.0];
    }
    let scale = (target_l / l_color).clamp(0.0, 1.0);
    [color[0] * scale, color[1] * scale, color[2] * scale]
}

fn nudge_for_rounding(mut color: Rgb, bg: Rgb, min_contrast: f32, lighten: bool) -> Rgb {
    // At most 2 sRGB steps handle 8-bit quantization without distorting hue.
    for _ in 0..2 {
        if contrast_ratio(color, bg) >= min_contrast {
            break;
        }
        if lighten {
            color.red = color.red.saturating_add(1);
            color.green = color.green.saturating_add(1);
            color.blue = color.blue.saturating_add(1);
        } else {
            color.red = color.red.saturating_sub(1);
            color.green = color.green.saturating_sub(1);
            color.blue = color.blue.saturating_sub(1);
        }
    }
    color
}

fn linear_luminance(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

fn contrast_from_luminance(a: f32, b: f32) -> f32 {
    let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

fn linear_to_srgb_u8(linear: f32) -> u8 {
    let linear = linear.clamp(0.0, 1.0);
    let srgb = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (srgb * 255.0).round().clamp(0.0, 255.0) as u8
}

fn linear_to_rgb(rgb: [f32; 3]) -> Rgb {
    Rgb {
        red: linear_to_srgb_u8(rgb[0]),
        green: linear_to_srgb_u8(rgb[1]),
        blue: linear_to_srgb_u8(rgb[2]),
    }
}

#[cfg(test)]
mod tests {
    use lune_terminal::Rgb;

    use super::{contrast_ratio, enforce, is_exempt_character};

    const BLACK: Rgb = Rgb {
        red: 0,
        green: 0,
        blue: 0,
    };
    const WHITE: Rgb = Rgb {
        red: 255,
        green: 255,
        blue: 255,
    };

    #[test]
    fn disabled_identical_and_sufficient_pairs_are_unchanged() {
        let dark_blue = Rgb {
            red: 0,
            green: 0,
            blue: 120,
        };
        assert_eq!(enforce(dark_blue, BLACK, 1.0), dark_blue);
        assert_eq!(enforce(dark_blue, dark_blue, 4.5), dark_blue);
        assert_eq!(enforce(WHITE, BLACK, 7.0), WHITE);
    }

    #[test]
    fn lifts_dark_blue_on_dark_bg_while_preserving_blue_hue() {
        let bg = Rgb {
            red: 24,
            green: 24,
            blue: 36,
        };
        let dark_blue = Rgb {
            red: 18,
            green: 24,
            blue: 110,
        };
        assert!(contrast_ratio(dark_blue, bg) < 4.5);
        let adjusted = enforce(dark_blue, bg, 4.5);
        assert!(contrast_ratio(adjusted, bg) >= 4.5);
        assert!(adjusted.blue > adjusted.green && adjusted.blue > adjusted.red);
    }

    #[test]
    fn darkens_pale_yellow_on_light_bg_while_preserving_warm_hue() {
        let bg = Rgb {
            red: 250,
            green: 250,
            blue: 250,
        };
        let pale_yellow = Rgb {
            red: 240,
            green: 230,
            blue: 110,
        };
        assert!(contrast_ratio(pale_yellow, bg) < 4.5);
        let adjusted = enforce(pale_yellow, bg, 4.5);
        assert!(contrast_ratio(adjusted, bg) >= 4.5);
        assert!(adjusted.red > adjusted.blue && adjusted.green > adjusted.blue);
    }

    #[test]
    fn unreachable_contrast_picks_best_achievable_endpoint() {
        let mid_gray = Rgb {
            red: 128,
            green: 128,
            blue: 128,
        };
        let slightly_darker = Rgb {
            red: 110,
            green: 110,
            blue: 110,
        };
        let adjusted = enforce(slightly_darker, mid_gray, 21.0);
        assert_eq!(adjusted, BLACK);
    }

    #[test]
    fn geometric_and_powerline_glyphs_are_exempt_from_contrast_shift() {
        assert!(is_exempt_character('─'));
        assert!(is_exempt_character('█'));
        assert!(is_exempt_character('⠀'));
        assert!(is_exempt_character('\u{e0b0}'));
        assert!(is_exempt_character('\u{e0b4}'));
        assert!(!is_exempt_character('A'));
        assert!(!is_exempt_character('中'));
    }
}
