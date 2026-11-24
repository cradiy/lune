use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::{SessionInterests, SessionReady};
use winit::event_loop::EventLoopProxy;

use super::{Event, TabId};

const STOP: u64 = 0;

pub(super) struct Readiness {
    epoll: Arc<OwnedFd>,
    stop: Option<UnixStream>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
pub(super) struct Registration {
    pty_events: Option<u32>,
    pty_sequence: u32,
    child_armed: bool,
}

impl Readiness {
    pub(super) fn new(proxy: EventLoopProxy<Event>) -> io::Result<Self> {
        // SAFETY: epoll_create1 takes a flag and returns a new descriptor on success.
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: The successful call transferred ownership of the descriptor.
        let epoll = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
        let (stop, worker_stop) = UnixStream::pair()?;
        control(&epoll, libc::EPOLL_CTL_ADD, worker_stop.as_fd(), 0, STOP)?;
        let worker_epoll = epoll.clone();
        let worker = thread::Builder::new()
            .name("lune-pty".into())
            .spawn(move || wait(&worker_epoll, worker_stop, proxy))?;
        Ok(Self {
            epoll,
            stop: Some(stop),
            worker: Some(worker),
        })
    }

    pub(super) fn arm(
        &self,
        id: TabId,
        registration: &mut Registration,
        interests: SessionInterests<'_>,
    ) -> io::Result<()> {
        if let Some(pty) = interests.pty {
            let events = (if pty.read { libc::EPOLLIN } else { 0 })
                | (if pty.write { libc::EPOLLOUT } else { 0 });
            let events = events as u32;
            if registration.pty_events != Some(events) {
                let operation = if registration.pty_sequence == 0 {
                    libc::EPOLL_CTL_ADD
                } else {
                    libc::EPOLL_CTL_MOD
                };
                let sequence = registration.pty_sequence.wrapping_add(1).max(1);
                control(&self.epoll, operation, pty.fd, events, token(id, sequence))?;
                registration.pty_sequence = sequence;
                registration.pty_events = Some(events);
            }
        } else {
            registration.pty_events = None;
        }
        if let Some(child) = interests.child_exit
            && !registration.child_armed
        {
            control(
                &self.epoll,
                libc::EPOLL_CTL_ADD,
                child,
                libc::EPOLLIN as u32,
                token(id, 0),
            )?;
            registration.child_armed = true;
        }
        Ok(())
    }

    pub(super) fn decode(token: u64) -> (TabId, u32) {
        ((token >> 32) as TabId, token as u32)
    }
}

impl Registration {
    pub(super) fn ready(&mut self, sequence: u32) -> Option<SessionReady> {
        if sequence == 0 {
            return Some(SessionReady {
                pty: false,
                child_exit: true,
            });
        }
        if sequence != self.pty_sequence || self.pty_events.take().is_none() {
            return None;
        }
        Some(SessionReady {
            pty: true,
            child_exit: false,
        })
    }
}

fn token(id: TabId, sequence: u32) -> u64 {
    (u64::from(id) << 32) | u64::from(sequence)
}

impl Drop for Readiness {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn control(
    epoll: &OwnedFd,
    operation: libc::c_int,
    fd: BorrowedFd<'_>,
    events: u32,
    token: u64,
) -> io::Result<()> {
    let mut event = libc::epoll_event {
        events: events | libc::EPOLLONESHOT as u32,
        u64: token,
    };
    // SAFETY: The descriptors are borrowed for the call and the event is initialized.
    if unsafe { libc::epoll_ctl(epoll.as_raw_fd(), operation, fd.as_raw_fd(), &mut event) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn wait(epoll: &OwnedFd, _stop: UnixStream, proxy: EventLoopProxy<Event>) {
    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 16];
    loop {
        // SAFETY: epoll owns the interest list and events can hold the requested results.
        let count = unsafe {
            libc::epoll_wait(
                epoll.as_raw_fd(),
                events.as_mut_ptr(),
                events.len() as i32,
                -1,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            let _ = proxy.send_event(Event::PtyError(error));
            break;
        }
        for event in &events[..count as usize] {
            let token = event.u64;
            if token == STOP || proxy.send_event(Event::PtyReady(token)).is_err() {
                return;
            }
        }
    }
}
