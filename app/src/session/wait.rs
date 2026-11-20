use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant};

use lune_pty::Pty;

use super::Session;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    Pumped,
    TimedOut,
    Finished,
}

impl Session<Pty> {
    /// Pumps on PTY readiness or at the presentation deadline.
    /// A zero timeout checks readiness without waiting. `None` waits without a caller limit.
    /// `Finished` marks drained PTY output. Use `try_wait` for the child status.
    pub fn wait_and_pump(&mut self, timeout: Option<Duration>) -> io::Result<WaitOutcome> {
        if self.is_finished() {
            return Ok(WaitOutcome::Finished);
        }
        let writable = self.wants_write();
        if self.read_closed || (self.incoming_bytes.is_some() && !writable) {
            return self.pump_ready();
        }
        let mut descriptor = libc::pollfd {
            fd: self.as_fd().as_raw_fd(),
            events: (if self.wants_read() { libc::POLLIN } else { 0 })
                | (if writable { libc::POLLOUT } else { 0 }),
            revents: 0,
        };
        let caller_limit = timeout.map(|duration| (Instant::now(), duration));
        let mut polled = false;
        loop {
            let presentation_wait = self
                .presentation_deadline()
                .map(|deadline| deadline.saturating_duration_since(Instant::now()));
            if presentation_wait == Some(Duration::ZERO) {
                return self.pump_ready();
            }
            let caller_wait =
                caller_limit.map(|(start, duration)| duration.saturating_sub(start.elapsed()));
            if polled && caller_wait == Some(Duration::ZERO) {
                return Ok(WaitOutcome::TimedOut);
            }
            let wait = match (presentation_wait, caller_wait) {
                (Some(presentation), Some(caller)) => Some(presentation.min(caller)),
                (presentation, caller) => presentation.or(caller),
            };
            let millis = wait.map_or(-1, |duration| {
                i32::try_from(duration.as_nanos().div_ceil(1_000_000)).unwrap_or(i32::MAX)
            });
            // SAFETY: descriptor is initialized, the session owns its fd, and poll writes one entry.
            let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
            if ready > 0 {
                if descriptor.revents & libc::POLLNVAL != 0 {
                    return Err(io::Error::from_raw_os_error(libc::EBADF));
                }
                return self.pump_ready();
            }
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            polled = true;
        }
    }

    fn pump_ready(&mut self) -> io::Result<WaitOutcome> {
        self.pump()?;
        Ok(if self.is_finished() {
            WaitOutcome::Finished
        } else {
            WaitOutcome::Pumped
        })
    }
}
