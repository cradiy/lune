use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;

use winit::event_loop::EventLoopProxy;

use super::Event;

pub(super) fn open(uri: Option<String>, proxy: EventLoopProxy<Event>) -> io::Result<()> {
    thread::Builder::new()
        .name("lune-open-directory".into())
        .spawn(move || {
            let _ = proxy.send_event(Event::DirectoryOpened(open_directory(uri.as_deref())));
        })?;
    Ok(())
}

fn open_directory(uri: Option<&str>) -> io::Result<()> {
    let path = match uri {
        Some(uri) => {
            let mut hostname = [0; 256];
            // SAFETY: The destination buffer is writable for the supplied length.
            let result = unsafe { libc::gethostname(hostname.as_mut_ptr().cast(), hostname.len()) };
            let local = if result == 0 {
                hostname.split(|&byte| byte == 0).next().unwrap_or_default()
            } else {
                &[]
            };
            local_path(uri, local)
                .ok_or_else(|| io::Error::other("The shell did not report a local directory"))?
        }
        None => std::env::current_dir()?,
    };
    if !path.metadata()?.is_dir() {
        return Err(io::Error::other("The reported path is not a directory"));
    }
    let status = Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!("xdg-open returned {status}")));
    }
    Ok(())
}

fn local_path(uri: &str, hostname: &[u8]) -> Option<PathBuf> {
    let (scheme, rest) = uri.as_bytes().split_at_checked(b"file://".len())?;
    if !scheme.eq_ignore_ascii_case(b"file://") {
        return None;
    }
    let (host, encoded) = rest.split_at(rest.iter().position(|&byte| byte == b'/')?);
    if !(host.is_empty()
        || host.eq_ignore_ascii_case(b"localhost")
        || host.eq_ignore_ascii_case(hostname))
    {
        return None;
    }
    let mut path = Vec::with_capacity(encoded.len());
    let mut bytes = encoded.iter().copied();
    while let Some(byte) = bytes.next() {
        let byte = match byte {
            b'?' | b'#' => return None,
            b'%' => {
                let high = char::from(bytes.next()?).to_digit(16)?;
                let low = char::from(bytes.next()?).to_digit(16)?;
                (high * 16 + low) as u8
            }
            _ => byte,
        };
        if byte == 0 {
            return None;
        }
        path.push(byte);
    }
    Some(PathBuf::from(OsString::from_vec(path)))
}

#[cfg(test)]
mod tests {
    use super::local_path;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn directory_reports_decode_only_local_file_paths() {
        let host = b"local.example.test";
        for authority in ["", "localhost", "LOCAL.example.test"] {
            let uri = format!("FILE://{authority}/build%20output/%E2%98%BE%23%FF");
            let path = local_path(&uri, host).unwrap();
            assert_eq!(
                path.as_os_str().as_bytes(),
                b"/build output/\xe2\x98\xbe#\xff"
            );
        }
        for uri in [
            "file://remote.example.test/build",
            "file://user@localhost/build",
            "file:///bad%00",
            "file:///bad%2",
            "file:///bad?query",
        ] {
            assert!(local_path(uri, host).is_none());
        }
    }
}
