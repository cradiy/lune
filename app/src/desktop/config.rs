use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use lune_terminal::{CursorShape, HistoryLimits};

use super::shortcuts::{self, Keybind};
use super::theme::Preference;

pub(super) mod watch;

pub(super) const MIN_FONT_SIZE: f32 = 8.0;
pub(super) const MAX_FONT_SIZE: f32 = 32.0;
pub(super) const MAX_WINDOW_PADDING: u32 = 64;
pub(super) const DEFAULT_SCROLLBACK_LINES: usize = 10_000;
pub(super) const MAX_SCROLLBACK_LINES: usize = 1_000_000;

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
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ClipboardPolicy {
    #[default]
    Allow,
    Deny,
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
    pub(super) font_family: Option<String>,
    pub(super) font_size_points: f32,
    pub(super) font_ligatures: bool,
    pub(super) theme: Preference,
    pub(super) background_opacity: f32,
    pub(super) window_padding: u32,
    pub(super) scrollback_lines: usize,
    pub(super) cursor_style: CursorShape,
    pub(super) cursor_blink: bool,
    pub(super) cursor_animation: bool,
    pub(super) shell: Option<Vec<OsString>>,
    pub(super) clipboard_write: ClipboardPolicy,
    pub(super) clipboard_read: ClipboardPolicy,
    pub(super) notifications: NotificationPolicy,
    pub(super) keybinds: Vec<Keybind>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size_points: 15.0,
            font_ligatures: true,
            theme: Preference::System,
            background_opacity: 1.0,
            window_padding: 0,
            scrollback_lines: DEFAULT_SCROLLBACK_LINES,
            cursor_style: CursorShape::Block,
            cursor_blink: true,
            cursor_animation: true,
            shell: None,
            clipboard_write: ClipboardPolicy::Allow,
            clipboard_read: ClipboardPolicy::Deny,
            notifications: NotificationPolicy::Unfocused,
            keybinds: Vec::new(),
        }
    }
}

impl Config {
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
            } else {
                config.set(key, value)
            }
        });
        errors.extend(theme_errors);
        Loaded { config, errors }
    }

    fn set(&mut self, key: &str, value: &str) -> Result<(), &'static str> {
        match key {
            "font-family" => {
                if value.is_empty() || value.chars().any(char::is_control) {
                    return Err("font-family must be a font family name");
                }
                self.font_family = (!value.eq_ignore_ascii_case("monospace")).then(|| value.into());
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
            "background-opacity" => {
                self.background_opacity = value
                    .parse::<f32>()
                    .ok()
                    .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
                    .ok_or("background-opacity must be between 0.0 and 1.0")?;
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
        format!(" Config error: {first} ")
    } else {
        format!(" Config error: {first} (+{extra} more) ")
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
             background-opacity = 0.85\r\nbackground-opacity = 1.5\r\nbackground-opacity = NaN\r\n\
             window-padding = 12\r\nwindow-padding = 999\r\n\
             scrollback-lines = 50000\r\nscrollback-lines = -1\r\n\
             cursor-style = bar\r\ncursor-style = triangle\r\n\
             cursor-blink = false\r\ncursor-animation = false\r\n\
             shell = /usr/bin/zsh -l\r\nshell = \"unterminated\r\n\
             keybind = alt+v=split_right\r\nkeybind = bad\r\n\
             clipboard-write = deny\r\nclipboard-read = allow\r\nclipboard-read = invalid\r\n\
             notifications = always\r\nnotifications = bad\r\n\
             font-family=\r\nunknown-key=true\r\nfont-size=NaN\r\nfont-size=33\r\n",
            Path::new(""),
        );
        let config = loaded.config;
        assert_eq!(config.font_family.as_deref(), Some("Fixture Mono"));
        assert_eq!(config.font_size_points, 19.5);
        assert_eq!(config.background_opacity, 0.85);
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
        assert_eq!(config.keybinds.len(), 1);
        assert_eq!(config.clipboard_write, super::ClipboardPolicy::Deny);
        assert_eq!(config.clipboard_read, super::ClipboardPolicy::Allow);
        assert_eq!(config.notifications, super::NotificationPolicy::Always);
        assert_eq!(loaded.errors.len(), 13);
        assert_eq!(
            format_errors(&loaded.errors).as_deref(),
            Some(
                " Config error: lune.conf:5: background-opacity must be between 0.0 and 1.0 (+12 more) "
            )
        );
    }
}
