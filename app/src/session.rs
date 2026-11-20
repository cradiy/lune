use std::io::{self, Read, Write};
use std::num::NonZeroU32;
use std::ops::Range;
use std::os::fd::{AsFd, BorrowedFd};
use std::process::{Command, ExitStatus};

use lune_pty::{Pty, PtySize};
use lune_terminal::{
    MAX_STYLES, Palette, PixelSize, ProcessFailure, Processor, Screen, StyleError, StyleId,
    UnicodeWriter,
};

const READ_BYTES: usize = 16 * 1024;
pub const MAX_INPUT_BYTES: usize = 16 * 1024;

/// Drives terminal state and child I/O from a nonblocking PTY.
pub struct Session<T = Pty> {
    transport: T,
    screen: Screen,
    processor: Processor,
    writer: UnicodeWriter,
    incoming: [u8; READ_BYTES],
    incoming_bytes: Option<Range<usize>>,
    outgoing: [u8; MAX_INPUT_BYTES],
    outgoing_bytes: Range<usize>,
    write_closed: bool,
    read_closed: bool,
    finished: bool,
}

impl Session<Pty> {
    pub fn spawn(command: Command, size: PtySize, palette: Palette) -> io::Result<Self> {
        let screen = Screen::new(usize::from(size.columns), usize::from(size.rows))
            .map_err(io::Error::other)?;
        let mut processor = Processor::new(MAX_STYLES, palette).map_err(io::Error::other)?;
        let pixel_size = NonZeroU32::new(u32::from(size.pixel_width))
            .zip(NonZeroU32::new(u32::from(size.pixel_height)))
            .map(|(width, height)| PixelSize { width, height });
        processor.update_geometry(&screen, pixel_size, None);

        let mut transport = Pty::spawn(command, size)?;
        transport.set_nonblocking(true)?;
        Ok(Self::new(transport, screen, processor))
    }

    /// Polls child status independently of whether PTY output has reached EOF.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.transport.try_wait()
    }
}

impl<T: AsFd> AsFd for Session<T> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.transport.as_fd()
    }
}

impl<T: Read + Write> Session<T> {
    fn new(transport: T, screen: Screen, processor: Processor) -> Self {
        Self {
            transport,
            screen,
            processor,
            writer: UnicodeWriter::default(),
            incoming: [0; READ_BYTES],
            incoming_bytes: None,
            outgoing: [0; MAX_INPUT_BYTES],
            outgoing_bytes: 0..0,
            write_closed: false,
            read_closed: false,
            finished: false,
        }
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    pub fn processor(&self) -> &Processor {
        &self.processor
    }

    /// Reclaims unreachable styles while preserving IDs held by the caller.
    pub fn collect_styles(
        &mut self,
        retained: impl IntoIterator<Item = StyleId>,
    ) -> Result<usize, StyleError> {
        self.processor.collect_styles(&self.screen, retained)
    }

    pub fn wants_read(&self) -> bool {
        !self.read_closed && self.incoming_bytes.is_none()
    }

    pub fn wants_write(&self) -> bool {
        !self.write_closed
            && (!self.outgoing_bytes.is_empty() || !self.processor.reply_bytes().is_empty())
    }

    /// Returns whether PTY output reached EOF and VT finalization succeeded.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Accepts a complete input frame when earlier input and replies have drained.
    pub fn queue_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if self.write_closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if self.wants_write() || self.incoming_bytes.is_some() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.outgoing[..bytes.len()].copy_from_slice(bytes);
        self.outgoing_bytes = 0..bytes.len();
        Ok(())
    }

    /// Flushes pending writes and reads at most one chunk. A readiness-driven
    /// caller should use level-triggered notifications and also pump on hangup.
    pub fn pump(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }

        let mut writable = self.flush_output()?;
        self.apply_input(&mut writable)?;
        if self.incoming_bytes.is_some() {
            return Ok(());
        }

        if !self.read_closed {
            loop {
                match self.transport.read(&mut self.incoming) {
                    Ok(0) => break,
                    Ok(count) => {
                        self.incoming_bytes = Some(0..count);
                        return self.apply_input(&mut writable);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
            self.read_closed = true;
            self.write_closed = true;
            self.flush_output()?;
        }

        self.processor
            .finish(&mut self.screen, &mut self.writer)
            .map_err(io::Error::other)?;
        self.finished = true;
        Ok(())
    }

    fn apply_input(&mut self, writable: &mut bool) -> io::Result<()> {
        while let Some(bytes) = &mut self.incoming_bytes {
            match self.processor.advance(
                &self.incoming[bytes.clone()],
                &mut self.screen,
                &mut self.writer,
            ) {
                Ok(_) => self.incoming_bytes = None,
                Err(error) => {
                    // An empty suffix can still have a pending processor event.
                    bytes.start += error.consumed;
                    if !matches!(error.error, ProcessFailure::ReplyPending) {
                        return Err(io::Error::other(error.error));
                    }
                }
            }
            if !*writable {
                break;
            }
            *writable = self.flush_output()?;
        }
        Ok(())
    }

    fn flush_output(&mut self) -> io::Result<bool> {
        loop {
            // Complete any partially written host frame before sending a reply.
            let host_input = !self.outgoing_bytes.is_empty();
            let bytes = if host_input {
                &self.outgoing[self.outgoing_bytes.clone()]
            } else {
                self.processor.reply_bytes()
            };
            if bytes.is_empty() {
                return Ok(true);
            }
            let count = if self.write_closed {
                bytes.len()
            } else {
                match self.transport.write(bytes) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(count) => count,
                    Err(error) => match error.kind() {
                        io::ErrorKind::Interrupted => continue,
                        io::ErrorKind::WouldBlock => return Ok(false),
                        io::ErrorKind::BrokenPipe => {
                            self.write_closed = true;
                            bytes.len()
                        }
                        _ => return Err(error),
                    },
                }
            };

            if host_input {
                self.outgoing_bytes.start += count;
            } else {
                let consumed = self.processor.consume_reply(count);
                debug_assert!(consumed);
            }
        }
    }
}

#[cfg(test)]
mod tests;
