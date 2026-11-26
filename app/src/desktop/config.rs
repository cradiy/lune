use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use lune_terminal::{CursorShape, HistoryLimits};

use super::bg_image::{
    BackgroundImageConfig, BackgroundImageFit, BackgroundImagePosition,
    DEFAULT_BACKGROUND_IMAGE_OPACITY,
};
use super::contrast::{MAX_CONTRAST, MIN_CONTRAST};
use super::font::{FontConfig, FontFeature, MetricAdjustment};
use super::scrollbar::ScrollbarPolicy;
use super::shortcuts::{self, Keybind};
use super::theme::Preference;

pub(super) mod watch;

pub(super) const MIN_FONT_SIZE: f32 = 8.0;
pub(super) const MAX_FONT_SIZE: f32 = 32.0;
pub(super) const MIN_UNFOCUSED_SPLIT_OPACITY: f32 = 0.15;
pub(super) const DEFAULT_UNFOCUSED_SPLIT_OPACITY: f32 = 0.75;
pub(super) const MAX_WINDOW_PADDING: u32 = 64;
pub(super) const DEFAULT_SCROLLBACK_LINES: usize = 10_000;
pub(super) const MAX_SCROLLBACK_LINES: usize = 1_000_000;
pub(super) const DEFAULT_TERM: &str = "xterm-256color";
pub(super) const DEFAULT_TERM_PROGRAM: &str = "Lune";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ConfigSource {
    Default,
    File(PathBuf),
}

impl ConfigSource {
    pub(super) fn from_option(path: Option<PathBuf>) -> Self {
        match path {
            Some(path) => {
                let resolved = if path.is_absolute() {
                    path
                } else {
                    env::current_dir()
                        .map(|cwd| cwd.join(&path))
                        .unwrap_or(path)
                };
                Self::File(resolved)
            }
            None => Self::Default,
        }
    }

    pub(super) fn watch_target(&self) -> Option<(PathBuf, OsString)> {
        match self {
            Self::Default => Config::directory().map(|dir| (dir, OsString::from("lune.conf"))),
            Self::File(path) => {
                let parent = path.parent()?.to_path_buf();
                let file_name = path.file_name()?.to_os_string();
                Some((parent, file_name))
            }
        }
    }

    pub(super) fn custom_path(&self) -> Option<&Path> {
        match self {
            Self::Default => None,
            Self::File(path) => Some(path.as_path()),
        }
    }

    pub(super) fn target_path(&self) -> Option<PathBuf> {
        match self {
            Self::Default => Config::directory().map(|dir| dir.join("lune.conf")),
            Self::File(path) => Some(path.clone()),
        }
    }

    /// Persists runtime settings to the active configuration file atomically,
    /// preserving existing comments, formatting, and unrelated keys.
    pub(super) fn save(&self, settings: &SavedSettings) -> std::io::Result<(PathBuf, usize)> {
        let path = self.target_path().ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::NotFound,
                "could not resolve configuration directory",
            )
        })?;
        let (existing, existed) = match fs::read_to_string(&path) {
            Ok(text) => (text, true),
            Err(error) if error.kind() == ErrorKind::NotFound => (String::new(), false),
            Err(error) => return Err(error),
        };
        let entries = settings.entries();
        let (merged, changed) = merge_config_text(&existing, &entries);
        if existed && changed == 0 {
            return Ok((path, 0));
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("lune.conf");
        let tmp = path.with_file_name(format!(".{file_name}.tmp.{}", std::process::id()));
        fs::write(&tmp, merged)?;
        if let Err(error) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        Ok((path, changed))
    }
}

pub(super) struct SavedSettings {
    pub(super) mode: TerminalMode,
    pub(super) tab_position: TabPosition,
    pub(super) window_decorations: bool,
    pub(super) background_blur: bool,
    pub(super) window_controls: WindowControlsPolicy,
    pub(super) command_blocks: bool,
    pub(super) scrollbar: ScrollbarPolicy,
    pub(super) copy_on_select: bool,
    pub(super) font_size_points: f32,
    pub(super) theme_name: String,
}

impl SavedSettings {
    fn entries(&self) -> Vec<(&'static str, String, bool)> {
        let defaults = Config::default();
        let mode_str = match self.mode {
            TerminalMode::Classic => "classic",
            TerminalMode::Modern => "modern",
        };
        let tab_pos_str = match self.tab_position {
            TabPosition::Top => "top",
            TabPosition::Left => "left",
        };
        let controls_str = match self.window_controls {
            WindowControlsPolicy::Auto => "auto",
            WindowControlsPolicy::Show => "show",
            WindowControlsPolicy::Hide => "hide",
        };
        let scrollbar_str = match self.scrollbar {
            ScrollbarPolicy::Auto => "auto",
            ScrollbarPolicy::Always => "always",
            ScrollbarPolicy::Never => "never",
        };
        let font_size_str = format_points(self.font_size_points);
        let default_font_size = (self.font_size_points - defaults.font_size_points).abs() < 0.01;
        vec![
            ("mode", mode_str.to_owned(), self.mode == defaults.mode),
            (
                "tab-position",
                tab_pos_str.to_owned(),
                self.tab_position == defaults.tab_position,
            ),
            (
                "window-decorations",
                self.window_decorations.to_string(),
                self.window_decorations == defaults.window_decorations,
            ),
            (
                "background-blur",
                self.background_blur.to_string(),
                self.background_blur == defaults.background_blur,
            ),
            (
                "window-controls",
                controls_str.to_owned(),
                self.window_controls == defaults.window_controls,
            ),
            (
                "command-blocks",
                self.command_blocks.to_string(),
                self.command_blocks == defaults.command_blocks,
            ),
            (
                "scrollbar",
                scrollbar_str.to_owned(),
                self.scrollbar == defaults.scrollbar,
            ),
            (
                "copy-on-select",
                self.copy_on_select.to_string(),
                self.copy_on_select == defaults.copy_on_select,
            ),
            ("font-size", font_size_str, default_font_size),
            (
                "theme",
                self.theme_name.clone(),
                self.theme_name == defaults.theme.config_name(),
            ),
        ]
    }
}

fn format_points(points: f32) -> String {
    let rounded = (points * 10.0).round() / 10.0;
    if (rounded.fract()).abs() < 0.01 {
        format!("{}", rounded as i32)
    } else {
        format!("{rounded:.1}")
    }
}

fn merge_config_text(existing: &str, entries: &[(&str, String, bool)]) -> (String, usize) {
    let mut lines: Vec<String> = existing
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect();

    // Find the last occurrence of each managed key so duplicate declarations
    // still resolve to the newly saved value.
    let mut last_line_for_key: Vec<Option<usize>> = vec![None; entries.len()];
    for (line_idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((raw_key, _)) = trimmed.split_once('=') else {
            continue;
        };
        let key = raw_key.trim();
        if let Some(entry_idx) = entries.iter().position(|(name, _, _)| *name == key) {
            last_line_for_key[entry_idx] = Some(line_idx);
        }
    }

    let mut changed = 0usize;
    let mut to_append = Vec::new();
    for (entry_idx, (key, value, is_default)) in entries.iter().enumerate() {
        let replacement = format!("{key} = {value}");
        if let Some(line_idx) = last_line_for_key[entry_idx] {
            let current_val = lines[line_idx]
                .split_once('=')
                .map(|(_, rhs)| rhs.trim())
                .unwrap_or("");
            if current_val != value.as_str() {
                lines[line_idx] = replacement;
                changed += 1;
            }
        } else if !is_default {
            to_append.push(replacement);
            changed += 1;
        }
    }

    if !to_append.is_empty() {
        lines.extend(to_append);
    }

    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    (out, changed)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum TerminalMode {
    #[default]
    Classic,
    Modern,
}

impl TerminalMode {
    pub(super) fn parse(value: &str) -> Result<Self, &'static str> {
        if value.eq_ignore_ascii_case("classic") || value.eq_ignore_ascii_case("traditional") {
            Ok(Self::Classic)
        } else if value.eq_ignore_ascii_case("modern") {
            Ok(Self::Modern)
        } else {
            Err("mode must be classic or modern")
        }
    }

    pub(super) const fn toggle(self) -> Self {
        match self {
            Self::Classic => Self::Modern,
            Self::Modern => Self::Classic,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum TabPosition {
    #[default]
    Top,
    Left,
}

impl TabPosition {
    pub(super) fn parse(value: &str) -> Result<Self, &'static str> {
        if value.eq_ignore_ascii_case("top") {
            Ok(Self::Top)
        } else if value.eq_ignore_ascii_case("left") || value.eq_ignore_ascii_case("sidebar") {
            Ok(Self::Left)
        } else {
            Err("tab-position must be top or left")
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ClipboardPolicy {
    #[default]
    Allow,
    Deny,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum WindowControlsPolicy {
    #[default]
    Auto,
    Show,
    Hide,
}

impl WindowControlsPolicy {
    fn parse(value: &str) -> Result<Self, &'static str> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "true" | "show" | "always" => Ok(Self::Show),
            "false" | "hide" | "never" | "none" => Ok(Self::Hide),
            _ => Err("window-controls must be auto, true, or false"),
        }
    }

    pub(super) fn cycle(self) -> Self {
        match self {
            Self::Auto => Self::Show,
            Self::Show => Self::Hide,
            Self::Hide => Self::Auto,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum NotificationPolicy {
    #[default]
    Unfocused,
    Always,
    Deny,
}

#[derive(PartialEq)]
pub(super) struct Config {
    pub(super) mode: TerminalMode,
    pub(super) tab_position: TabPosition,
    pub(super) window_decorations: bool,
    pub(super) window_controls: WindowControlsPolicy,
    pub(super) command_blocks: bool,
    pub(super) scrollbar: ScrollbarPolicy,
    pub(super) font_family: Option<String>,
    pub(super) font_family_bold: Option<String>,
    pub(super) font_family_italic: Option<String>,
    pub(super) font_family_bold_italic: Option<String>,
    pub(super) font_size_points: f32,
    pub(super) font_ligatures: bool,
    pub(super) font_features: Vec<FontFeature>,
    pub(super) font_thicken: bool,
    pub(super) adjust_cell_height: MetricAdjustment,
    pub(super) adjust_cell_width: MetricAdjustment,
    pub(super) adjust_baseline: MetricAdjustment,
    pub(super) minimum_contrast: f32,
    pub(super) theme: Preference,
    pub(super) background_opacity: f32,
    pub(super) background_blur: bool,
    pub(super) background_image: Option<PathBuf>,
    pub(super) background_image_opacity: f32,
    pub(super) background_image_fit: BackgroundImageFit,
    pub(super) background_image_position: BackgroundImagePosition,
    pub(super) unfocused_split_opacity: f32,
    pub(super) window_padding: u32,
    pub(super) scrollback_lines: usize,
    pub(super) cursor_style: CursorShape,
    pub(super) cursor_blink: bool,
    pub(super) cursor_animation: bool,
    pub(super) shell: Option<Vec<OsString>>,
    pub(super) shell_integration: bool,
    pub(super) term: String,
    pub(super) term_program: String,
    pub(super) env: Vec<(String, String)>,
    pub(super) confirm_close: bool,
    pub(super) confirm_paste: bool,
    pub(super) copy_on_select: bool,
    pub(super) clipboard_write: ClipboardPolicy,
    pub(super) clipboard_read: ClipboardPolicy,
    pub(super) notifications: NotificationPolicy,
    pub(super) keybinds: Vec<Keybind>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: TerminalMode::Classic,
            tab_position: TabPosition::Top,
            window_decorations: true,
            window_controls: WindowControlsPolicy::Auto,
            command_blocks: true,
            scrollbar: ScrollbarPolicy::Auto,
            font_family: None,
            font_family_bold: None,
            font_family_italic: None,
            font_family_bold_italic: None,
            font_size_points: 15.0,
            font_ligatures: true,
            font_features: Vec::new(),
            font_thicken: false,
            adjust_cell_height: MetricAdjustment::default(),
            adjust_cell_width: MetricAdjustment::default(),
            adjust_baseline: MetricAdjustment::default(),
            minimum_contrast: MIN_CONTRAST,
            theme: Preference::System,
            background_opacity: 1.0,
            background_blur: false,
            background_image: None,
            background_image_opacity: DEFAULT_BACKGROUND_IMAGE_OPACITY,
            background_image_fit: BackgroundImageFit::Cover,
            background_image_position: BackgroundImagePosition::Center,
            unfocused_split_opacity: DEFAULT_UNFOCUSED_SPLIT_OPACITY,
            window_padding: 0,
            scrollback_lines: DEFAULT_SCROLLBACK_LINES,
            cursor_style: CursorShape::Block,
            cursor_blink: true,
            cursor_animation: true,
            shell: None,
            shell_integration: true,
            term: DEFAULT_TERM.to_owned(),
            term_program: DEFAULT_TERM_PROGRAM.to_owned(),
            env: Vec::new(),
            confirm_close: true,
            confirm_paste: true,
            copy_on_select: false,
            clipboard_write: ClipboardPolicy::Allow,
            clipboard_read: ClipboardPolicy::Deny,
            notifications: NotificationPolicy::Unfocused,
            keybinds: Vec::new(),
        }
    }
}

impl Config {
    pub(super) fn font_config(&self) -> FontConfig {
        FontConfig {
            family: self.font_family.clone(),
            family_bold: self.font_family_bold.clone(),
            family_italic: self.font_family_italic.clone(),
            family_bold_italic: self.font_family_bold_italic.clone(),
            ligatures: self.font_ligatures,
            features: self.font_features.clone(),
            thicken: self.font_thicken,
            adjust_cell_height: self.adjust_cell_height,
            adjust_cell_width: self.adjust_cell_width,
            adjust_baseline: self.adjust_baseline,
        }
    }

    pub(super) fn background_image_config(&self) -> BackgroundImageConfig {
        BackgroundImageConfig {
            path: self.background_image.clone(),
            opacity: self.background_image_opacity,
            fit: self.background_image_fit,
            position: self.background_image_position,
        }
    }

    pub(super) fn history_limits(&self) -> HistoryLimits {
        if self.scrollback_lines == 0 {
            return HistoryLimits {
                max_rows: 0,
                max_bytes: 0,
            };
        }
        let max_bytes = self
            .scrollback_lines
            .saturating_mul(3200)
            .clamp(32 << 20, 512 << 20);
        HistoryLimits {
            max_rows: self.scrollback_lines,
            max_bytes,
        }
    }
}

pub(super) struct Loaded {
    pub(super) config: Config,
    pub(super) errors: Vec<String>,
}

impl Config {
    pub(super) fn directory() -> Option<PathBuf> {
        absolute_env_path("XDG_CONFIG_HOME")
            .or_else(|| absolute_env_path("HOME").map(|home| home.join(".config")))
            .map(|directory| directory.join("lune"))
    }

    pub(super) fn load(source: &ConfigSource) -> Loaded {
        match Self::load_checked(source) {
            Ok(loaded) => loaded,
            Err(error) => Loaded {
                config: Self::default(),
                errors: vec![error],
            },
        }
    }

    pub(super) fn load_checked(source: &ConfigSource) -> Result<Loaded, String> {
        match source {
            ConfigSource::Default => {
                let Some(directory) = Self::directory() else {
                    return Ok(Loaded {
                        config: Self::default(),
                        errors: Vec::new(),
                    });
                };
                match fs::read_to_string(directory.join("lune.conf")) {
                    Ok(text) => Ok(Self::parse("lune.conf", &text, &directory)),
                    Err(error) if error.kind() == ErrorKind::NotFound => Ok(Loaded {
                        config: Self::default(),
                        errors: Vec::new(),
                    }),
                    Err(error) => {
                        log::warn!("Could not read lune.conf: {error}");
                        Err(format!("lune.conf: {error}"))
                    }
                }
            }
            ConfigSource::File(path) => {
                let display = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                let directory = path.parent().unwrap_or_else(|| Path::new(""));
                match fs::read_to_string(path) {
                    Ok(text) => Ok(Self::parse(&display, &text, directory)),
                    Err(error) => {
                        log::warn!("Could not read {}: {error}", path.display());
                        Err(format!("{display}: {error}"))
                    }
                }
            }
        }
    }

    fn parse(file: &str, text: &str, directory: &Path) -> Loaded {
        let mut config = Self::default();
        let mut errors = Vec::new();
        let mut theme_errors = Vec::new();
        parse_settings(file, text, &mut errors, |key, value| {
            if key == "theme" {
                let mut current_theme_errors = Vec::new();
                config.theme = Preference::load(directory, value, &mut current_theme_errors)?;
                theme_errors = current_theme_errors;
                Ok(())
            } else if key == "background-image" {
                config.background_image = resolve_image_path(directory, value)?;
                Ok(())
            } else {
                config.set(key, value)
            }
        });
        errors.extend(theme_errors);
        Loaded { config, errors }
    }

    fn set(&mut self, key: &str, value: &str) -> Result<(), &'static str> {
        match key {
            "mode" => {
                self.mode = TerminalMode::parse(value)?;
            }
            "tab-position" => {
                self.tab_position = TabPosition::parse(value)?;
            }
            "window-decorations" => {
                self.window_decorations = parse_window_decorations(value)?;
            }
            "window-controls" => {
                self.window_controls = WindowControlsPolicy::parse(value)?;
            }
            "command-blocks" => {
                self.command_blocks = value
                    .parse()
                    .map_err(|_| "command-blocks must be true or false")?;
            }
            "scrollbar" => {
                self.scrollbar = ScrollbarPolicy::parse(value)?;
            }
            "font-family" => {
                self.font_family = parse_font_family(value)?;
            }
            "font-family-bold" => {
                self.font_family_bold = parse_font_family(value)?;
            }
            "font-family-italic" => {
                self.font_family_italic = parse_font_family(value)?;
            }
            "font-family-bold-italic" => {
                self.font_family_bold_italic = parse_font_family(value)?;
            }
            "font-size" => {
                self.font_size_points = value
                    .parse::<f32>()
                    .ok()
                    .filter(|value| (MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(value))
                    .ok_or("font-size must be between 8 and 32 points")?;
            }
            "font-ligatures" => {
                self.font_ligatures = value
                    .parse()
                    .map_err(|_| "font-ligatures must be true or false")?;
            }
            "font-feature" => {
                let mut parsed = Vec::new();
                for part in value.split(',') {
                    parsed.push(FontFeature::parse(part)?);
                }
                self.font_features.extend(parsed);
            }
            "font-thicken" => {
                self.font_thicken = value
                    .parse()
                    .map_err(|_| "font-thicken must be true or false")?;
            }
            "adjust-cell-height" => {
                self.adjust_cell_height = parse_metric_adjustment(value, -20..=40, -30.0..=100.0)?;
            }
            "adjust-cell-width" => {
                self.adjust_cell_width = parse_metric_adjustment(value, -10..=20, -30.0..=50.0)?;
            }
            "adjust-baseline" => {
                self.adjust_baseline = parse_metric_adjustment(value, -20..=20, -50.0..=50.0)?;
            }
            "minimum-contrast" => {
                self.minimum_contrast = value
                    .parse::<f32>()
                    .ok()
                    .filter(|value| {
                        value.is_finite() && (MIN_CONTRAST..=MAX_CONTRAST).contains(value)
                    })
                    .ok_or("minimum-contrast must be between 1.0 and 21.0")?;
            }
            "background-opacity" => {
                self.background_opacity = value
                    .parse::<f32>()
                    .ok()
                    .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
                    .ok_or("background-opacity must be between 0.0 and 1.0")?;
            }
            "background-blur" => {
                self.background_blur = value
                    .parse()
                    .map_err(|_| "background-blur must be true or false")?;
            }
            "background-image-opacity" => {
                self.background_image_opacity = value
                    .parse::<f32>()
                    .ok()
                    .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
                    .ok_or("background-image-opacity must be between 0.0 and 1.0")?;
            }
            "background-image-fit" => {
                self.background_image_fit = BackgroundImageFit::parse(value)?;
            }
            "background-image-position" => {
                self.background_image_position = BackgroundImagePosition::parse(value)?;
            }
            "unfocused-split-opacity" => {
                self.unfocused_split_opacity = value
                    .parse::<f32>()
                    .ok()
                    .filter(|value| {
                        value.is_finite() && (MIN_UNFOCUSED_SPLIT_OPACITY..=1.0).contains(value)
                    })
                    .ok_or("unfocused-split-opacity must be between 0.15 and 1.0")?;
            }
            "window-padding" => {
                self.window_padding = value
                    .parse::<u32>()
                    .ok()
                    .filter(|&value| value <= MAX_WINDOW_PADDING)
                    .ok_or("window-padding must be an integer between 0 and 64")?;
            }
            "scrollback-lines" => {
                self.scrollback_lines = value
                    .parse::<usize>()
                    .ok()
                    .filter(|&value| value <= MAX_SCROLLBACK_LINES)
                    .ok_or("scrollback-lines must be between 0 and 1000000")?;
            }
            "cursor-style" => {
                self.cursor_style = parse_cursor_style(value)?;
            }
            "cursor-blink" => {
                self.cursor_blink = value
                    .parse()
                    .map_err(|_| "cursor-blink must be true or false")?;
            }
            "cursor-animation" => {
                self.cursor_animation = value
                    .parse()
                    .map_err(|_| "cursor-animation must be true or false")?;
            }
            "shell" => {
                self.shell = Some(parse_command_words(value)?);
            }
            "shell-integration" => {
                self.shell_integration = value
                    .parse()
                    .map_err(|_| "shell-integration must be true or false")?;
            }
            "term" => {
                self.term = parse_term_name(value)?;
            }
            "term-program" => {
                self.term_program = parse_term_program(value)?;
            }
            "env" => {
                if value.is_empty() {
                    self.env.clear();
                } else {
                    let (name, val) = parse_env_pair(value)?;
                    if let Some((_, existing)) = self.env.iter_mut().find(|(k, _)| k == &name) {
                        *existing = val;
                    } else {
                        self.env.push((name, val));
                    }
                }
            }
            "confirm-close" => {
                self.confirm_close = value
                    .parse()
                    .map_err(|_| "confirm-close must be true or false")?;
            }
            "confirm-paste" => {
                self.confirm_paste = value
                    .parse()
                    .map_err(|_| "confirm-paste must be true or false")?;
            }
            "copy-on-select" => {
                self.copy_on_select = value
                    .parse()
                    .map_err(|_| "copy-on-select must be true or false")?;
            }
            "keybind" => {
                self.keybinds.push(shortcuts::parse_keybind(value)?);
            }
            "clipboard-write" => {
                self.clipboard_write = parse_clipboard_policy(value)?;
            }
            "clipboard-read" => {
                self.clipboard_read = parse_clipboard_policy(value)?;
            }
            "notifications" => {
                self.notifications = parse_notification_policy(value)?;
            }
            _ => return Err("unknown setting"),
        }
        Ok(())
    }
}

fn resolve_image_path(directory: &Path, value: &str) -> Result<Option<PathBuf>, &'static str> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err("background-image must be an image file path or none");
    }
    if value.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    if let Some(rest) = value.strip_prefix("~/") {
        let home = absolute_env_path("HOME")
            .ok_or("HOME must be an absolute path to expand '~' in background-image")?;
        return Ok(Some(home.join(rest)));
    }
    let path = PathBuf::from(value);
    if path.is_absolute() || directory.as_os_str().is_empty() {
        Ok(Some(path))
    } else {
        Ok(Some(directory.join(path)))
    }
}

fn parse_window_decorations(value: &str) -> Result<bool, &'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "show" | "auto" => Ok(true),
        "false" | "off" | "no" | "none" | "hide" => Ok(false),
        _ => Err("window-decorations must be true or false (or none)"),
    }
}

fn parse_font_family(value: &str) -> Result<Option<String>, &'static str> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err("font-family must be a font family name");
    }
    Ok((!value.eq_ignore_ascii_case("monospace")).then(|| value.into()))
}

fn parse_metric_adjustment(
    value: &str,
    pixel_range: std::ops::RangeInclusive<i32>,
    percent_range: std::ops::RangeInclusive<f32>,
) -> Result<MetricAdjustment, &'static str> {
    if let Some(raw) = value.strip_suffix('%') {
        let pct = raw
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite() && percent_range.contains(v))
            .ok_or("metric adjustment percentage is out of range")?;
        Ok(MetricAdjustment::Percent(pct))
    } else {
        let px = value
            .parse::<i32>()
            .ok()
            .filter(|v| pixel_range.contains(v))
            .ok_or("metric adjustment pixels are out of range")?;
        Ok(MetricAdjustment::Pixels(px))
    }
}

fn parse_command_words(value: &str) -> Result<Vec<OsString>, &'static str> {
    if value.chars().any(char::is_control) {
        return Err("shell must not contain control characters");
    }
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else if q == '"' && ch == '\\' {
                    let next = chars.next().ok_or("trailing backslash in shell setting")?;
                    current.push(next);
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    in_token = true;
                }
                '\\' => {
                    let next = chars.next().ok_or("trailing backslash in shell setting")?;
                    current.push(next);
                    in_token = true;
                }
                c if c.is_whitespace() => {
                    if in_token {
                        words.push(OsString::from(std::mem::take(&mut current)));
                        in_token = false;
                    }
                }
                _ => {
                    current.push(ch);
                    in_token = true;
                }
            },
        }
    }
    if quote.is_some() {
        return Err("unterminated quote in shell setting");
    }
    if in_token {
        words.push(OsString::from(current));
    }
    if words.is_empty() || words[0].is_empty() {
        return Err("shell must be a command path or executable name");
    }
    Ok(words)
}

fn parse_term_name(value: &str) -> Result<String, &'static str> {
    if value.is_empty()
        || value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err("term must be a non-empty terminal name without whitespace");
    }
    Ok(value.to_owned())
}

fn parse_term_program(value: &str) -> Result<String, &'static str> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err("term-program must be a non-empty name without control characters");
    }
    Ok(value.to_owned())
}

fn parse_env_pair(value: &str) -> Result<(String, String), &'static str> {
    let Some((raw_name, raw_val)) = value.split_once('=') else {
        return Err("env must be formatted as KEY=VALUE");
    };
    let name = raw_name.trim();
    if name.is_empty()
        || name
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || ch == '=')
    {
        return Err("env variable name must be non-empty without whitespace");
    }
    if raw_val.chars().any(char::is_control) {
        return Err("env variable value must not contain control characters");
    }
    Ok((name.to_owned(), raw_val.to_owned()))
}

fn parse_cursor_style(value: &str) -> Result<CursorShape, &'static str> {
    if value.eq_ignore_ascii_case("block") {
        Ok(CursorShape::Block)
    } else if value.eq_ignore_ascii_case("bar") || value.eq_ignore_ascii_case("beam") {
        Ok(CursorShape::Bar)
    } else if value.eq_ignore_ascii_case("underline") {
        Ok(CursorShape::Underline)
    } else {
        Err("cursor-style must be block, bar, or underline")
    }
}

fn parse_notification_policy(value: &str) -> Result<NotificationPolicy, &'static str> {
    if value.eq_ignore_ascii_case("unfocused") {
        Ok(NotificationPolicy::Unfocused)
    } else if value.eq_ignore_ascii_case("always") {
        Ok(NotificationPolicy::Always)
    } else if value.eq_ignore_ascii_case("deny") {
        Ok(NotificationPolicy::Deny)
    } else {
        Err("notifications must be unfocused, always, or deny")
    }
}

fn parse_clipboard_policy(value: &str) -> Result<ClipboardPolicy, &'static str> {
    if value.eq_ignore_ascii_case("allow") {
        Ok(ClipboardPolicy::Allow)
    } else if value.eq_ignore_ascii_case("deny") {
        Ok(ClipboardPolicy::Deny)
    } else {
        Err("clipboard policy must be allow or deny")
    }
}

pub(super) fn format_errors(errors: &[String]) -> Option<String> {
    let first = errors.first()?;
    let extra = errors.len() - 1;
    Some(if extra == 0 {
        format!("Config error: {first}")
    } else {
        format!("Config error: {first} (+{extra} more)")
    })
}

pub(super) fn parse_settings(
    file: &str,
    text: &str,
    errors: &mut Vec<String>,
    mut set: impl FnMut(&str, &str) -> Result<(), &'static str>,
) {
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let result = line
            .split_once('=')
            .ok_or("expected key = value")
            .and_then(|(key, value)| set(key.trim(), value.trim()));
        if let Err(error) = result {
            let line_number = index + 1;
            log::warn!("{file} line {line_number}: {error}");
            errors.push(format!("{file}:{line_number}: {error}"));
        }
    }
}

fn absolute_env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Config, format_errors};

    #[test]
    fn invalid_settings_preserve_valid_font_overrides() {
        let loaded = Config::parse(
            "lune.conf",
            "# Font settings\r\nfont-family = Fixture Mono\r\nfont-size=19.5\r\n\
             mode = modern\r\nmode = futuristic\r\n\
             tab-position = left\r\ntab-position = bottom\r\n\
             window-decorations = none\r\nwindow-decorations = bogus\r\n\
             window-controls = false\r\nwindow-controls = nope\r\n\
             command-blocks = false\r\ncommand-blocks = maybe\r\n\
             scrollbar = always\r\nscrollbar = maybe\r\n\
             font-family-bold = Fixture Bold\r\nfont-family-italic = Fixture Italic\r\n\
             font-family-bold-italic = Fixture Bold Italic\r\n\
             font-feature = ss01, -calt, cv02=2\r\nfont-feature = bad\r\n\
             font-thicken = true\r\nfont-thicken = maybe\r\n\
             adjust-cell-height = 10%\r\nadjust-cell-height = 200%\r\n\
             adjust-cell-width = 1\r\nadjust-cell-width = 50\r\n\
             adjust-baseline = -1\r\nadjust-baseline = 99\r\n\
             minimum-contrast = 4.5\r\nminimum-contrast = 25\r\n\
             background-image = wallpaper.webp\r\nbackground-image = \r\n\
             background-image-opacity = 0.4\r\nbackground-image-opacity = 2.0\r\n\
             background-image-fit = contain\r\nbackground-image-fit = zoom\r\n\
             background-image-position = top-right\r\nbackground-image-position = diagonal\r\n\
             background-opacity = 0.85\r\nbackground-opacity = 1.5\r\nbackground-opacity = NaN\r\n\
             background-blur = true\r\nbackground-blur = nope\r\n\
             unfocused-split-opacity = 0.6\r\nunfocused-split-opacity = 0.05\r\n\
             window-padding = 12\r\nwindow-padding = 999\r\n\
             scrollback-lines = 50000\r\nscrollback-lines = -1\r\n\
             cursor-style = bar\r\ncursor-style = triangle\r\n\
             cursor-blink = false\r\ncursor-animation = false\r\n\
             shell = /usr/bin/zsh -l\r\nshell = \"unterminated\r\n\
             shell-integration = false\r\nshell-integration = maybe\r\n\
             confirm-close = false\r\nconfirm-close = maybe\r\n\
             confirm-paste = false\r\nconfirm-paste = nope\r\n\
             copy-on-select = true\r\ncopy-on-select = nope\r\n\
             keybind = alt+v=split_right\r\nkeybind = bad\r\n\
             clipboard-write = deny\r\nclipboard-read = allow\r\nclipboard-read = invalid\r\n\
             notifications = always\r\nnotifications = bad\r\n\
             font-family=\r\nunknown-key=true\r\nfont-size=NaN\r\nfont-size=33\r\n",
            Path::new(""),
        );
        let config = loaded.config;
        assert_eq!(config.mode, super::TerminalMode::Modern);
        assert_eq!(config.tab_position, super::TabPosition::Left);
        assert!(!config.window_decorations);
        assert_eq!(config.window_controls, super::WindowControlsPolicy::Hide);
        assert!(!config.command_blocks);
        assert_eq!(config.scrollbar, super::ScrollbarPolicy::Always);
        assert_eq!(config.font_family.as_deref(), Some("Fixture Mono"));
        assert_eq!(config.font_family_bold.as_deref(), Some("Fixture Bold"));
        assert_eq!(config.font_family_italic.as_deref(), Some("Fixture Italic"));
        assert_eq!(
            config.font_family_bold_italic.as_deref(),
            Some("Fixture Bold Italic")
        );
        assert_eq!(config.font_features.len(), 3);
        assert!(config.font_thicken);
        assert_eq!(
            config.adjust_cell_height,
            super::MetricAdjustment::Percent(10.0)
        );
        assert_eq!(config.adjust_cell_width, super::MetricAdjustment::Pixels(1));
        assert_eq!(config.adjust_baseline, super::MetricAdjustment::Pixels(-1));
        assert_eq!(config.minimum_contrast, 4.5);
        assert_eq!(
            config.background_image.as_deref(),
            Some(Path::new("wallpaper.webp"))
        );
        assert_eq!(config.background_image_opacity, 0.4);
        assert_eq!(
            config.background_image_fit,
            super::BackgroundImageFit::Contain
        );
        assert_eq!(
            config.background_image_position,
            super::BackgroundImagePosition::TopRight
        );
        assert_eq!(config.font_size_points, 19.5);
        assert_eq!(config.background_opacity, 0.85);
        assert!(config.background_blur);
        assert_eq!(config.unfocused_split_opacity, 0.6);
        assert_eq!(config.window_padding, 12);
        assert_eq!(config.scrollback_lines, 50_000);
        assert_eq!(config.cursor_style, lune_terminal::CursorShape::Bar);
        assert!(!config.cursor_blink);
        assert!(!config.cursor_animation);
        assert_eq!(
            config.shell.as_deref(),
            Some(
                [
                    std::ffi::OsString::from("/usr/bin/zsh"),
                    std::ffi::OsString::from("-l"),
                ]
                .as_slice()
            )
        );
        assert!(!config.shell_integration);
        assert!(!config.confirm_close);
        assert!(!config.confirm_paste);
        assert!(config.copy_on_select);
        assert_eq!(config.keybinds.len(), 1);
        assert_eq!(config.clipboard_write, super::ClipboardPolicy::Deny);
        assert_eq!(config.clipboard_read, super::ClipboardPolicy::Allow);
        assert_eq!(config.notifications, super::NotificationPolicy::Always);
        assert_eq!(loaded.errors.len(), 35);
        assert_eq!(
            format_errors(&loaded.errors).as_deref(),
            Some("Config error: lune.conf:5: mode must be classic or modern (+34 more)")
        );
    }

    #[test]
    fn merge_and_save_preserves_comments_and_roundtrips_settings() {
        let existing = "\
# User config\n\
mode = classic\n\
keybind = ctrl+shift+k=unbind\n\
# End\n";
        let settings = super::SavedSettings {
            mode: super::TerminalMode::Modern,
            tab_position: super::TabPosition::Left,
            window_decorations: false,
            background_blur: true,
            window_controls: super::WindowControlsPolicy::Auto,
            command_blocks: true,
            scrollbar: super::ScrollbarPolicy::Always,
            copy_on_select: true,
            font_size_points: 16.0,
            theme_name: "dark".into(),
        };
        let (merged, changed) = super::merge_config_text(existing, &settings.entries());
        assert_eq!(changed, 8);
        assert!(merged.contains("# User config\n"));
        assert!(merged.contains("keybind = ctrl+shift+k=unbind\n"));
        assert!(merged.contains("# End\n"));
        assert!(merged.contains("mode = modern\n"));
        assert!(merged.contains("tab-position = left\n"));
        assert!(merged.contains("window-decorations = false\n"));
        assert!(merged.contains("background-blur = true\n"));
        assert!(!merged.contains("window-controls ="));

        let reparsed = Config::parse("lune.conf", &merged, Path::new(""));
        assert!(reparsed.errors.is_empty(), "{:?}", reparsed.errors);
        assert_eq!(reparsed.config.mode, super::TerminalMode::Modern);
        assert_eq!(reparsed.config.tab_position, super::TabPosition::Left);
        assert!(!reparsed.config.window_decorations);
        assert!(reparsed.config.background_blur);
        assert_eq!(reparsed.config.scrollbar, super::ScrollbarPolicy::Always);
        assert!(reparsed.config.copy_on_select);
        assert_eq!(reparsed.config.font_size_points, 16.0);
        assert_eq!(reparsed.config.theme.config_name(), "dark");

        // Second merge with identical settings produces zero changes.
        let (_, second_changed) = super::merge_config_text(&merged, &settings.entries());
        assert_eq!(second_changed, 0);
    }

    #[test]
    fn parses_term_term_program_and_custom_env_overrides() {
        let defaults = Config::default();
        assert_eq!(defaults.term, super::DEFAULT_TERM);
        assert_eq!(defaults.term_program, super::DEFAULT_TERM_PROGRAM);
        assert!(defaults.env.is_empty());

        let loaded = Config::parse(
            "lune.conf",
            "\
term = xterm-kitty\n\
term = invalid term\n\
term-program = CustomTerm\n\
term-program =\n\
env = FOO=old\n\
env = BAR=1\n\
env =\n\
env = SNACKS_KITTY=true\n\
env = LESS=-R --tabs=4\n\
env = SNACKS_KITTY=false\n\
env = EMPTY=\n\
env = MISSING_EQUALS\n\
env = BAD KEY=1\n",
            Path::new(""),
        );
        assert_eq!(loaded.config.term, "xterm-kitty");
        assert_eq!(loaded.config.term_program, "CustomTerm");
        assert_eq!(
            loaded.config.env,
            vec![
                ("SNACKS_KITTY".to_owned(), "false".to_owned()),
                ("LESS".to_owned(), "-R --tabs=4".to_owned()),
                ("EMPTY".to_owned(), String::new()),
            ]
        );
        assert_eq!(loaded.errors.len(), 4, "{:?}", loaded.errors);
    }
}
