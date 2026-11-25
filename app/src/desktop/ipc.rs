//! Local Unix-domain-socket IPC for controlling a running Lune window via
//! `lune msg <subcommand>`.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use winit::event_loop::EventLoopProxy;

use super::Event;
use super::split::Axis;

const IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_ARGS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IpcCommand {
    NewTab {
        working_directory: Option<PathBuf>,
        title: Option<String>,
        command: Option<Vec<OsString>>,
    },
    Split {
        axis: Axis,
        working_directory: Option<PathBuf>,
    },
    SetTheme(String),
    ReloadConfig,
    SaveConfig,
}

pub(super) struct IpcServer {
    socket_path: PathBuf,
    wake_write: Option<OwnedFd>,
    thread: Option<JoinHandle<()>>,
}

impl IpcServer {
    pub(super) fn start(proxy: EventLoopProxy<Event>) -> Option<Self> {
        let dir = runtime_dir()?;
        if let Err(error) = ensure_secure_dir(&dir) {
            log::debug!("Could not prepare IPC runtime directory: {error}");
            return None;
        }
        let socket_path = dir.join(format!("ipc-{}.sock", std::process::id()));
        let _ = fs::remove_file(&socket_path);

        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(error) => {
                log::debug!("Could not bind IPC socket: {error}");
                return None;
            }
        };
        let _ = fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600));
        if listener.set_nonblocking(true).is_err() {
            let _ = fs::remove_file(&socket_path);
            return None;
        }

        let Ok((wake_read, wake_write)) = create_wake_pipe() else {
            let _ = fs::remove_file(&socket_path);
            return None;
        };

        let thread = match thread::Builder::new()
            .name("lune-ipc".into())
            .spawn(move || run_listener(listener, wake_read, proxy))
        {
            Ok(thread) => thread,
            Err(error) => {
                log::warn!("Could not start IPC listener thread: {error}");
                let _ = fs::remove_file(&socket_path);
                return None;
            }
        };

        log::debug!("IPC socket listening at {}", socket_path.display());
        Some(Self {
            socket_path,
            wake_write: Some(wake_write),
            thread: Some(thread),
        })
    }

    pub(super) fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        if let Some(wake) = self.wake_write.take() {
            let byte = [1u8; 1];
            // SAFETY: `wake` is a valid owned pipe write descriptor and `byte` is initialized.
            let _ = unsafe { libc::write(wake.as_raw_fd(), byte.as_ptr().cast(), byte.len()) };
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

pub fn list_sockets() -> Vec<PathBuf> {
    let Some(dir) = runtime_dir() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut sockets: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !(name.starts_with("ipc-") && name.ends_with(".sock")) {
            continue;
        }
        if UnixStream::connect(&path).is_ok() {
            let mtime = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            sockets.push((mtime, path));
        } else {
            // Remove stale socket left behind by an abruptly killed instance.
            let _ = fs::remove_file(&path);
        }
    }
    sockets.sort_by_key(|a| std::cmp::Reverse(a.0));
    sockets.into_iter().map(|(_, path)| path).collect()
}

pub fn send_command(socket: Option<&Path>, command: &IpcCommand) -> Result<(), String> {
    let target = if let Some(path) = socket {
        path.to_path_buf()
    } else if let Some(env_path) = env::var_os("LUNE_IPC_SOCKET")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.exists())
    {
        env_path
    } else {
        list_sockets()
            .into_iter()
            .next()
            .ok_or_else(|| "no running Lune window found".to_owned())?
    };

    send_to_socket(&target, command)
}

fn send_to_socket(path: &Path, command: &IpcCommand) -> Result<(), String> {
    let mut stream = UnixStream::connect(path)
        .map_err(|error| format!("could not connect to {}: {error}", path.display()))?;
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    let frame = encode_command(command)?;
    stream
        .write_all(&frame)
        .map_err(|error| format!("failed to send IPC command: {error}"))?;
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let mut reply = String::new();
    stream
        .take(1024)
        .read_to_string(&mut reply)
        .map_err(|error| format!("failed to read IPC response: {error}"))?;
    let line = reply.lines().next().unwrap_or("");
    if line == "OK" {
        Ok(())
    } else if let Some(err) = line.strip_prefix("ERR ") {
        Err(err.to_owned())
    } else {
        Err("unexpected IPC response from Lune window".to_owned())
    }
}

fn run_listener(listener: UnixListener, wake_read: OwnedFd, proxy: EventLoopProxy<Event>) {
    let mut fds = [
        libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake_read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        fds[0].revents = 0;
        fds[1].revents = 0;
        // SAFETY: Both descriptors are valid and `fds` has length 2.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[1].revents != 0 {
            break;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => handle_client(stream, &proxy),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => return,
                }
            }
        }
    }
}

fn handle_client(mut stream: UnixStream, proxy: &EventLoopProxy<Event>) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    let mut raw = Vec::new();
    if Read::by_ref(&mut stream)
        .take((MAX_MESSAGE_BYTES + 1) as u64)
        .read_to_end(&mut raw)
        .is_err()
    {
        return;
    }
    if raw.len() > MAX_MESSAGE_BYTES {
        let _ = stream.write_all(b"ERR message too large\n");
        return;
    }
    match decode_command(&raw) {
        Ok(command) => {
            if proxy.send_event(Event::IpcCommand(command)).is_ok() {
                let _ = stream.write_all(b"OK\n");
            } else {
                let _ = stream.write_all(b"ERR window is closing\n");
            }
        }
        Err(error) => {
            let _ = writeln!(stream, "ERR {error}");
        }
    }
}

fn runtime_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty())?;
    let path = PathBuf::from(base);
    path.is_absolute().then(|| path.join("lune"))
}

fn ensure_secure_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

fn create_wake_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    // SAFETY: `fds` is a writable 2-element `c_int` buffer for `pipe2`.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe2` succeeded and returned two newly created descriptors.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: Same as above for the write end.
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((read_fd, write_fd))
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn push_opt_os(out: &mut Vec<u8>, value: Option<&OsStr>) {
    match value {
        Some(v) => {
            out.push(1);
            push_bytes(out, v.as_bytes());
        }
        None => out.push(0),
    }
}

fn encode_command(command: &IpcCommand) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(128);
    out.push(1); // protocol version 1
    match command {
        IpcCommand::NewTab {
            working_directory,
            title,
            command,
        } => {
            out.push(1);
            push_opt_os(&mut out, working_directory.as_deref().map(Path::as_os_str));
            push_opt_os(&mut out, title.as_deref().map(OsStr::new));
            match command {
                Some(args) => {
                    if args.len() > MAX_ARGS {
                        return Err("too many command arguments".into());
                    }
                    out.push(1);
                    out.extend_from_slice(&(args.len() as u32).to_le_bytes());
                    for arg in args {
                        push_bytes(&mut out, arg.as_bytes());
                    }
                }
                None => out.push(0),
            }
        }
        IpcCommand::Split {
            axis,
            working_directory,
        } => {
            out.push(2);
            out.push(match axis {
                Axis::Columns => 0,
                Axis::Rows => 1,
            });
            push_opt_os(&mut out, working_directory.as_deref().map(Path::as_os_str));
        }
        IpcCommand::SetTheme(name) => {
            out.push(3);
            push_bytes(&mut out, name.as_bytes());
        }
        IpcCommand::ReloadConfig => {
            out.push(4);
        }
        IpcCommand::SaveConfig => {
            out.push(5);
        }
    }
    if out.len() > MAX_MESSAGE_BYTES {
        return Err("IPC message exceeds size limit".into());
    }
    Ok(out)
}

fn take_u8(input: &mut &[u8]) -> Result<u8, String> {
    let (&first, rest) = input
        .split_first()
        .ok_or_else(|| "truncated IPC message".to_owned())?;
    *input = rest;
    Ok(first)
}

fn take_u32(input: &mut &[u8]) -> Result<u32, String> {
    if input.len() < 4 {
        return Err("truncated IPC integer".into());
    }
    let (head, rest) = input.split_at(4);
    *input = rest;
    Ok(u32::from_le_bytes(head.try_into().unwrap()))
}

fn take_bytes<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], String> {
    let len = take_u32(input)? as usize;
    if len > input.len() {
        return Err("truncated IPC payload".into());
    }
    let (head, rest) = input.split_at(len);
    *input = rest;
    Ok(head)
}

fn take_opt_os(input: &mut &[u8]) -> Result<Option<OsString>, String> {
    match take_u8(input)? {
        0 => Ok(None),
        1 => Ok(Some(OsString::from_vec(take_bytes(input)?.to_vec()))),
        _ => Err("invalid optional field tag".into()),
    }
}

fn decode_command(mut input: &[u8]) -> Result<IpcCommand, String> {
    let version = take_u8(&mut input)?;
    if version != 1 {
        return Err(format!("unsupported IPC version {version}"));
    }
    let kind = take_u8(&mut input)?;
    let command = match kind {
        1 => {
            let working_directory = take_opt_os(&mut input)?.map(PathBuf::from);
            let title = match take_opt_os(&mut input)? {
                Some(raw) => Some(
                    raw.into_string()
                        .map_err(|_| "tab title must be valid UTF-8".to_owned())?,
                ),
                None => None,
            };
            let command = match take_u8(&mut input)? {
                0 => None,
                1 => {
                    let count = take_u32(&mut input)? as usize;
                    if count > MAX_ARGS {
                        return Err("too many command arguments".into());
                    }
                    let mut args = Vec::with_capacity(count);
                    for _ in 0..count {
                        args.push(OsString::from_vec(take_bytes(&mut input)?.to_vec()));
                    }
                    Some(args)
                }
                _ => return Err("invalid command tag".into()),
            };
            IpcCommand::NewTab {
                working_directory,
                title,
                command,
            }
        }
        2 => {
            let axis = match take_u8(&mut input)? {
                0 => Axis::Columns,
                1 => Axis::Rows,
                _ => return Err("invalid split axis".into()),
            };
            let working_directory = take_opt_os(&mut input)?.map(PathBuf::from);
            IpcCommand::Split {
                axis,
                working_directory,
            }
        }
        3 => {
            let name = std::str::from_utf8(take_bytes(&mut input)?)
                .map_err(|_| "theme name must be valid UTF-8".to_owned())?
                .to_owned();
            IpcCommand::SetTheme(name)
        }
        4 => IpcCommand::ReloadConfig,
        5 => IpcCommand::SaveConfig,
        _ => return Err("unknown IPC command".into()),
    };
    if !input.is_empty() {
        return Err("trailing bytes in IPC message".into());
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes_all_ipc_commands() {
        let cases = [
            IpcCommand::NewTab {
                working_directory: Some(PathBuf::from("/tmp/dir with spaces")),
                title: Some("Build".into()),
                command: Some(vec![OsString::from("sh"), OsString::from("-c")]),
            },
            IpcCommand::Split {
                axis: Axis::Rows,
                working_directory: None,
            },
            IpcCommand::SetTheme("tokyo-night".into()),
            IpcCommand::ReloadConfig,
            IpcCommand::SaveConfig,
        ];
        for command in cases {
            let wire = encode_command(&command).unwrap();
            let decoded = decode_command(&wire).unwrap();
            assert_eq!(decoded, command);
        }
    }
}
