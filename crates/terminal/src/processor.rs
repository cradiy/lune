use std::{error::Error, fmt};

use lune_vt::{CsiSequence, EscapeSequence, Event, Parameter, Parser};

use crate::{EraseMode, Screen, StyleId};

/// A text callback failure and the input prefix already owned by the processor.
/// Retry with `input[consumed..]`; retained events are applied before new input.
#[derive(Debug)]
pub struct ProcessError<E> {
    pub consumed: usize,
    pub error: E,
}

impl<E: fmt::Display> fmt::Display for ProcessError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Text processing failed after consuming {} bytes: {}",
            self.consumed, self.error
        )
    }
}

impl<E: Error + 'static> Error for ProcessError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.error)
    }
}

#[derive(Clone, Copy)]
enum Pending {
    Text(char),
    Execute(u8),
}

/// Incremental execution of screen controls with caller-owned text layout.
/// Pair a processor with one screen and text writer across calls. Text callbacks
/// receive a printable ASCII run or one Unicode scalar, including replacements
/// for malformed UTF-8. A callback must preserve the screen and accepted-text
/// state on error. Apply accepted text synchronously and reconcile layout state
/// with screen control changes. The writer supplies display units and widths.
///
/// A byte can complete a replacement and a second text/control event. Two fixed
/// slots retain these events across writer failures; sequence headers remain in
/// the fixed-capacity parser. Execution and buffering make no allocation requests.
#[derive(Default)]
pub struct Processor {
    parser: Parser,
    style: StyleId,
    pending: [Option<Pending>; 2],
    head: usize,
    end: usize,
    parse_errors: u64,
    ignored_sequences: u64,
}

impl Processor {
    /// Sets the style key used for text and clearing operations.
    pub fn new(style: StyleId) -> Self {
        Self {
            style,
            ..Self::default()
        }
    }

    pub fn parse_errors(&self) -> u64 {
        self.parse_errors
    }

    /// Number of ESC/CSI sequences with an unsupported header or parameter form.
    pub fn ignored_sequences(&self) -> u64 {
        self.ignored_sequences
    }

    /// Consumes input and returns its byte count on success. A successful call
    /// has applied every completed event; unfinished sequences remain buffered.
    ///
    /// On error, `consumed` includes bytes buffered in the parser or retained as
    /// events. An ASCII run rejected by the writer remains unconsumed. A failed
    /// decoded scalar is retained, including any following event from its byte.
    /// Retry the suffix even when empty. Repeated failures consume zero new bytes.
    pub fn advance<E>(
        &mut self,
        input: &[u8],
        screen: &mut Screen,
        mut text: impl FnMut(&mut Screen, &str, StyleId) -> Result<(), E>,
    ) -> Result<usize, ProcessError<E>> {
        self.drain(screen, &mut text)
            .map_err(|error| ProcessError { consumed: 0, error })?;
        let mut consumed = 0;
        while consumed < input.len() {
            if self.parser.is_ground() {
                let length = input[consumed..]
                    .iter()
                    .position(|byte| !(b' '..=b'~').contains(byte))
                    .unwrap_or(input.len() - consumed);
                if length != 0 {
                    let run = std::str::from_utf8(&input[consumed..consumed + length])
                        .expect("The run is printable ASCII");
                    text(screen, run, self.style)
                        .map_err(|error| ProcessError { consumed, error })?;
                    consumed += length;
                    continue;
                }
                let scalar_bytes = match input[consumed] {
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => 0,
                };
                if scalar_bytes != 0
                    && let Some(bytes) = input[consumed..].get(..scalar_bytes)
                    && let Ok(value) = std::str::from_utf8(bytes)
                {
                    consumed += scalar_bytes;
                    if let Err(error) = text(screen, value, self.style) {
                        self.pending[0] = Some(Pending::Text(value.chars().next().unwrap()));
                        self.end = 1;
                        return Err(ProcessError { consumed, error });
                    }
                    continue;
                }
            }
            let discarded = self.parser.discard_string_payload(&input[consumed..]);
            if discarded != 0 {
                consumed += discarded;
                continue;
            }
            let Self {
                parser,
                pending,
                end,
                parse_errors,
                ignored_sequences,
                style,
                ..
            } = self;
            parser.advance(&input[consumed..consumed + 1], |event| match event {
                Event::Text(value) => {
                    let mut characters = value.chars();
                    let character = characters.next().expect("A decoded scalar is present");
                    debug_assert!(characters.next().is_none());
                    pending[*end] = Some(Pending::Text(character));
                    *end += 1;
                }
                Event::Execute(control) => {
                    pending[*end] = Some(Pending::Execute(control));
                    *end += 1;
                }
                Event::Escape(sequence) => {
                    debug_assert_eq!(*end, 0);
                    if !escape(screen, sequence, *style) {
                        *ignored_sequences = ignored_sequences.saturating_add(1);
                    }
                }
                Event::Csi(sequence) => {
                    debug_assert_eq!(*end, 0);
                    if !csi(screen, sequence, *style) {
                        *ignored_sequences = ignored_sequences.saturating_add(1);
                    }
                }
                Event::Error(_) => *parse_errors = parse_errors.saturating_add(1),
            });
            consumed += 1;
            self.drain(screen, &mut text)
                .map_err(|error| ProcessError { consumed, error })?;
        }
        Ok(consumed)
    }

    /// Ends the lexical stream. Failed final text remains retained for retry;
    /// repeated successful calls emit no further replacements or diagnostics.
    pub fn finish<E>(
        &mut self,
        screen: &mut Screen,
        mut text: impl FnMut(&mut Screen, &str, StyleId) -> Result<(), E>,
    ) -> Result<(), E> {
        self.drain(screen, &mut text)?;
        let Self {
            parser,
            pending,
            end,
            parse_errors,
            ..
        } = self;
        parser.finish(|event| match event {
            Event::Text(_) => {
                pending[*end] = Some(Pending::Text(char::REPLACEMENT_CHARACTER));
                *end += 1;
            }
            Event::Error(_) => *parse_errors = parse_errors.saturating_add(1),
            _ => unreachable!("Finishing emits text or a lexical error"),
        });
        self.drain(screen, &mut text)
    }

    /// Discards buffered input and events, clears diagnostics, and resets the screen.
    /// Reset caller-owned text state at the same stream boundary.
    pub fn reset(&mut self, screen: &mut Screen) {
        let style = self.style;
        *self = Self::new(style);
        screen.reset(style);
    }

    fn drain<E>(
        &mut self,
        screen: &mut Screen,
        text: &mut impl FnMut(&mut Screen, &str, StyleId) -> Result<(), E>,
    ) -> Result<(), E> {
        while self.head < self.end {
            match self.pending[self.head].expect("A pending action is present") {
                Pending::Text(character) => {
                    text(screen, character.encode_utf8(&mut [0; 4]), self.style)?
                }
                Pending::Execute(control) => execute(screen, control, self.style),
            }
            self.pending[self.head] = None;
            self.head += 1;
        }
        self.head = 0;
        self.end = 0;
        Ok(())
    }
}

fn execute(screen: &mut Screen, control: u8, style: StyleId) {
    match control {
        0x08 => screen.backspace(),
        0x09 => screen.tab_forward(1),
        0x0a..=0x0c => screen.line_feed(style),
        0x0d => screen.carriage_return(),
        _ => {}
    }
}

fn escape(screen: &mut Screen, sequence: EscapeSequence<'_>, style: StyleId) -> bool {
    if !sequence.intermediates.is_empty() {
        return false;
    }
    match sequence.final_byte {
        b'7' => screen.save_cursor(),
        b'8' => screen.restore_cursor(),
        b'D' => screen.line_feed(style),
        b'E' => {
            screen.carriage_return();
            screen.line_feed(style);
        }
        b'H' => screen.set_tab_stop(),
        b'M' => screen.reverse_index(style),
        b'c' => screen.reset(style),
        _ => return false,
    }
    true
}

fn value(parameters: &[Parameter], index: usize, default: usize) -> usize {
    parameters
        .get(index)
        .and_then(|p| p.value)
        .map(usize::from)
        .unwrap_or(default)
}

fn count(parameters: &[Parameter]) -> usize {
    value(parameters, 0, 1).max(1)
}

fn erase_mode(value: usize) -> Option<EraseMode> {
    match value {
        0 => Some(EraseMode::FromCursor),
        1 => Some(EraseMode::ToCursor),
        2 => Some(EraseMode::All),
        _ => None,
    }
}

fn csi(screen: &mut Screen, sequence: CsiSequence<'_>, style: StyleId) -> bool {
    let p = sequence.parameters;
    if !sequence.intermediates.is_empty() || p.iter().any(|p| p.is_subparameter) {
        return false;
    }
    if matches!(sequence.final_byte, b'h' | b'l') {
        let private = match sequence.private_markers {
            [] => false,
            [b'?'] => true,
            _ => return false,
        };
        if p.is_empty()
            || !p
                .iter()
                .all(|p| matches!((private, p.value), (false, Some(4)) | (true, Some(6 | 7))))
        {
            return false;
        }
        let enabled = sequence.final_byte == b'h';
        for parameter in p {
            match (private, parameter.value) {
                (false, Some(4)) => screen.set_insert_mode(enabled),
                (true, Some(6)) => screen.set_origin_mode(enabled),
                (true, Some(7)) => screen.set_auto_wrap(enabled),
                _ => unreachable!("Mode parameters were validated"),
            }
        }
        return true;
    }
    if !sequence.private_markers.is_empty() {
        return false;
    }
    let maximum = if matches!(sequence.final_byte, b'H' | b'f' | b'r') {
        2
    } else {
        1
    };
    if p.len() > maximum {
        return false;
    }
    let n = count(p);
    match sequence.final_byte {
        b'A' => screen.move_cursor_up(n),
        b'B' => screen.move_cursor_down(n),
        b'C' => screen.move_cursor_right(n),
        b'D' => screen.move_cursor_left(n),
        b'E' => {
            screen.move_cursor_down(n);
            screen.carriage_return();
        }
        b'F' => {
            screen.move_cursor_up(n);
            screen.carriage_return();
        }
        b'G' => {
            let row = screen.cursor().row
                - if screen.origin_mode() {
                    screen.scroll_region().start
                } else {
                    0
                };
            screen.set_cursor(row, n - 1);
        }
        b'd' => screen.set_cursor(n - 1, screen.cursor().column),
        b'H' | b'f' => screen.set_cursor(value(p, 0, 1).max(1) - 1, value(p, 1, 1).max(1) - 1),
        b'I' => screen.tab_forward(n),
        b'Z' => screen.tab_backward(n),
        b'@' => screen.insert_cells(n, style),
        b'P' => screen.delete_cells(n, style),
        b'X' => screen.erase_cells(n, style),
        b'L' => screen.insert_lines(n, style),
        b'M' => screen.delete_lines(n, style),
        b'S' => screen.scroll_up(n, style),
        b'T' => screen.scroll_down(n, style),
        b'J' | b'K' => {
            let Some(mode) = erase_mode(value(p, 0, 0)) else {
                return false;
            };
            if sequence.final_byte == b'J' {
                screen.erase_display(mode, style);
            } else {
                screen.erase_line(mode, style);
            }
        }
        b'g' => match value(p, 0, 0) {
            0 => screen.clear_tab_stop(),
            3 => screen.clear_tab_stops(),
            _ => return false,
        },
        b'r' => {
            let top = value(p, 0, 1).max(1);
            let bottom = match value(p, 1, screen.grid().rows()) {
                0 => screen.grid().rows(),
                n => n,
            };
            if screen.set_scroll_region(top - 1..bottom).is_err() {
                return false;
            }
        }
        _ => return false,
    }
    true
}
