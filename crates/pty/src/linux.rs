use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub columns: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl PtySize {
    pub const fn new(rows: u16, columns: u16) -> Self {
        Self {
            rows,
            columns,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    fn winsize(self) -> io::Result<libc::winsize> {
        if self.rows == 0 || self.columns == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PTY rows and columns must be positive",
            ));
        }
        Ok(libc::winsize {
            ws_row: self.rows,
            ws_col: self.columns,
            ws_xpixel: self.pixel_width,
            ws_ypixel: self.pixel_height,
        })
    }
}

#[derive(Debug)]
pub enum ChildExit {
    Pending(OwnedFd),
    Exited(ExitStatus),
}

/// Owns a Linux PTY master and its direct child. Dropping it kills and reaps
/// any remaining child and closes the master.
#[derive(Debug)]
pub struct Pty {
    master: File,
    child: Child,
}

impl Pty {
    /// Starts a new session with the PTY slave as its controlling terminal and
    /// all three standard streams attached to the slave.
    pub fn spawn(mut command: Command, size: PtySize) -> io::Result<Self> {
        let size = size.winsize()?;
        let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
        // SAFETY: The flags are valid for opening a fresh Linux PTY master.
        let master_fd = cvt(unsafe { libc::posix_openpt(flags) })?;
        // SAFETY: A successful posix_openpt returns a new owned descriptor.
        let master = unsafe { File::from_raw_fd(master_fd) };
        // SAFETY: master owns the open PTY descriptor for both calls.
        cvt(unsafe { libc::grantpt(master_fd) })?;
        // SAFETY: master still owns the open PTY descriptor.
        cvt(unsafe { libc::unlockpt(master_fd) })?;
        // SAFETY: The master is unlocked and the ioctl takes open flags by value.
        let slave_fd = cvt(unsafe { libc::ioctl(master_fd, libc::TIOCGPTPEER, flags) })?;
        // SAFETY: TIOCGPTPEER returned a new owned slave descriptor.
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        set_size(master.as_fd(), &size)?;

        command
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: The hook runs after stdio is installed and makes only system
        // calls and allocation-free errno conversions before exec.
        unsafe {
            command.pre_exec(|| {
                cvt(libc::setsid())?;
                cvt(libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0))?;
                Ok(())
            });
        }
        let child = command.spawn()?;
        Ok(Self { master, child })
    }

    pub fn size(&self) -> io::Result<PtySize> {
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: The master is open and size is writable for the duration of the ioctl.
        cvt(unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCGWINSZ, &mut size) })?;
        Ok(PtySize {
            rows: size.ws_row,
            columns: size.ws_col,
            pixel_width: size.ws_xpixel,
            pixel_height: size.ws_ypixel,
        })
    }

    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        set_size(self.master.as_fd(), &size.winsize()?)
    }

    /// Selects blocking or readiness-driven I/O on the master.
    pub fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()> {
        let fd = self.master.as_raw_fd();
        // SAFETY: F_GETFL takes only the live descriptor and command.
        let current = cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
        let next = if nonblocking {
            current | libc::O_NONBLOCK
        } else {
            current & !libc::O_NONBLOCK
        };
        if current != next {
            // SAFETY: F_SETFL takes the updated status flags by value.
            cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, next) })?;
        }
        Ok(())
    }

    pub fn child_id(&self) -> u32 {
        self.child.id()
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Opens a pollable child exit descriptor on Linux 5.3 or later.
    /// Returns the status if the child has already been reaped by this owner.
    /// Keep child reaping under this `Pty`'s ownership.
    pub fn monitor_child(&mut self) -> io::Result<ChildExit> {
        if let Some(status) = self.try_wait()? {
            return Ok(ChildExit::Exited(status));
        }
        // SAFETY: The unreaped child owns this PID and pidfd_open takes two integer arguments.
        let fd = cvt(unsafe {
            libc::syscall(libc::SYS_pidfd_open, self.child.id() as libc::pid_t, 0_u32)
                as libc::c_int
        })?;
        // SAFETY: A successful pidfd_open returns a new owned descriptor.
        Ok(ChildExit::Pending(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// Waits for the child. Continue reading while the child can still write.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }
}

impl AsFd for Pty {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }
}

impl Read for Pty {
    /// Reads the master, mapping Linux's slave-hangup EIO to EOF.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.master.read(buffer) {
            Err(error) if error.raw_os_error() == Some(libc::EIO) => Ok(0),
            result => result,
        }
    }
}

impl Write for Pty {
    /// Writes to the master, mapping a slave-hangup EIO to BrokenPipe.
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self.master.write(buffer) {
            Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, error))
            }
            result => result,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.master.flush()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) && self.child.kill().is_ok() {
            let _ = self.child.wait();
        }
    }
}

fn set_size(fd: BorrowedFd<'_>, size: &libc::winsize) -> io::Result<()> {
    // SAFETY: fd is open and the ioctl reads the initialized winsize.
    cvt(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, size) }).map(|_| ())
}

fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}
