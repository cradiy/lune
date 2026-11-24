use std::ffi::{CString, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use winit::event_loop::EventLoopProxy;

use crate::desktop::Event;

const DEBOUNCE: Duration = Duration::from_millis(50);

const PARENT_MASK: u32 = libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_ONLYDIR;
const DIR_MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_TO
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MODIFY
    | libc::IN_ATTRIB
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR;

pub(in crate::desktop) struct Watch {
    stop: Option<UnixStream>,
    worker: Option<JoinHandle<()>>,
}

impl Watch {
    pub(in crate::desktop) fn start(
        directory: PathBuf,
        file_name: OsString,
        proxy: EventLoopProxy<Event>,
    ) -> io::Result<Self> {
        Self::start_with(directory, file_name, move || {
            proxy.send_event(Event::ConfigReload(false)).is_ok()
        })
    }

    fn start_with(
        directory: PathBuf,
        file_name: OsString,
        on_change: impl Fn() -> bool + Send + 'static,
    ) -> io::Result<Self> {
        // SAFETY: inotify_init1 takes flags and returns a new descriptor on success.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: The successful call transferred ownership of the descriptor.
        let inotify = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut watches = WatchSet::new(directory, file_name);
        watches.refresh(&inotify);
        let (stop, worker_stop) = UnixStream::pair()?;
        let worker = thread::Builder::new()
            .name("lune-config".into())
            .spawn(move || run(inotify, worker_stop, watches, on_change))?;
        Ok(Self {
            stop: Some(stop),
            worker: Some(worker),
        })
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct WatchSet {
    parent_dir: Option<PathBuf>,
    config_dir: PathBuf,
    themes_dir: PathBuf,
    config_name: Vec<u8>,
    file_name: Vec<u8>,
    parent_wd: Option<libc::c_int>,
    config_wd: Option<libc::c_int>,
    themes_wd: Option<libc::c_int>,
}

impl WatchSet {
    fn new(config_dir: PathBuf, file_name: OsString) -> Self {
        let parent_dir = config_dir.parent().map(Path::to_path_buf);
        let config_name = config_dir
            .file_name()
            .map(|name| name.as_bytes().to_vec())
            .unwrap_or_default();
        let themes_dir = config_dir.join("themes");
        Self {
            parent_dir,
            config_dir,
            themes_dir,
            config_name,
            file_name: file_name.as_bytes().to_vec(),
            parent_wd: None,
            config_wd: None,
            themes_wd: None,
        }
    }

    fn refresh(&mut self, inotify: &OwnedFd) {
        if self.parent_wd.is_none()
            && let Some(parent) = &self.parent_dir
        {
            self.parent_wd = add_watch(inotify, parent, PARENT_MASK);
        }
        if self.config_wd.is_none() {
            self.config_wd = add_watch(inotify, &self.config_dir, DIR_MASK);
        }
        if self.themes_wd.is_none() {
            self.themes_wd = add_watch(inotify, &self.themes_dir, DIR_MASK);
        }
    }

    fn handle_event(&mut self, wd: libc::c_int, mask: u32, name: &[u8]) -> (bool, bool) {
        let mut relevant = false;
        let mut rewatch = false;
        if mask & libc::IN_Q_OVERFLOW != 0 {
            return (true, true);
        }
        if self.parent_wd == Some(wd) && name == self.config_name.as_slice() {
            relevant = true;
            rewatch = true;
        } else if self.config_wd == Some(wd) {
            if mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED) != 0 {
                self.config_wd = None;
                relevant = true;
                rewatch = true;
            } else if name == self.file_name.as_slice() {
                relevant = true;
            } else if name == b"themes" {
                relevant = true;
                rewatch = true;
            }
        } else if self.themes_wd == Some(wd) {
            if mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED) != 0 {
                self.themes_wd = None;
                relevant = true;
                rewatch = true;
            } else if name.ends_with(b".conf") {
                relevant = true;
            }
        }
        (relevant, rewatch)
    }
}

fn add_watch(inotify: &OwnedFd, path: &Path, mask: u32) -> Option<libc::c_int> {
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: inotify is a valid descriptor and c_path is a NUL-terminated path string.
    let wd = unsafe { libc::inotify_add_watch(inotify.as_raw_fd(), c_path.as_ptr(), mask) };
    (wd >= 0).then_some(wd)
}

fn run(inotify: OwnedFd, stop: UnixStream, mut watches: WatchSet, on_change: impl Fn() -> bool) {
    let mut buffer = [0u8; 4096];
    let mut deadline: Option<Instant> = None;

    loop {
        let timeout_ms = deadline.map_or(-1, |due| {
            due.saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32
        });
        let mut fds = [
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: inotify.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: fds points to two initialized pollfd structures for the duration of the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[0].revents != 0 {
            break;
        }
        if fds[1].revents & libc::POLLIN != 0 {
            let (relevant, rewatch) = drain_events(&inotify, &mut watches, &mut buffer);
            if rewatch {
                watches.refresh(&inotify);
            }
            if relevant {
                deadline = Some(Instant::now() + DEBOUNCE);
            }
        }
        if let Some(due) = deadline
            && Instant::now() >= due
        {
            deadline = None;
            watches.refresh(&inotify);
            if !on_change() {
                break;
            }
        }
    }
}

fn drain_events(inotify: &OwnedFd, watches: &mut WatchSet, buffer: &mut [u8]) -> (bool, bool) {
    let mut relevant = false;
    let mut rewatch = false;
    loop {
        // SAFETY: buffer is writable for buffer.len() bytes and inotify is a valid descriptor.
        let read = unsafe {
            libc::read(
                inotify.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if read <= 0 {
            break;
        }
        let bytes = &buffer[..read as usize];
        let mut offset = 0;
        let header_size = size_of::<libc::inotify_event>();
        while offset + header_size <= bytes.len() {
            // SAFETY: offset + header_size <= bytes.len() and read_unaligned handles any alignment.
            let event = unsafe {
                std::ptr::read_unaligned(bytes[offset..].as_ptr().cast::<libc::inotify_event>())
            };
            let name_len = event.len as usize;
            let name_start = offset + header_size;
            let name_end = (name_start + name_len).min(bytes.len());
            let raw_name = &bytes[name_start..name_end];
            let nul = raw_name
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(raw_name.len());
            let (hit, need_rewatch) = watches.handle_event(event.wd, event.mask, &raw_name[..nul]);
            relevant |= hit;
            rewatch |= need_rewatch;
            offset = name_end;
        }
    }
    (relevant, rewatch)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::Watch;

    #[test]
    fn watcher_detects_atomic_config_rename_and_theme_edits() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("lune-config-watch-{}-{unique}", std::process::id()));
        let config_dir = root.join("lune");
        let themes_dir = config_dir.join("themes");
        fs::create_dir_all(&themes_dir).unwrap();

        let (tx, rx) = mpsc::channel();
        let watch = Watch::start_with(config_dir.clone(), "lune.conf".into(), move || {
            tx.send(()).is_ok()
        })
        .unwrap();

        // Atomic save via temp file + rename (vim/nvim/helix style).
        let temp = config_dir.join("lune.conf.tmp");
        let conf = config_dir.join("lune.conf");
        fs::write(&temp, "background-opacity = 0.85\n").unwrap();
        fs::rename(&temp, &conf).unwrap();
        rx.recv_timeout(Duration::from_secs(2))
            .expect("expected reload notification after atomic lune.conf save");

        // Edit a custom theme file in themes/.
        fs::write(themes_dir.join("night.conf"), "variant = dark\n").unwrap();
        rx.recv_timeout(Duration::from_secs(2))
            .expect("expected reload notification after theme file edit");

        drop(watch);
        let _ = fs::remove_dir_all(&root);
    }
}
