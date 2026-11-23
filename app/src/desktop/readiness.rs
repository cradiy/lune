use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::{SessionInterests, SessionReady};
use winit::event_loop::EventLoopProxy;

use super::Event;

const STOP: u64 = 0;
const CHILD: u64 = 1;

pub(super) struct Readiness {
    epoll: Arc<OwnedFd>,
    stop: Option<UnixStream>,
    worker: Option<JoinHandle<()>>,
    pty_events: Option<u32>,
    pty_token: u64,
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
            pty_events: None,
            pty_token: CHILD,
            child_armed: false,
        })
    }

    pub(super) fn arm(&mut self, interests: SessionInterests<'_>) -> io::Result<()> {
        if let Some(pty) = interests.pty {
            let events = (if pty.read { libc::EPOLLIN } else { 0 })
                | (if pty.write { libc::EPOLLOUT } else { 0 });
            let events = events as u32;
            if self.pty_events != Some(events) {
                let operation = if self.pty_token == CHILD {
                    libc::EPOLL_CTL_ADD
                } else {
                    libc::EPOLL_CTL_MOD
                };
                self.pty_token += 1;
                control(&self.epoll, operation, pty.fd, events, self.pty_token)?;
                self.pty_events = Some(events);
            }
        } else {
            self.pty_events = None;
        }
        if let Some(child) = interests.child_exit
            && !self.child_armed
        {
            control(
                &self.epoll,
                libc::EPOLL_CTL_ADD,
                child,
                libc::EPOLLIN as u32,
                CHILD,
            )?;
            self.child_armed = true;
        }
        Ok(())
    }

    pub(super) fn ready(&mut self, token: u64) -> Option<SessionReady> {
        if token == CHILD {
            return Some(SessionReady {
                pty: false,
                child_exit: true,
            });
        }
        if token != self.pty_token || self.pty_events.take().is_none() {
            return None;
        }
        Some(SessionReady {
            pty: true,
            child_exit: false,
        })
    }
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
    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 3];
    loop {
        // SAFETY: epoll owns the interest list and events holds room for three results.
        let count = unsafe { libc::epoll_wait(epoll.as_raw_fd(), events.as_mut_ptr(), 3, -1) };
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
