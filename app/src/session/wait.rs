use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use lune_pty::{ChildExit, Pty};

use super::Session;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    Pumped,
    ChildExited(ExitStatus),
    TimedOut,
    Finished,
}

impl Session<Pty> {
    /// Pumps on PTY readiness or at the presentation deadline and reports child exit once.
    /// A zero timeout checks readiness without waiting. `None` waits without a caller limit.
    /// `Finished` follows both child exit notification and drained PTY output.
    pub fn wait_and_pump(&mut self, timeout: Option<Duration>) -> io::Result<WaitOutcome> {
        let child_fd = match &self.child_exit {
            Some(ChildExit::Pending(fd)) => fd.as_raw_fd(),
            Some(ChildExit::Exited(status)) => {
                let status = *status;
                self.child_exit = None;
                return Ok(WaitOutcome::ChildExited(status));
            }
            None => -1,
        };
        let output_finished = self.is_finished();
        if output_finished && child_fd < 0 {
            return Ok(WaitOutcome::Finished);
        }
        let writable = self.wants_write();
        if !output_finished && (self.read_closed || (self.incoming_bytes.is_some() && !writable)) {
            return self.pump_ready();
        }
        let mut descriptors = [
            libc::pollfd {
                fd: if output_finished {
                    -1
                } else {
                    self.as_fd().as_raw_fd()
                },
                events: (if self.wants_read() { libc::POLLIN } else { 0 })
                    | (if writable { libc::POLLOUT } else { 0 }),
                revents: 0,
            },
            libc::pollfd {
                fd: child_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
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
            // SAFETY: Both poll entries are initialized and their enabled fds are owned by self.
            let ready = unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    millis,
                )
            };
            if ready > 0 {
                if (descriptors[0].revents | descriptors[1].revents) & libc::POLLNVAL != 0 {
                    return Err(io::Error::from_raw_os_error(libc::EBADF));
                }
                if descriptors[1].revents != 0 {
                    let status = self
                        .try_wait()?
                        .ok_or_else(|| io::Error::other("Child exit status unavailable"))?;
                    self.child_exit = None;
                    return Ok(WaitOutcome::ChildExited(status));
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
        Ok(if self.is_finished() && self.child_exit.is_none() {
            WaitOutcome::Finished
        } else {
            WaitOutcome::Pumped
        })
    }
}
