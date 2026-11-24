use std::env;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

pub const HELP_TEXT: &str = "\
Usage: lune [OPTIONS] [-- COMMAND [ARGS...]]

Options:
  -d, --working-directory <DIR>  Start the initial session in DIR
  -T, --title <TITLE>            Set the initial window title
      --class, --app-id <ID>     Set the Wayland application ID (default: lune)
  -c, --config <PATH>            Load configuration from PATH
  -e, --command <CMD> [ARGS...]  Run CMD instead of the default shell
  -h, --help                     Print this help message and exit
  -V, --version                  Print version information and exit
";

pub const VERSION_TEXT: &str = concat!("lune ", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchOptions {
    pub working_directory: Option<PathBuf>,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub config: Option<PathBuf>,
    pub command: Option<Vec<OsString>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliAction {
    Run(LaunchOptions),
    Help,
    Version,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliError {
    message: String,
}

impl CliError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

pub fn parse_env() -> Result<CliAction, CliError> {
    parse(env::args_os().skip(1))
}

pub fn parse<I>(args: I) -> Result<CliAction, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut iter = args.into_iter();
    let mut options = LaunchOptions::default();

    while let Some(arg) = iter.next() {
        let Some(text) = arg.to_str() else {
            return Err(CliError::new(format!(
                "unexpected argument: {}",
                arg.to_string_lossy()
            )));
        };

        match text {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "-d" | "--working-directory" => {
                let value = next_value(&mut iter, text)?;
                options.working_directory = Some(PathBuf::from(value));
            }
            "-T" | "--title" => {
                let value = next_string(&mut iter, text)?;
                options.title = Some(sanitize_title(&value));
            }
            "--class" | "--app-id" => {
                let value = next_string(&mut iter, text)?;
                options.app_id = Some(validate_app_id(&value)?);
            }
            "-c" | "--config" => {
                let value = next_value(&mut iter, text)?;
                options.config = Some(PathBuf::from(value));
            }
            "-e" | "--command" | "--" => {
                let command: Vec<OsString> = iter.collect();
                if command.is_empty() {
                    return Err(CliError::new(format!("{text} requires a command to run")));
                }
                options.command = Some(command);
                break;
            }
            _ => {
                if let Some((flag, value)) = split_long_option(text) {
                    match flag {
                        "--working-directory" => {
                            options.working_directory = Some(PathBuf::from(value));
                        }
                        "--title" => {
                            options.title = Some(sanitize_title(value));
                        }
                        "--class" | "--app-id" => {
                            options.app_id = Some(validate_app_id(value)?);
                        }
                        "--config" => {
                            options.config = Some(PathBuf::from(value));
                        }
                        _ => {
                            return Err(CliError::new(format!("unknown option: {flag}")));
                        }
                    }
                } else if text.starts_with('-') {
                    return Err(CliError::new(format!("unknown option: {text}")));
                } else {
                    return Err(CliError::new(format!(
                        "unexpected argument: {text} (use -e or -- to run a command)"
                    )));
                }
            }
        }
    }

    Ok(CliAction::Run(options))
}

fn split_long_option(text: &str) -> Option<(&str, &str)> {
    let (flag, value) = text.split_once('=')?;
    flag.starts_with("--").then_some((flag, value))
}

fn next_value(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<OsString, CliError> {
    iter.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CliError::new(format!("{flag} requires a value")))
}

fn next_string(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<String, CliError> {
    let value = next_value(iter, flag)?;
    value
        .into_string()
        .map_err(|_| CliError::new(format!("{flag} value must be valid UTF-8")))
}

fn sanitize_title(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(128)
        .collect()
}

fn validate_app_id(value: &str) -> Result<String, CliError> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(CliError::new(
            "application ID must be non-empty without control characters",
        ));
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::{CliAction, LaunchOptions, parse};

    fn os(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_launch_options_and_trailing_command() {
        let action = parse(os(&[
            "--working-directory",
            "/tmp",
            "--title=Build\x07 Log",
            "--app-id",
            "lune.dev",
            "-c",
            "/etc/lune.conf",
            "-e",
            "sh",
            "-c",
            "echo --help",
        ]))
        .unwrap();
        assert_eq!(
            action,
            CliAction::Run(LaunchOptions {
                working_directory: Some(PathBuf::from("/tmp")),
                title: Some("Build Log".into()),
                app_id: Some("lune.dev".into()),
                config: Some(PathBuf::from("/etc/lune.conf")),
                command: Some(os(&["sh", "-c", "echo --help"])),
            })
        );
    }

    #[test]
    fn parses_double_dash_command_and_help_version() {
        assert_eq!(parse(os(&["--help"])).unwrap(), CliAction::Help);
        assert_eq!(parse(os(&["-V"])).unwrap(), CliAction::Version);
        let action = parse(os(&["-d", ".", "--", "ls", "-la"])).unwrap();
        assert_eq!(
            action,
            CliAction::Run(LaunchOptions {
                working_directory: Some(PathBuf::from(".")),
                command: Some(os(&["ls", "-la"])),
                ..LaunchOptions::default()
            })
        );
    }

    #[test]
    fn rejects_invalid_flags_and_missing_values() {
        for bad in [
            &["--unknown"][..],
            &["--working-directory"][..],
            &["-c"][..],
            &["-e"][..],
            &["--"][..],
            &["--app-id="][..],
            &["positional"][..],
        ] {
            assert!(parse(os(bad)).is_err(), "{bad:?}");
        }
    }
}
