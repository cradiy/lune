use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::thread;
use std::time::Instant;

use lune_terminal::{ClipboardQuery, MAX_CLIPBOARD_BYTES};
use winit::event_loop::EventLoopProxy;
use wl_clipboard_rs::{copy, paste};

use crate::MAX_PASTE_BYTES;

use super::{Event, PASTE_TIMEOUT, PaneId};

pub(super) use wl_clipboard_rs::paste::ClipboardType;

mod data_device;
mod primary;
pub(super) use data_device::{DataDevice, DataDeviceHandle};
use data_device::{ReadOutcome, WriteOutcome};
pub(super) use primary::Primary;

pub(super) fn write(
    text: String,
    data_device: Option<DataDeviceHandle>,
    proxy: EventLoopProxy<Event>,
) -> io::Result<()> {
    thread::Builder::new()
        .name("lune-copy".into())
        .spawn(move || {
            let allow_data_control = data_device
                .as_ref()
                .is_none_or(DataDeviceHandle::has_data_control);
            if let Some(dd) = data_device {
                match dd.write(text.clone(), PASTE_TIMEOUT) {
                    WriteOutcome::Published => {
                        let _ = proxy.send_event(Event::CopyReady(Ok(())));
                        return;
                    }
                    WriteOutcome::Fallback if !allow_data_control => {
                        let _ = proxy.send_event(Event::CopyReady(Err(
                            "Clipboard write requires window focus".into(),
                        )));
                        return;
                    }
                    WriteOutcome::Fallback => {}
                }
            }
            let source = copy::Source::Bytes(text.into_bytes().into_boxed_slice());
            let mut options = copy::Options::new();
            options.foreground(true);
            match options.prepare_copy(source, copy::MimeType::Text) {
                Ok(prepared) => {
                    if proxy.send_event(Event::CopyReady(Ok(()))).is_ok() {
                        let _ = prepared.serve();
                    }
                }
                Err(error) => {
                    let _ = proxy.send_event(Event::CopyReady(Err(error.to_string())));
                }
            }
        })?;
    Ok(())
}

pub(super) fn read(
    clipboard: ClipboardType,
    data_device: Option<DataDeviceHandle>,
    primary: Option<&Primary>,
    pane: PaneId,
    proxy: EventLoopProxy<Event>,
) -> io::Result<()> {
    let publication = primary
        .filter(|_| clipboard == ClipboardType::Primary)
        .and_then(Primary::publication);
    thread::Builder::new()
        .name("lune-paste".into())
        .spawn(move || {
            let deadline = Instant::now() + PASTE_TIMEOUT;
            let result = match publication {
                Some(publication) if !publication.wait(deadline) => Err("Paste timed out".into()),
                _ => read_text(
                    clipboard,
                    data_device.as_ref(),
                    deadline,
                    MAX_PASTE_BYTES,
                    true,
                ),
            };
            let _ = proxy.send_event(Event::PasteReady(pane, result));
        })?;
    Ok(())
}

pub(super) fn read_osc52(
    clipboard: ClipboardType,
    data_device: Option<DataDeviceHandle>,
    primary: Option<&Primary>,
    pane: PaneId,
    query: ClipboardQuery,
    proxy: EventLoopProxy<Event>,
) -> io::Result<()> {
    let publication = primary
        .filter(|_| clipboard == ClipboardType::Primary)
        .and_then(Primary::publication);
    thread::Builder::new()
        .name("lune-osc52-read".into())
        .spawn(move || {
            let deadline = Instant::now() + PASTE_TIMEOUT;
            let result = match publication {
                Some(publication) if !publication.wait(deadline) => {
                    Err("Clipboard read timed out".into())
                }
                _ => read_text(
                    clipboard,
                    data_device.as_ref(),
                    deadline,
                    MAX_CLIPBOARD_BYTES,
                    false,
                ),
            };
            let _ = proxy.send_event(Event::Osc52ReadReady(pane, query, result));
        })?;
    Ok(())
}

fn read_text(
    clipboard: ClipboardType,
    data_device: Option<&DataDeviceHandle>,
    deadline: Instant,
    limit: usize,
    strip_controls: bool,
) -> Result<String, String> {
    if let Some(dd) = data_device {
        match dd.read(clipboard, deadline) {
            ReadOutcome::Text(text) => {
                if text.len() > limit {
                    return Err("Clipboard text exceeds the size limit".into());
                }
                return Ok(sanitize_text(text, strip_controls));
            }
            ReadOutcome::Pipe(fd) => {
                return read_pipe_text(File::from(fd), deadline, limit, strip_controls);
            }
            ReadOutcome::Empty => return Ok(String::new()),
            ReadOutcome::Fallback => {}
        }
    }

    if data_device.is_some_and(|dd| !dd.has_data_control()) {
        return Ok(String::new());
    }

    let reader =
        match paste::get_contents(clipboard, paste::Seat::Unspecified, paste::MimeType::Text) {
            Ok((pipe, _)) => pipe,
            Err(
                paste::Error::ClipboardEmpty
                | paste::Error::NoMimeType
                | paste::Error::NoSeats
                | paste::Error::MissingProtocol { .. },
            ) => {
                return Ok(String::new());
            }
            Err(error) => return Err(error.to_string()),
        };
    read_pipe_text(reader, deadline, limit, strip_controls)
}

pub(super) fn read_pipe_text<R: Read + AsFd>(
    mut reader: R,
    deadline: Instant,
    limit: usize,
    strip_controls: bool,
) -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 16 * 1024];
    loop {
        if !wait_fd(reader.as_fd(), libc::POLLIN, deadline).map_err(|error| error.to_string())? {
            return Err("Paste timed out".into());
        }
        let count = reader.read(&mut chunk).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        if bytes.len() + count > limit {
            return Err("Clipboard text exceeds the size limit".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let text = String::from_utf8(bytes).map_err(|error| error.to_string())?;
    Ok(sanitize_text(text, strip_controls))
}

fn sanitize_text(mut text: String, strip_controls: bool) -> String {
    if strip_controls {
        text.retain(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'));
    }
    text
}

pub(super) fn transfer(fd: OwnedFd, mut bytes: &[u8]) -> io::Result<()> {
    let mut pipe = File::from(fd);
    // SAFETY: The pipe owns the descriptor for both fcntl calls.
    unsafe {
        let flags = libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let deadline = Instant::now() + PASTE_TIMEOUT;
    while !bytes.is_empty() && wait_fd(pipe.as_fd(), libc::POLLOUT, deadline)? {
        match pipe.write(bytes) {
            Ok(0) => break,
            Ok(count) => bytes = &bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn wait_fd(fd: BorrowedFd<'_>, events: libc::c_short, deadline: Instant) -> io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    };
    loop {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Ok(false);
        }
        // SAFETY: The descriptor is borrowed for the call and the poll entry is initialized.
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout.as_millis() as i32 + 1) };
        if ready >= 0 {
            return Ok(ready > 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
