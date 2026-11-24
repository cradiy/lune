//! Built-in shell integration for bash, zsh, and fish.
//!
//! Automatically wires OSC 133 semantic prompt markers (prompt/input/output/exit)
//! and OSC 7 working-directory reporting without modifying user dotfiles.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShellKind {
    Bash,
    Zsh,
    Fish,
}

const BASH_SCRIPT: &str = r#"# Lune shell integration for bash
if [ -n "${LUNE_BASH_LOGIN:-}" ]; then
    unset LUNE_BASH_LOGIN
    [ -r /etc/profile ] && . /etc/profile
    if [ -r "$HOME/.bash_profile" ]; then
        . "$HOME/.bash_profile"
    elif [ -r "$HOME/.bash_login" ]; then
        . "$HOME/.bash_login"
    elif [ -r "$HOME/.profile" ]; then
        . "$HOME/.profile"
    fi
else
    [ -r /etc/bash.bashrc ] && . /etc/bash.bashrc
    [ -r "$HOME/.bashrc" ] && . "$HOME/.bashrc"
fi

if [ -n "${LUNE_SHELL_INTEGRATION_LOADED:-}" ]; then
    return 0 2>/dev/null || exit 0
fi
LUNE_SHELL_INTEGRATION_LOADED=1

__lune_osc7_encode() {
    local LC_ALL=C str="$1" i ch out=""
    for ((i = 0; i < ${#str}; i++)); do
        ch="${str:i:1}"
        case "$ch" in
            [a-zA-Z0-9./_~-]) out+="$ch" ;;
            *) printf -v ch '%%%02X' "'$ch"; out+="$ch" ;;
        esac
    done
    printf '%s' "$out"
}

__lune_cmd_running=0
__lune_in_prompt=1

__lune_preexec() {
    if [ "$__lune_in_prompt" = 1 ]; then
        return
    fi
    case "${BASH_COMMAND:-}" in
        __lune_precmd*|*__lune_precmd*) return ;;
    esac
    if [ "$__lune_cmd_running" = 0 ]; then
        __lune_cmd_running=1
        printf '\033]133;C\033\\'
    fi
}

__lune_precmd() {
    local status="$?"
    __lune_in_prompt=1
    if [ "$__lune_cmd_running" = 1 ]; then
        printf '\033]133;D;%d\033\\' "$status"
        __lune_cmd_running=0
    fi
    printf '\033]7;file://%s%s\033\\' "${HOSTNAME:-localhost}" "$(__lune_osc7_encode "$PWD")"
    printf '\033]133;A\033\\'
    case "$PS1" in
        *'133;B'*) ;;
        *) PS1="${PS1}\[\033]133;B\033\\\]" ;;
    esac
    __lune_in_prompt=0
}

if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]]; then
    PROMPT_COMMAND+=(__lune_precmd)
elif [ -n "${PROMPT_COMMAND:-}" ]; then
    PROMPT_COMMAND="${PROMPT_COMMAND%;}; __lune_precmd"
else
    PROMPT_COMMAND="__lune_precmd"
fi

if [ -z "$(trap -p DEBUG)" ]; then
    trap '__lune_preexec' DEBUG
fi
"#;

const ZSH_ZSHENV: &str = r#"# Lune shell integration bootstrap for zsh
if [[ -n "${LUNE_ORIG_ZDOTDIR+x}" ]]; then
    __lune_user_zdotdir="$LUNE_ORIG_ZDOTDIR"
else
    __lune_user_zdotdir="$HOME"
fi

if [[ ! -o interactive ]]; then
    if [[ -n "${LUNE_ORIG_ZDOTDIR+x}" ]]; then
        export ZDOTDIR="$LUNE_ORIG_ZDOTDIR"
    else
        unset ZDOTDIR
    fi
    unset LUNE_ORIG_ZDOTDIR
fi

if [[ -r "$__lune_user_zdotdir/.zshenv" ]]; then
    builtin source "$__lune_user_zdotdir/.zshenv"
fi
"#;

const ZSH_ZPROFILE: &str = r#"# Lune shell integration zprofile forwarder
if [[ -r "${__lune_user_zdotdir:-$HOME}/.zprofile" ]]; then
    builtin source "${__lune_user_zdotdir:-$HOME}/.zprofile"
fi
"#;

const ZSH_ZSHRC: &str = r#"# Lune shell integration for zsh
if [[ -n "${LUNE_ORIG_ZDOTDIR+x}" ]]; then
    export ZDOTDIR="$LUNE_ORIG_ZDOTDIR"
else
    unset ZDOTDIR
fi
unset LUNE_ORIG_ZDOTDIR

if [[ -r "${__lune_user_zdotdir:-$HOME}/.zshrc" ]]; then
    builtin source "${__lune_user_zdotdir:-$HOME}/.zshrc"
fi
unset __lune_user_zdotdir

if [[ -n "${LUNE_SHELL_INTEGRATION_LOADED:-}" ]]; then
    return 0
fi
LUNE_SHELL_INTEGRATION_LOADED=1

autoload -Uz add-zsh-hook 2>/dev/null || true

__lune_osc7_encode() {
    emulate -L zsh
    setopt extendedglob
    local LC_ALL=C str="$1" i ch out=""
    for ((i = 1; i <= ${#str}; i++)); do
        ch="${str[i]}"
        if [[ "$ch" == [a-zA-Z0-9./_~-] ]]; then
            out+="$ch"
        else
            out+=$(printf '%%%02X' "'$ch")
        fi
    done
    print -rn -- "$out"
}

__lune_cmd_running=0

__lune_preexec() {
    __lune_cmd_running=1
    print -rn -- $'\e]133;C\e\\'
}

__lune_precmd() {
    local last_status=$?
    if (( __lune_cmd_running )); then
        print -rn -- $'\e]133;D;'"${last_status}"$'\e\\'
        __lune_cmd_running=0
    fi
    print -rn -- $'\e]7;file://'"${HOST:-localhost}$(__lune_osc7_encode "$PWD")"$'\e\\'
    print -rn -- $'\e]133;A\e\\'
    if [[ "$PS1" != *'133;B'* ]]; then
        PS1="${PS1}%{"$'\e]133;B\e\\'"%}"
    fi
}

if (( $+functions[add-zsh-hook] )); then
    add-zsh-hook precmd __lune_precmd
    add-zsh-hook preexec __lune_preexec
else
    precmd_functions+=(__lune_precmd)
    preexec_functions+=(__lune_preexec)
fi
"#;

const ZSH_ZLOGIN: &str = r#"# Lune shell integration zlogin forwarder
if [[ -r "${ZDOTDIR:-$HOME}/.zlogin" ]]; then
    builtin source "${ZDOTDIR:-$HOME}/.zlogin"
fi
"#;

const FISH_SCRIPT: &str = r#"# Lune shell integration for fish
status is-interactive; or exit
set -q LUNE_SHELL_INTEGRATION_LOADED; and exit
set -g LUNE_SHELL_INTEGRATION_LOADED 1

set -g __lune_cmd_running 0

function __lune_osc7 --on-variable PWD
    set -l host (hostname 2>/dev/null; or echo localhost)
    set -l encoded (string escape --style=url -- "$PWD" | string replace -a '%2F' '/')
    printf '\033]7;file://%s%s\033\\' "$host" "$encoded"
end

function __lune_prompt --on-event fish_prompt
    set -l last_status $status
    if test "$__lune_cmd_running" = 1
        printf '\033]133;D;%d\033\\' "$last_status"
        set -g __lune_cmd_running 0
    end
    __lune_osc7
    printf '\033]133;A\033\\'
end

function __lune_preexec --on-event fish_preexec
    set -g __lune_cmd_running 1
    printf '\033]133;C\033\\'
end

if functions -q fish_prompt; and not functions -q __lune_orig_fish_prompt
    functions -c fish_prompt __lune_orig_fish_prompt
    function fish_prompt
        __lune_orig_fish_prompt
        printf '\033]133;B\033\\'
    end
end
"#;

/// Detects a supported interactive shell from its executable path or name.
pub(super) fn detect_shell(executable: &OsStr) -> Option<ShellKind> {
    let name = Path::new(executable).file_name()?.to_str()?;
    let trimmed = name.strip_prefix('-').unwrap_or(name);
    match trimmed {
        "bash" => Some(ShellKind::Bash),
        "zsh" => Some(ShellKind::Zsh),
        "fish" => Some(ShellKind::Fish),
        _ => None,
    }
}

/// Builds a `Command` for launching a shell, automatically injecting built-in
/// shell integration when `enabled` is true, `executable` is a supported shell,
/// and `args` do not opt out of shell initialization files.
pub(super) fn build_command(executable: &OsStr, args: &[OsString], enabled: bool) -> Command {
    let mut command = Command::new(executable);
    if enabled
        && let Some(kind) = detect_shell(executable)
        && should_inject(kind, args)
    {
        match ensure_scripts() {
            Ok(root) => {
                apply(kind, args, &root, &mut command);
                return command;
            }
            Err(error) => {
                log::warn!("Could not prepare shell integration scripts: {error}");
            }
        }
    }
    command.args(args);
    command
}

fn should_inject(kind: ShellKind, args: &[OsString]) -> bool {
    for arg in args {
        let Some(text) = arg.to_str() else { continue };
        if text == "-c" || text == "--command" || text == "-s" {
            return false;
        }
        match kind {
            ShellKind::Bash => {
                if matches!(text, "--rcfile" | "--init-file" | "--norc" | "--posix") {
                    return false;
                }
            }
            ShellKind::Zsh => {
                if matches!(text, "-f" | "--no-rcs" | "--norcs") {
                    return false;
                }
            }
            ShellKind::Fish => {
                if matches!(text, "-N" | "--no-config") {
                    return false;
                }
            }
        }
    }
    true
}

fn apply(kind: ShellKind, args: &[OsString], root: &Path, command: &mut Command) {
    command.env("LUNE_SHELL_INTEGRATION", "1");
    match kind {
        ShellKind::Bash => {
            let mut is_login = false;
            let mut forwarded = Vec::with_capacity(args.len() + 2);
            for arg in args {
                if arg == "-l" || arg == "--login" {
                    is_login = true;
                } else {
                    forwarded.push(arg.clone());
                }
            }
            // Rebuild args with `--rcfile` before forwarded flags.
            let rcfile = root.join("bash").join("lune.bash");
            command.arg("--rcfile").arg(rcfile);
            command.args(forwarded);
            if is_login {
                command.env("LUNE_BASH_LOGIN", "1");
            }
        }
        ShellKind::Zsh => {
            command.args(args);
            if let Some(orig) = env::var_os("ZDOTDIR") {
                command.env("LUNE_ORIG_ZDOTDIR", orig);
            } else {
                command.env_remove("LUNE_ORIG_ZDOTDIR");
            }
            command.env("ZDOTDIR", root.join("zsh"));
        }
        ShellKind::Fish => {
            command.args(args);
            let fish_data = root.join("fish-data");
            let mut joined = OsString::from(fish_data);
            joined.push(":");
            match env::var_os("XDG_DATA_DIRS") {
                Some(existing) if !existing.is_empty() => joined.push(existing),
                _ => joined.push("/usr/local/share:/usr/share"),
            }
            command.env("XDG_DATA_DIRS", joined);
        }
    }
}

fn script_root() -> PathBuf {
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return runtime.join("lune").join("shell-integration");
    }
    if let Some(cache) = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return cache.join("lune").join("shell-integration");
    }
    // SAFETY: `geteuid` is an infallible read-only POSIX syscall with no preconditions.
    let uid = unsafe { libc::geteuid() };
    env::temp_dir()
        .join(format!("lune-{uid}"))
        .join("shell-integration")
}

fn ensure_scripts() -> io::Result<PathBuf> {
    let root = script_root();
    write_scripts_to(&root)?;
    Ok(root)
}

fn write_scripts_to(root: &Path) -> io::Result<()> {
    let bash_dir = root.join("bash");
    let zsh_dir = root.join("zsh");
    let fish_dir = root.join("fish-data").join("fish").join("vendor_conf.d");
    for dir in [root, &bash_dir, &zsh_dir, &fish_dir] {
        fs::create_dir_all(dir)?;
    }
    let _ = fs::set_permissions(root, fs::Permissions::from_mode(0o700));

    write_if_changed(&bash_dir.join("lune.bash"), BASH_SCRIPT)?;
    write_if_changed(&zsh_dir.join(".zshenv"), ZSH_ZSHENV)?;
    write_if_changed(&zsh_dir.join(".zprofile"), ZSH_ZPROFILE)?;
    write_if_changed(&zsh_dir.join(".zshrc"), ZSH_ZSHRC)?;
    write_if_changed(&zsh_dir.join(".zlogin"), ZSH_ZLOGIN)?;
    write_if_changed(&fish_dir.join("lune-shell-integration.fish"), FISH_SCRIPT)?;
    Ok(())
}

fn write_if_changed(path: &Path, content: &str) -> io::Result<()> {
    if let Ok(existing) = fs::read_to_string(path)
        && existing == content
    {
        return Ok(());
    }
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or("script");
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, content)?;
    fs::rename(tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_shells_and_respects_opt_out_flags() {
        assert_eq!(detect_shell(OsStr::new("/bin/bash")), Some(ShellKind::Bash));
        assert_eq!(
            detect_shell(OsStr::new("/usr/bin/zsh")),
            Some(ShellKind::Zsh)
        );
        assert_eq!(detect_shell(OsStr::new("fish")), Some(ShellKind::Fish));
        assert_eq!(detect_shell(OsStr::new("-zsh")), Some(ShellKind::Zsh));
        assert_eq!(detect_shell(OsStr::new("/bin/sh")), None);

        assert!(should_inject(ShellKind::Bash, &[OsString::from("-l")]));
        assert!(!should_inject(ShellKind::Bash, &[OsString::from("--norc")]));
        assert!(!should_inject(
            ShellKind::Bash,
            &[OsString::from("-c"), OsString::from("echo hi")]
        ));
        assert!(!should_inject(ShellKind::Zsh, &[OsString::from("-f")]));
        assert!(!should_inject(
            ShellKind::Fish,
            &[OsString::from("--no-config")]
        ));
    }

    #[test]
    fn writes_integration_scripts_and_prepares_shell_commands() {
        let dir = env::temp_dir().join(format!("lune-shell-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        write_scripts_to(&dir).unwrap();

        assert!(dir.join("bash/lune.bash").is_file());
        assert!(dir.join("zsh/.zshenv").is_file());
        assert!(dir.join("zsh/.zshrc").is_file());
        assert!(
            dir.join("fish-data/fish/vendor_conf.d/lune-shell-integration.fish")
                .is_file()
        );

        let mut bash_cmd = Command::new("/bin/bash");
        apply(
            ShellKind::Bash,
            &[OsString::from("-l")],
            &dir,
            &mut bash_cmd,
        );
        let bash_args: Vec<_> = bash_cmd.get_args().collect();
        assert_eq!(bash_args[0], "--rcfile");
        assert_eq!(bash_args[1], dir.join("bash/lune.bash").as_os_str());
        assert!(
            bash_cmd
                .get_envs()
                .any(|(k, v)| k == "LUNE_BASH_LOGIN" && v == Some(OsStr::new("1")))
        );

        let mut zsh_cmd = Command::new("/bin/zsh");
        apply(ShellKind::Zsh, &[], &dir, &mut zsh_cmd);
        assert!(
            zsh_cmd
                .get_envs()
                .any(|(k, v)| k == "ZDOTDIR" && v == Some(dir.join("zsh").as_os_str()))
        );

        let mut fish_cmd = Command::new("/usr/bin/fish");
        apply(ShellKind::Fish, &[], &dir, &mut fish_cmd);
        assert!(fish_cmd.get_envs().any(|(k, v)| {
            k == "XDG_DATA_DIRS"
                && v.and_then(OsStr::to_str).is_some_and(|val| {
                    val.starts_with(&dir.join("fish-data").to_string_lossy().to_string())
                })
        }));

        let _ = fs::remove_dir_all(&dir);
    }
}
