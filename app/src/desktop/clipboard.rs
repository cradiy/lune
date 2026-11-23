use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd};
use std::thread;
use std::time::Instant;

use winit::event_loop::EventLoopProxy;
use wl_clipboard_rs::{copy, paste};

use crate::MAX_PASTE_BYTES;

use super::{Event, PASTE_TIMEOUT};

pub(super) use wl_clipboard_rs::paste::ClipboardType;

pub(super) fn write(text: String, proxy: EventLoopProxy<Event>) -> io::Result<()> {
    thread::Builder::new()
        .name("lune-copy".into())
        .spawn(move || {
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

pub(super) fn read(clipboard: ClipboardType, proxy: EventLoopProxy<Event>) -> io::Result<()> {
    thread::Builder::new()
        .name("lune-paste".into())
        .spawn(move || {
            let _ = proxy.send_event(Event::PasteReady(read_text(clipboard)));
        })?;
    Ok(())
}

fn read_text(clipboard: ClipboardType) -> Result<String, String> {
    let deadline = Instant::now() + PASTE_TIMEOUT;
    let mut reader =
        match paste::get_contents(clipboard, paste::Seat::Unspecified, paste::MimeType::Text) {
            Ok((pipe, _)) => pipe,
            Err(
                paste::Error::ClipboardEmpty | paste::Error::NoMimeType | paste::Error::NoSeats,
            ) => {
                return Ok(String::new());
            }
            Err(error) => return Err(error.to_string()),
        };
    let mut bytes = Vec::new();
    let mut chunk = [0; 16 * 1024];
    let mut descriptor = libc::pollfd {
        fd: reader.as_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err("Paste timed out".into());
        }
        // SAFETY: The reader owns the fd and the poll entry is initialized.
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout.as_millis() as i32 + 1) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.to_string());
        }
        if ready == 0 {
            continue;
        }
        let count = reader.read(&mut chunk).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        if bytes.len() + count > MAX_PASTE_BYTES {
            return Err("Clipboard text exceeds the paste size limit".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let mut text = String::from_utf8(bytes).map_err(|error| error.to_string())?;
    text.retain(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'));
    Ok(text)
}
