use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::theme::Preference;

pub(super) const MIN_FONT_SIZE: f32 = 8.0;
pub(super) const MAX_FONT_SIZE: f32 = 32.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ClipboardPolicy {
    #[default]
    Allow,
    Deny,
}

pub(super) struct Config {
    pub(super) font_family: Option<String>,
    pub(super) font_size_points: f32,
    pub(super) font_ligatures: bool,
    pub(super) theme: Preference,
    pub(super) clipboard_write: ClipboardPolicy,
    pub(super) clipboard_read: ClipboardPolicy,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size_points: 15.0,
            font_ligatures: true,
            theme: Preference::System,
            clipboard_write: ClipboardPolicy::Allow,
            clipboard_read: ClipboardPolicy::Deny,
        }
    }
}

impl Config {
    pub(super) fn load() -> Self {
        let directory = absolute_env_path("XDG_CONFIG_HOME")
            .or_else(|| absolute_env_path("HOME").map(|home| home.join(".config")))
            .map(|directory| directory.join("lune"));
        let Some(directory) = directory else {
            return Self::default();
        };
        match fs::read_to_string(directory.join("lune.conf")) {
            Ok(text) => Self::parse(&text, &directory),
            Err(error) if error.kind() == ErrorKind::NotFound => Self::default(),
            Err(error) => {
                log::warn!("Could not read lune.conf: {error}");
                Self::default()
            }
        }
    }

    fn parse(text: &str, directory: &Path) -> Self {
        let mut config = Self::default();
        parse_settings("lune.conf", text, |key, value| {
            if key == "theme" {
                config.theme = Preference::load(directory, value)?;
                Ok(())
            } else {
                config.set(key, value)
            }
        });
        config
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
            "clipboard-write" => {
                self.clipboard_write = parse_clipboard_policy(value)?;
            }
            "clipboard-read" => {
                self.clipboard_read = parse_clipboard_policy(value)?;
            }
            _ => return Err("unknown setting"),
        }
        Ok(())
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

pub(super) fn parse_settings(
    file: &str,
    text: &str,
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
            log::warn!("{file} line {}: {error}", index + 1);
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

    use super::Config;

    #[test]
    fn invalid_settings_preserve_valid_font_overrides() {
        let config = Config::parse(
            "# Font settings\r\nfont-family = Fixture Mono\r\nfont-size=19.5\r\n\
             clipboard-write = deny\r\nclipboard-read = allow\r\nclipboard-read = invalid\r\n\
             font-family=\r\nunknown-key=true\r\nfont-size=NaN\r\nfont-size=33\r\n",
            Path::new(""),
        );
        assert_eq!(config.font_family.as_deref(), Some("Fixture Mono"));
        assert_eq!(config.font_size_points, 19.5);
        assert_eq!(config.clipboard_write, super::ClipboardPolicy::Deny);
        assert_eq!(config.clipboard_read, super::ClipboardPolicy::Allow);
    }
}
