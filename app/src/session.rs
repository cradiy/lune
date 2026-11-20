use std::io::{self, Read, Write};
use std::num::NonZeroU32;
use std::ops::Range;
use std::os::fd::{AsFd, BorrowedFd};
use std::process::{Command, ExitStatus};

use lune_pty::{Pty, PtySize};
use lune_terminal::{
    Key, KeySequence, MAX_STYLES, Modifiers, Palette, PixelSize, ProcessFailure, Processor, Screen,
    StorageError, StyleError, StyleId, TextSink, UnicodeWriter,
};

const READ_BYTES: usize = 16 * 1024;
pub const MAX_INPUT_BYTES: usize = 16 * 1024;
const PASTE_WRITE_BYTES: usize = MAX_INPUT_BYTES;
pub const MAX_PASTE_BYTES: usize = 1024 * 1024;

fn pixel_size(size: PtySize) -> Option<PixelSize> {
    let width = NonZeroU32::new(u32::from(size.pixel_width))?;
    let height = NonZeroU32::new(u32::from(size.pixel_height))?;
    Some(PixelSize { width, height })
}

enum ResizeError {
    Storage(StorageError),
    Transport(io::Error),
}

impl From<StorageError> for ResizeError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// Drives terminal state and child I/O from a nonblocking PTY.
pub struct Session<T = Pty> {
    transport: T,
    size: PtySize,
    screen: Screen,
    processor: Processor,
    writer: UnicodeWriter,
    incoming: [u8; READ_BYTES],
    incoming_bytes: Option<Range<usize>>,
    outgoing: [u8; MAX_INPUT_BYTES],
    outgoing_bytes: Range<usize>,
    large_paste: Option<Vec<u8>>,
    write_closed: bool,
    read_closed: bool,
    finished: bool,
}

impl Session<Pty> {
    pub fn spawn(command: Command, size: PtySize, palette: Palette) -> io::Result<Self> {
        let screen = Screen::new(usize::from(size.columns), usize::from(size.rows))
            .map_err(io::Error::other)?;
        let mut processor = Processor::new(MAX_STYLES, palette).map_err(io::Error::other)?;
        processor.update_geometry(&screen, pixel_size(size), None);

        let mut transport = Pty::spawn(command, size)?;
        transport.set_nonblocking(true)?;
        Ok(Self::new(transport, size, screen, processor))
    }

    /// Commits a PTY resize and reflows its screen before publishing VT geometry.
    /// A pending input chunk returns `WouldBlock`; pump it before retrying.
    pub fn resize(&mut self, size: PtySize) -> io::Result<()> {
        if self.write_closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if size == self.size {
            return Ok(());
        }
        if self.incoming_bytes.is_some() {
            return Err(io::ErrorKind::WouldBlock.into());
        }

        let grid_changed = size.columns != self.size.columns || size.rows != self.size.rows;
        self.screen
            .reflow_with(
                usize::from(size.columns),
                usize::from(size.rows),
                StyleId::default(),
                || {
                    self.transport
                        .resize(size)
                        .map_err(ResizeError::Transport)?;
                    if grid_changed {
                        self.writer.boundary();
                    }
                    Ok(())
                },
            )
            .map_err(|error| match error {
                ResizeError::Storage(error) => io::Error::other(error),
                ResizeError::Transport(error) => error,
            })?;
        self.size = size;
        self.processor
            .update_geometry(&self.screen, pixel_size(size), None);
        Ok(())
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
    fn new(transport: T, size: PtySize, screen: Screen, processor: Processor) -> Self {
        Self {
            transport,
            size,
            screen,
            processor,
            writer: UnicodeWriter::default(),
            incoming: [0; READ_BYTES],
            incoming_bytes: None,
            outgoing: [0; MAX_INPUT_BYTES],
            outgoing_bytes: 0..0,
            large_paste: None,
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
        self.check_input_ready()?;
        self.outgoing[..bytes.len()].copy_from_slice(bytes);
        self.outgoing_bytes = 0..bytes.len();
        Ok(())
    }

    /// Queues a key using current VT modes. Returns false for a suppressed event.
    pub fn queue_key(&mut self, key: Key, modifiers: Modifiers, repeat: bool) -> io::Result<bool> {
        self.queue_key_sequence(
            self.processor.input_modes().encode_key(key, modifiers),
            repeat,
        )
    }

    /// Queues a layout-resolved character with its Control and Alt modifiers.
    pub fn queue_character(
        &mut self,
        character: char,
        modifiers: Modifiers,
        repeat: bool,
    ) -> io::Result<bool> {
        self.queue_key_sequence(KeySequence::from_character(character, modifiers), repeat)
    }

    fn queue_key_sequence(
        &mut self,
        sequence: Option<KeySequence>,
        repeat: bool,
    ) -> io::Result<bool> {
        let Some(sequence) = sequence else {
            return Ok(false);
        };
        if repeat && !self.processor.auto_repeat_enabled() {
            return Ok(false);
        }
        self.queue_input(sequence.as_bytes())?;
        Ok(true)
    }

    /// Queues up to `MAX_PASTE_BYTES` of text with the current framing.
    /// The entire paste precedes later replies.
    pub fn queue_paste(&mut self, payload: &str) -> io::Result<()> {
        if payload.len() > MAX_PASTE_BYTES {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        self.check_input_ready()?;

        let mut paste = self.processor.input_modes().encode_paste(payload);
        let len = paste.remaining_bytes();
        let mut large_paste = if len > self.outgoing.len() {
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(len).map_err(io::Error::other)?;
            Some(bytes)
        } else {
            None
        };
        let mut offset = 0;
        while !paste.is_empty() {
            let bytes = paste.bytes();
            let end = offset + bytes.len();
            if let Some(buffer) = &mut large_paste {
                buffer.extend_from_slice(bytes);
            } else {
                self.outgoing[offset..end].copy_from_slice(bytes);
            }
            let consumed = paste.consume(bytes.len());
            debug_assert!(consumed);
            offset = end;
        }
        debug_assert_eq!(offset, len);
        self.large_paste = large_paste;
        self.outgoing_bytes = 0..len;
        Ok(())
    }

    fn check_input_ready(&self) -> io::Result<()> {
        if self.write_closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if self.wants_write() || self.incoming_bytes.is_some() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(())
    }

    /// Writes at most 16 KiB of paste data and reads at most one chunk.
    /// Use level-triggered notifications and also pump on hangup.
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
        let mut paste_budget = PASTE_WRITE_BYTES;
        loop {
            // Keep each host frame contiguous on the wire, including paste markers.
            let host_input = !self.outgoing_bytes.is_empty();
            let bytes = if host_input {
                let range = self.outgoing_bytes.clone();
                if let Some(paste) = &self.large_paste {
                    let end = if self.write_closed {
                        range.end
                    } else {
                        range.end.min(range.start + paste_budget)
                    };
                    &paste[range.start..end]
                } else {
                    &self.outgoing[range]
                }
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
                if self.outgoing_bytes.is_empty() {
                    self.large_paste = None;
                } else if self.large_paste.is_some() && !self.write_closed {
                    paste_budget -= count;
                    if paste_budget == 0 {
                        return Ok(false);
                    }
                }
            } else {
                let consumed = self.processor.consume_reply(count);
                debug_assert!(consumed);
            }
        }
    }
}

#[cfg(test)]
mod tests;
