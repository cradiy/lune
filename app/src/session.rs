use std::io::{self, Read, Write};
use std::num::NonZeroU32;
use std::ops::Range;
use std::os::fd::{AsFd, BorrowedFd};
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

use lune_pty::{ChildExit, Pty, PtySize};
use lune_terminal::{
    CellPosition, ColorScheme, HistoryLimits, Key, KeySequence, MAX_STYLES, Modifiers, MouseEvent,
    MouseFormat, MouseSequence, MouseTracking, Palette, PixelPosition, PixelSize, ProcessFailure,
    Processor, Screen, ScreenBuffer, StorageError, StyleError, StyleId, TerminalNotifications,
    TextSink, UnicodeWriter,
};

mod wait;

pub use wait::WaitOutcome;

const READ_BYTES: usize = 16 * 1024;
const SYNCHRONIZED_OUTPUT_TIMEOUT: Duration = Duration::from_millis(150);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseInputOutcome {
    Ignored,
    Queued,
    HistoryScrolled,
}

/// Drives terminal state and child I/O from a nonblocking PTY.
pub struct Session<T = Pty> {
    transport: T,
    child_exit: Option<ChildExit>,
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
    presentation_deadline: Option<Instant>,
}

impl Session<Pty> {
    /// Starts a session with a child exit monitor on Linux 5.3 or later.
    pub fn spawn(command: Command, size: PtySize, palette: Palette) -> io::Result<Self> {
        let screen = Screen::new(usize::from(size.columns), usize::from(size.rows))
            .map_err(io::Error::other)?;
        let mut processor = Processor::new(MAX_STYLES, palette).map_err(io::Error::other)?;
        processor.update_geometry(&screen, pixel_size(size), None);

        let mut transport = Pty::spawn(command, size)?;
        transport.set_nonblocking(true)?;
        let child_exit = transport.monitor_child()?;
        let mut session = Self::new(transport, size, screen, processor);
        session.child_exit = Some(child_exit);
        Ok(session)
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
            child_exit: None,
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
            presentation_deadline: None,
        }
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// The live screen when it is ready for presentation.
    pub fn presentation_screen(&self) -> Option<&Screen> {
        self.presentation_deadline.is_none().then_some(&self.screen)
    }

    /// When to pump again to release a synchronized frame, even if the PTY is idle.
    pub fn presentation_deadline(&self) -> Option<Instant> {
        self.presentation_deadline
    }

    pub fn processor(&self) -> &Processor {
        &self.processor
    }

    pub fn take_notifications(&mut self) -> TerminalNotifications<'_> {
        self.processor.take_notifications()
    }

    pub fn set_theme(&mut self, palette: &Palette, scheme: Option<ColorScheme>) {
        self.processor.set_theme(palette, scheme);
    }

    /// Updates primary scrollback limits and clamps the visible history.
    pub fn set_history_limits(&mut self, limits: HistoryLimits) -> Result<(), StorageError> {
        self.screen.set_history_limits(limits)
    }

    /// Moves the local viewport. Positive steps go toward older rows.
    pub fn scroll_history(&mut self, steps: isize) -> bool {
        let offset = self.screen.viewport_offset();
        if steps > 0 {
            self.screen.scroll_viewport_up(steps as usize);
        } else {
            self.screen.scroll_viewport_down(steps.unsigned_abs());
        }
        self.screen.viewport_offset() != offset
    }

    /// Returns the primary viewport to the live buffer.
    pub fn follow_output(&mut self) {
        self.screen.scroll_viewport_to_bottom();
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
        self.stage_input(bytes);
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

    /// Queues a focus transition when focus reporting is enabled.
    pub fn queue_focus(&mut self, focused: bool) -> io::Result<bool> {
        self.check_input_ready()?;
        Ok(self.stage_report(self.processor.input_modes().encode_focus(focused)))
    }

    /// Routes a pointer event to the primary viewport or the PTY.
    /// A historical viewport handles vertical wheel steps locally.
    /// Pixel reports require a position inside the committed content size.
    pub fn mouse_input(
        &mut self,
        event: MouseEvent,
        cell: CellPosition,
        pixel: Option<PixelPosition>,
        modifiers: Modifiers,
    ) -> io::Result<MouseInputOutcome> {
        if cell.row >= usize::from(self.size.rows) || cell.column >= usize::from(self.size.columns)
        {
            return Ok(MouseInputOutcome::Ignored);
        }
        let modes = self.processor.input_modes();
        let buffer = self.screen.active_buffer();
        if buffer == ScreenBuffer::Primary
            && (modes.mouse_tracking == MouseTracking::Disabled
                || self.write_closed
                || self.screen.viewport_offset() != 0)
        {
            let steps = match event {
                MouseEvent::WheelUp => 1,
                MouseEvent::WheelDown => -1,
                _ => return Ok(MouseInputOutcome::Ignored),
            };
            return Ok(if self.scroll_history(steps) {
                MouseInputOutcome::HistoryScrolled
            } else {
                MouseInputOutcome::Ignored
            });
        }

        self.check_input_ready()?;
        if modes.mouse_tracking == MouseTracking::Disabled {
            let key = modes.encode_alternate_scroll(event, buffer);
            return Ok(self.stage_mouse_report(key.as_ref().map(KeySequence::as_bytes)));
        }
        let report = if modes.mouse_format == MouseFormat::SgrPixels {
            let Some(pixel) = pixel.filter(|position| {
                position.x < u32::from(self.size.pixel_width)
                    && position.y < u32::from(self.size.pixel_height)
            }) else {
                return Ok(MouseInputOutcome::Ignored);
            };
            modes.encode_pixel_mouse(event, pixel, modifiers)
        } else {
            modes.encode_mouse(event, cell, modifiers)
        };
        Ok(self.stage_mouse_report(report.as_ref().map(MouseSequence::as_bytes)))
    }

    fn stage_mouse_report(&mut self, report: Option<&[u8]>) -> MouseInputOutcome {
        if self.stage_report(report) {
            MouseInputOutcome::Queued
        } else {
            MouseInputOutcome::Ignored
        }
    }

    fn stage_report(&mut self, report: Option<&[u8]>) -> bool {
        if let Some(bytes) = report {
            self.stage_input(bytes);
            true
        } else {
            false
        }
    }

    fn stage_input(&mut self, bytes: &[u8]) {
        self.outgoing[..bytes.len()].copy_from_slice(bytes);
        self.outgoing_bytes = 0..bytes.len();
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
        self.pump_with_clock(Instant::now)
    }

    fn pump_with_clock(&mut self, now: impl Fn() -> Instant) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        if self
            .presentation_deadline
            .is_some_and(|deadline| now() >= deadline)
        {
            self.processor.end_synchronized_output();
            self.presentation_deadline = None;
            return Ok(());
        }

        let result = self.pump_pty();
        if self.processor.synchronized_output() {
            self.presentation_deadline
                .get_or_insert_with(|| now() + SYNCHRONIZED_OUTPUT_TIMEOUT);
        } else {
            self.presentation_deadline = None;
        }
        result
    }

    fn pump_pty(&mut self) -> io::Result<()> {
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
            self.processor.end_synchronized_output();
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
