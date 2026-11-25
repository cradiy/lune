use winit::window::Theme;

use super::Colors;

#[derive(Clone, Copy)]
pub(in super::super) struct BuiltinPreset {
    pub(super) name: &'static str,
    pub(in super::super) variant: Theme,
    pub(super) foreground: u32,
    pub(super) background: u32,
    pub(super) cursor: u32,
    pub(super) selection_background: u32,
    pub(super) selection_foreground: u32,
    pub(super) ansi: [u32; 16],
}

impl BuiltinPreset {
    pub(super) fn to_colors(self) -> Colors {
        Colors {
            ansi: self.ansi.map(Some),
            foreground: Some(self.foreground),
            background: Some(self.background),
            cursor: Some(self.cursor),
            selection_background: Some(self.selection_background),
            selection_foreground: Some(self.selection_foreground),
        }
    }
}

pub(super) const BUILTIN_PRESETS: &[BuiltinPreset] = &[
    BuiltinPreset {
        name: "catppuccin-mocha",
        variant: Theme::Dark,
        foreground: 0xcdd6f4,
        background: 0x1e1e2e,
        cursor: 0xf5e0dc,
        selection_background: 0x585b70,
        selection_foreground: 0xcdd6f4,
        ansi: [
            0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
            0x585b70, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
        ],
    },
    BuiltinPreset {
        name: "catppuccin-latte",
        variant: Theme::Light,
        foreground: 0x4c4f69,
        background: 0xeff1f5,
        cursor: 0xdc8a78,
        selection_background: 0xacb0be,
        selection_foreground: 0x4c4f69,
        ansi: [
            0x5c5f77, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xacb0be,
            0x6c6f85, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xbcc0cc,
        ],
    },
    BuiltinPreset {
        name: "tokyo-night",
        variant: Theme::Dark,
        foreground: 0xc0caf5,
        background: 0x1a1b26,
        cursor: 0xc0caf5,
        selection_background: 0x33467c,
        selection_foreground: 0xc0caf5,
        ansi: [
            0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
            0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
        ],
    },
    BuiltinPreset {
        name: "tokyo-night-day",
        variant: Theme::Light,
        foreground: 0x3760bf,
        background: 0xe1e2e7,
        cursor: 0x3760bf,
        selection_background: 0xb7c1e3,
        selection_foreground: 0x3760bf,
        ansi: [
            0xb4b5b9, 0xf52a65, 0x587539, 0x8c6c3e, 0x2e7de9, 0x9854f1, 0x007197, 0x6172b0,
            0xa1a6c5, 0xf52a65, 0x587539, 0x8c6c3e, 0x2e7de9, 0x9854f1, 0x007197, 0x3760bf,
        ],
    },
    BuiltinPreset {
        name: "gruvbox-dark",
        variant: Theme::Dark,
        foreground: 0xebdbb2,
        background: 0x282828,
        cursor: 0xebdbb2,
        selection_background: 0x504945,
        selection_foreground: 0xebdbb2,
        ansi: [
            0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
            0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
        ],
    },
    BuiltinPreset {
        name: "gruvbox-light",
        variant: Theme::Light,
        foreground: 0x3c3836,
        background: 0xfbf1c7,
        cursor: 0x3c3836,
        selection_background: 0xd5c4a1,
        selection_foreground: 0x3c3836,
        ansi: [
            0xfbf1c7, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0x7c6f64,
            0x928374, 0x9d0006, 0x79740e, 0xb57614, 0x076678, 0x8f3f71, 0x427b58, 0x3c3836,
        ],
    },
    BuiltinPreset {
        name: "nord",
        variant: Theme::Dark,
        foreground: 0xd8dee9,
        background: 0x2e3440,
        cursor: 0xd8dee9,
        selection_background: 0x4c566a,
        selection_foreground: 0xeceff4,
        ansi: [
            0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
            0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
        ],
    },
    BuiltinPreset {
        name: "dracula",
        variant: Theme::Dark,
        foreground: 0xf8f8f2,
        background: 0x282a36,
        cursor: 0xf8f8f2,
        selection_background: 0x44475a,
        selection_foreground: 0xf8f8f2,
        ansi: [
            0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
            0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
        ],
    },
    BuiltinPreset {
        name: "rose-pine",
        variant: Theme::Dark,
        foreground: 0xe0def4,
        background: 0x191724,
        cursor: 0x524f67,
        selection_background: 0x403d52,
        selection_foreground: 0xe0def4,
        ansi: [
            0x26233a, 0xeb6f92, 0x31748f, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xebbcba, 0xe0def4,
            0x6e6a86, 0xeb6f92, 0x31748f, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xebbcba, 0xe0def4,
        ],
    },
    BuiltinPreset {
        name: "rose-pine-dawn",
        variant: Theme::Light,
        foreground: 0x575279,
        background: 0xfaf4ed,
        cursor: 0xcecacd,
        selection_background: 0xdfdad9,
        selection_foreground: 0x575279,
        ansi: [
            0xf2e9e1, 0xb4637a, 0x286983, 0xea9d34, 0x56949f, 0x907aa9, 0xd7827e, 0x575279,
            0x9893a5, 0xb4637a, 0x286983, 0xea9d34, 0x56949f, 0x907aa9, 0xd7827e, 0x575279,
        ],
    },
    BuiltinPreset {
        name: "kanagawa",
        variant: Theme::Dark,
        foreground: 0xdcd7ba,
        background: 0x1f1f28,
        cursor: 0xc8c093,
        selection_background: 0x2d4f67,
        selection_foreground: 0xc8c093,
        ansi: [
            0x090618, 0xc34043, 0x76946a, 0xc0a36e, 0x7e9cd8, 0x957fb8, 0x6a9589, 0xc8c093,
            0x727169, 0xe82424, 0x98bb6c, 0xe6c384, 0x7fb4ca, 0x938aa9, 0x7aa89f, 0xdcd7ba,
        ],
    },
    BuiltinPreset {
        name: "one-dark",
        variant: Theme::Dark,
        foreground: 0xabb2bf,
        background: 0x282c34,
        cursor: 0x528bff,
        selection_background: 0x3e4451,
        selection_foreground: 0xabb2bf,
        ansi: [
            0x282c34, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf,
            0x5c6370, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
        ],
    },
];

pub(in super::super) fn find(name: &str) -> Option<BuiltinPreset> {
    BUILTIN_PRESETS.iter().copied().find(|p| p.name == name)
}
