use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::theme::Preference;

pub(super) mod watch;

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

pub(super) const MIN_FONT_SIZE: f32 = 8.0;
pub(super) const MAX_FONT_SIZE: f32 = 32.0;

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
    pub(super) clipboard_write: ClipboardPolicy,
    pub(super) clipboard_read: ClipboardPolicy,
    pub(super) notifications: NotificationPolicy,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size_points: 15.0,
            font_ligatures: true,
            theme: Preference::System,
            background_opacity: 1.0,
            clipboard_write: ClipboardPolicy::Allow,
            clipboard_read: ClipboardPolicy::Deny,
            notifications: NotificationPolicy::Unfocused,
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
             clipboard-write = deny\r\nclipboard-read = allow\r\nclipboard-read = invalid\r\n\
             notifications = always\r\nnotifications = bad\r\n\
             font-family=\r\nunknown-key=true\r\nfont-size=NaN\r\nfont-size=33\r\n",
            Path::new(""),
        );
        let config = loaded.config;
        assert_eq!(config.font_family.as_deref(), Some("Fixture Mono"));
        assert_eq!(config.font_size_points, 19.5);
        assert_eq!(config.background_opacity, 0.85);
        assert_eq!(config.clipboard_write, super::ClipboardPolicy::Deny);
        assert_eq!(config.clipboard_read, super::ClipboardPolicy::Allow);
        assert_eq!(config.notifications, super::NotificationPolicy::Always);
        assert_eq!(loaded.errors.len(), 8);
        assert_eq!(
            format_errors(&loaded.errors).as_deref(),
            Some(
                " Config error: lune.conf:5: background-opacity must be between 0.0 and 1.0 (+7 more) "
            )
        );
    }
}
