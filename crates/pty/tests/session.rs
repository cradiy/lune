#![cfg(target_os = "linux")]

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::process::Command;
use std::time::{Duration, Instant};

use lune_pty::{Pty, PtySize};

#[test]
fn shell_uses_the_pty_for_io_and_observes_the_resize() {
    let mut shell = Command::new("sh");
    shell.arg("-c").arg(
        "test -t 0 && test -t 1 && test -t 2 && exec 3<>/dev/tty || exit 91
         stty -echo opost onlcr || exit 92
         stty size || exit 93
         printf 'ready\n'
         IFS= read -r value || exit 94
         stty size || exit 95
         printf 'input:%s\n' \"$value\"
         printf 'stderr\n' >&2
         exit 17",
    );
    let initial = PtySize::new(24, 80);
    let mut pty = Pty::spawn(shell, initial).unwrap();
    pty.set_nonblocking(true).unwrap();
    assert_eq!(pty.size().unwrap(), initial);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    while !output.ends_with(b"ready\r\n") {
        assert!(read_output(&mut pty, &mut output, deadline));
    }
    assert_eq!(output, b"24 80\r\nready\r\n");
    assert_eq!(pty.try_wait().unwrap(), None);
    assert_eq!(
        pty.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );

    let size = PtySize {
        pixel_width: 960,
        pixel_height: 560,
        ..PtySize::new(28, 96)
    };
    pty.resize(size).unwrap();
    assert_eq!(pty.size().unwrap(), size);
    assert_eq!(
        pty.resize(PtySize::new(0, 80)).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(pty.size().unwrap(), size);
    pty.write_all(b"sample\n").unwrap();

    while read_output(&mut pty, &mut output, deadline) {}
    assert_eq!(
        output,
        b"24 80\r\nready\r\n28 96\r\ninput:sample\r\nstderr\r\n"
    );
    let status = pty.wait().unwrap();
    assert_eq!(status.code(), Some(17));
    assert_eq!(pty.try_wait().unwrap(), Some(status));
}

#[test]
fn dropping_a_running_pty_reaps_its_child() {
    let mut shell = Command::new("sh");
    shell.args(["-c", "IFS= read -r value"]);
    let pty = Pty::spawn(shell, PtySize::new(24, 80)).unwrap();
    let pid = pty.child_id() as libc::pid_t;
    drop(pty);
    // SAFETY: The PID identifies the test's former child. A null status discards
    // the status, and WNOHANG makes the check nonblocking.
    let result = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
    let error = io::Error::last_os_error();
    assert_eq!(result, -1);
    assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
}

fn read_output(pty: &mut Pty, output: &mut Vec<u8>, deadline: Instant) -> bool {
    let mut buffer = [0; 256];
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for PTY output"
        );
        match pty.read(&mut buffer) {
            Ok(0) => return false,
            Ok(count) => {
                output.extend_from_slice(&buffer[..count]);
                return true;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let mut waiter = libc::pollfd {
                    fd: pty.as_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let remaining = deadline.saturating_duration_since(Instant::now());
                // SAFETY: waiter points to one initialized pollfd for this call.
                let result = unsafe { libc::poll(&mut waiter, 1, remaining.as_millis() as i32) };
                if result < 0 {
                    assert_eq!(
                        io::Error::last_os_error().kind(),
                        io::ErrorKind::Interrupted
                    );
                }
            }
            Err(error) => panic!("PTY read failed: {error}"),
        }
    }
}
