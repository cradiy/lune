use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

pub(super) const MIN_FONT_SIZE: f32 = 8.0;
pub(super) const MAX_FONT_SIZE: f32 = 32.0;

pub(super) struct Config {
    pub(super) font_family: Option<String>,
    pub(super) font_size_points: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size_points: 15.0,
        }
    }
}

impl Config {
    pub(super) fn load() -> Self {
        let directory = absolute_env_path("XDG_CONFIG_HOME")
            .or_else(|| absolute_env_path("HOME").map(|home| home.join(".config")));
        let Some(path) = directory.map(|directory| directory.join("lune/lune.conf")) else {
            return Self::default();
        };
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(error) if error.kind() == ErrorKind::NotFound => Self::default(),
            Err(error) => {
                log::warn!("Could not read lune.conf: {error}");
                Self::default()
            }
        }
    }

    fn parse(text: &str) -> Self {
        let mut config = Self::default();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let result = line
                .split_once('=')
                .ok_or("expected key = value")
                .and_then(|(key, value)| config.set(key.trim(), value.trim()));
            if let Err(error) = result {
                log::warn!("lune.conf line {}: {error}", index + 1);
            }
        }
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
            _ => return Err("unknown setting"),
        }
        Ok(())
    }
}

fn absolute_env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn invalid_settings_preserve_valid_font_overrides() {
        let config = Config::parse(
            "# Font settings\r\nfont-family = Fixture Mono\r\nfont-size=19.5\r\n\
             font-family=\r\nunknown-key=true\r\nfont-size=NaN\r\nfont-size=33\r\n",
        );
        assert_eq!(config.font_family.as_deref(), Some("Fixture Mono"));
        assert_eq!(config.font_size_points, 19.5);
    }
}
