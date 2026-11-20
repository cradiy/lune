use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use lune_pty::{ChildExit, Pty};

use super::Session;

#[derive(Clone, Copy, Debug)]
pub struct PtyInterest<'a> {
    pub fd: BorrowedFd<'a>,
    pub read: bool,
    pub write: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct SessionInterests<'a> {
    pub pty: Option<PtyInterest<'a>>,
    pub child_exit: Option<BorrowedFd<'a>>,
    pub presentation_deadline: Option<Instant>,
    /// Dispatch before blocking to process buffered work or report completion.
    pub immediate: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SessionReady {
    /// Readable, writable, or hung up.
    pub pty: bool,
    pub child_exit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    Pumped,
    ChildExited(ExitStatus),
    TimedOut,
    Finished,
}

impl Session<Pty> {
    /// Current interests for a level-triggered event loop. Refresh after dispatch.
    pub fn interests(&self) -> SessionInterests<'_> {
        let output_finished = self.is_finished();
        SessionInterests {
            pty: (!output_finished).then(|| PtyInterest {
                fd: self.as_fd(),
                read: self.wants_read(),
                write: self.wants_write(),
            }),
            child_exit: match &self.child_exit {
                Some(ChildExit::Pending(fd)) => Some(fd.as_fd()),
                _ => None,
            },
            presentation_deadline: self.presentation_deadline(),
            immediate: matches!(self.child_exit, Some(ChildExit::Exited(_)))
                || (output_finished && self.child_exit.is_none())
                || self.needs_pump(),
        }
    }

    /// Services readiness, buffered work, or an expired presentation deadline without waiting.
    /// A child exit is delivered before PTY work when both are ready.
    pub fn dispatch_ready(&mut self, ready: SessionReady) -> io::Result<Option<WaitOutcome>> {
        let status = match self.child_exit {
            Some(ChildExit::Exited(status)) => Some(status),
            Some(ChildExit::Pending(_)) if ready.child_exit => self.try_wait()?,
            _ => None,
        };
        if let Some(status) = status {
            self.child_exit = None;
            return Ok(Some(WaitOutcome::ChildExited(status)));
        }
        if self.is_finished() {
            return Ok(self.child_exit.is_none().then_some(WaitOutcome::Finished));
        }
        if ready.pty
            || self.needs_pump()
            || self
                .presentation_deadline()
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return self.pump_ready().map(Some);
        }
        Ok(None)
    }

    fn needs_pump(&self) -> bool {
        !self.is_finished()
            && (self.read_closed || (self.incoming_bytes.is_some() && !self.wants_write()))
    }

    /// Pumps on PTY readiness or at the presentation deadline and reports child exit once.
    /// A zero timeout checks readiness without waiting. `None` waits without a caller limit.
    /// `Finished` follows both child exit notification and drained PTY output.
    pub fn wait_and_pump(&mut self, timeout: Option<Duration>) -> io::Result<WaitOutcome> {
        let interests = self.interests();
        if interests.immediate {
            return self
                .dispatch_ready(SessionReady::default())?
                .ok_or_else(|| io::Error::other("Session work unavailable"));
        }
        let mut descriptors = [
            interests.pty.map_or(
                libc::pollfd {
                    fd: -1,
                    events: 0,
                    revents: 0,
                },
                |pty| libc::pollfd {
                    fd: pty.fd.as_raw_fd(),
                    events: (if pty.read { libc::POLLIN } else { 0 })
                        | (if pty.write { libc::POLLOUT } else { 0 }),
                    revents: 0,
                },
            ),
            libc::pollfd {
                fd: interests.child_exit.map_or(-1, |fd| fd.as_raw_fd()),
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
                let ready = SessionReady {
                    pty: descriptors[0].revents != 0,
                    child_exit: descriptors[1].revents != 0,
                };
                if let Some(outcome) = self.dispatch_ready(ready)? {
                    return Ok(outcome);
                }
                return Err(io::Error::other("Session readiness did not produce work"));
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
