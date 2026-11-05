use std::{error::Error, fmt};

use lune_vt::{CsiSequence, EscapeSequence, Event, Parameter, Parser};

use crate::charsets::Charsets;
use crate::{EraseMode, InputModes, MAX_STYLES, Screen, Style, StyleError, StyleId, StyleTable};

/// A failure while applying a completed text or rendition event.
#[derive(Debug)]
pub enum ProcessFailure<E> {
    Text(E),
    Style(StyleError),
}

impl<E: fmt::Display> fmt::Display for ProcessFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(error) => write!(f, "Text processing failed: {error}"),
            Self::Style(error) => error.fmt(f),
        }
    }
}

impl<E: Error + 'static> Error for ProcessFailure<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(match self {
            Self::Text(error) => error,
            Self::Style(error) => error,
        })
    }
}

/// An execution failure and the input prefix already owned by the processor.
/// Retry with `input[consumed..]`; retained events are applied before new input.
#[derive(Debug)]
pub struct ProcessError<E> {
    pub consumed: usize,
    pub error: ProcessFailure<E>,
}

impl<E: fmt::Display> fmt::Display for ProcessError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Processing failed after consuming {} bytes: {}",
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
    Rendition(Style),
}

#[derive(Default)]
struct State {
    style: StyleId,
    saved_style: StyleId,
    charsets: Charsets,
    saved_charsets: Charsets,
    input: InputModes,
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
/// the fixed-capacity parser. Rendition changes use a bounded, owned style table.
/// Include retained screen/rendering snapshots when collecting style storage.
/// Screen edits use keys from this processor's table; callbacks receive the
/// current key. Default construction permits up to [`MAX_STYLES`] entries.
/// G0/G1 designation supports ASCII (`B`) and DEC Special Graphics (`0`), selected
/// by SI/SO. Graphics bytes map to Unicode scalars before text callbacks; literal
/// non-ASCII Unicode retains its identity. Cursor saves include both designated
/// sets and their selection. Reset restores both sets to ASCII and selects G0.
pub struct Processor {
    parser: Parser,
    styles: StyleTable,
    state: State,
    pending: [Option<Pending>; 2],
    head: usize,
    end: usize,
    parse_errors: u64,
    ignored_sequences: u64,
}

impl Default for Processor {
    fn default() -> Self {
        Self::new(MAX_STYLES).expect("The default style limit is valid")
    }
}

impl Processor {
    /// Starts with default rendition and a bounded style table. Construction
    /// makes no allocation requests; the limit includes permanent default.
    pub fn new(style_limit: usize) -> Result<Self, StyleError> {
        Ok(Self {
            parser: Parser::default(),
            styles: StyleTable::new(style_limit)?,
            state: State::default(),
            pending: [None; 2],
            head: 0,
            end: 0,
            parse_errors: 0,
            ignored_sequences: 0,
        })
    }

    pub fn style_id(&self) -> StyleId {
        self.state.style
    }

    /// Keyboard modes in effect at the end of the accepted input prefix.
    pub fn input_modes(&self) -> InputModes {
        self.state.input
    }

    /// Resolves keys issued for this processor's screen and retained snapshots.
    pub fn styles(&self) -> &StyleTable {
        &self.styles
    }

    /// Traces current/saved rendition and the paired screen automatically.
    /// Supply every externally retained ID as an additional root. Reclaimed
    /// history can make room for a pending rendition change before retrying it.
    pub fn collect_styles(
        &mut self,
        screen: &Screen,
        external: impl IntoIterator<Item = StyleId>,
    ) -> Result<usize, StyleError> {
        self.styles.collect(
            [self.state.style, self.state.saved_style]
                .into_iter()
                .chain(
                    screen
                        .grid()
                        .iter_rows()
                        .flat_map(|row| row.cells().iter().map(|c| c.style())),
                )
                .chain(external),
        )
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
    /// A rendition storage failure retains the complete candidate style and
    /// consumes its sequence, leaving current rendition unchanged until retry.
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
                let first = input[consumed];
                if (b' '..=b'~').contains(&first) {
                    let length = if self.state.charsets.is_graphics() {
                        if first >= b'_' {
                            let character = self.state.charsets.map(char::from(first));
                            consumed += 1;
                            if let Err(error) =
                                text(screen, character.encode_utf8(&mut [0; 4]), self.state.style)
                            {
                                self.pending[0] = Some(Pending::Text(character));
                                self.end = 1;
                                return Err(ProcessError {
                                    consumed,
                                    error: ProcessFailure::Text(error),
                                });
                            }
                            continue;
                        }
                        ascii_prefix::<b'^'>(&input[consumed..])
                    } else {
                        ascii_prefix::<b'~'>(&input[consumed..])
                    };
                    let run = std::str::from_utf8(&input[consumed..consumed + length])
                        .expect("The run is printable ASCII");
                    text(screen, run, self.state.style).map_err(|error| ProcessError {
                        consumed,
                        error: ProcessFailure::Text(error),
                    })?;
                    consumed += length;
                    continue;
                }
                let scalar_bytes = match first {
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
                    if let Err(error) = text(screen, value, self.state.style) {
                        self.pending[0] = Some(Pending::Text(value.chars().next().unwrap()));
                        self.end = 1;
                        return Err(ProcessError {
                            consumed,
                            error: ProcessFailure::Text(error),
                        });
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
                styles,
                state,
                ..
            } = self;
            parser.advance(&input[consumed..consumed + 1], |event| match event {
                Event::Text(value) => {
                    let mut characters = value.chars();
                    let character = characters.next().expect("A decoded scalar is present");
                    debug_assert!(characters.next().is_none());
                    pending[*end] = Some(Pending::Text(state.charsets.map(character)));
                    *end += 1;
                }
                Event::Execute(control) => {
                    pending[*end] = Some(Pending::Execute(control));
                    *end += 1;
                }
                Event::Escape(sequence) => {
                    debug_assert_eq!(*end, 0);
                    if !escape(screen, sequence, state) {
                        *ignored_sequences = ignored_sequences.saturating_add(1);
                    }
                }
                Event::Csi(sequence) => {
                    debug_assert_eq!(*end, 0);
                    let accepted = if sequence.final_byte == b'm'
                        && sequence.private_markers.is_empty()
                        && sequence.intermediates.is_empty()
                    {
                        let mut next = *styles
                            .get(state.style)
                            .expect("Current rendition is rooted");
                        if next.apply_sgr(sequence.parameters) {
                            pending[*end] = Some(Pending::Rendition(next));
                            *end += 1;
                            true
                        } else {
                            false
                        }
                    } else {
                        csi(screen, sequence, state)
                    };
                    if !accepted {
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

    /// Ends the lexical stream. Failed text or rendition remains retained for retry;
    /// repeated successful calls emit no further replacements or diagnostics.
    pub fn finish<E>(
        &mut self,
        screen: &mut Screen,
        mut text: impl FnMut(&mut Screen, &str, StyleId) -> Result<(), E>,
    ) -> Result<(), ProcessFailure<E>> {
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
    /// Reset caller-owned text state at the same stream boundary. Style storage
    /// remains available for retained snapshots until explicit collection.
    pub fn reset(&mut self, screen: &mut Screen) {
        self.parser.reset();
        self.pending = [None; 2];
        self.head = 0;
        self.end = 0;
        self.parse_errors = 0;
        self.ignored_sequences = 0;
        self.state = State::default();
        screen.reset(StyleId(0));
    }

    fn drain<E>(
        &mut self,
        screen: &mut Screen,
        text: &mut impl FnMut(&mut Screen, &str, StyleId) -> Result<(), E>,
    ) -> Result<(), ProcessFailure<E>> {
        while self.head < self.end {
            match self.pending[self.head].expect("A pending action is present") {
                Pending::Text(character) => {
                    text(screen, character.encode_utf8(&mut [0; 4]), self.state.style)
                        .map_err(ProcessFailure::Text)?
                }
                Pending::Execute(0x0e) => self.state.charsets.shift(true),
                Pending::Execute(0x0f) => self.state.charsets.shift(false),
                Pending::Execute(control) => {
                    execute(screen, control, self.state.style, self.state.input.new_line)
                }
                Pending::Rendition(style) => {
                    self.state.style = self.styles.intern(style).map_err(ProcessFailure::Style)?;
                }
            }
            self.pending[self.head] = None;
            self.head += 1;
        }
        self.head = 0;
        self.end = 0;
        Ok(())
    }
}

#[inline]
fn ascii_prefix<const LAST: u8>(input: &[u8]) -> usize {
    const ONES: u64 = u64::MAX / 255;
    const HIGH: u64 = ONES * 128;
    let mut offset = 0;
    while input.len() - offset >= 8 {
        let word = u64::from_ne_bytes(input[offset..offset + 8].try_into().unwrap());
        // Out-of-range bytes set a high bit; the scalar tail locates the boundary.
        let below = word.wrapping_sub(ONES * 32) & !word;
        let above = word.wrapping_add(ONES * u64::from(127 - LAST)) | word;
        if (below | above) & HIGH != 0 {
            break;
        }
        offset += 8;
    }
    offset
        + input[offset..]
            .iter()
            .position(|byte| !(b' '..=LAST).contains(byte))
            .unwrap_or(input.len() - offset)
}

fn execute(screen: &mut Screen, control: u8, style: StyleId, new_line_mode: bool) {
    match control {
        0x08 => screen.backspace(),
        0x09 => screen.tab_forward(1),
        0x0a..=0x0c => {
            if new_line_mode {
                screen.carriage_return();
            }
            screen.line_feed(style);
        }
        0x0d => screen.carriage_return(),
        _ => {}
    }
}

fn escape(screen: &mut Screen, sequence: EscapeSequence<'_>, state: &mut State) -> bool {
    if let [designation @ (b'(' | b')')] = sequence.intermediates {
        return state
            .charsets
            .designate(*designation == b')', sequence.final_byte);
    }
    if !sequence.intermediates.is_empty() {
        return false;
    }
    match sequence.final_byte {
        b'7' => {
            screen.save_cursor();
            state.saved_style = state.style;
            state.saved_charsets = state.charsets;
        }
        b'8' => {
            screen.restore_cursor();
            state.style = state.saved_style;
            state.charsets = state.saved_charsets;
        }
        b'D' => screen.line_feed(state.style),
        b'E' => {
            screen.carriage_return();
            screen.line_feed(state.style);
        }
        b'H' => screen.set_tab_stop(),
        b'=' => state.input.application_keypad = true,
        b'>' => state.input.application_keypad = false,
        b'M' => screen.reverse_index(state.style),
        b'c' => {
            *state = State::default();
            screen.reset(state.style);
        }
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

fn csi(screen: &mut Screen, sequence: CsiSequence<'_>, state: &mut State) -> bool {
    let style = state.style;
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
            || !p.iter().all(|p| {
                matches!(
                    (private, p.value),
                    (false, Some(4 | 20)) | (true, Some(1 | 6 | 7))
                )
            })
        {
            return false;
        }
        let enabled = sequence.final_byte == b'h';
        for parameter in p {
            match (private, parameter.value) {
                (false, Some(4)) => screen.set_insert_mode(enabled),
                (false, Some(20)) => state.input.new_line = enabled,
                (true, Some(1)) => state.input.application_cursor = enabled,
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

#[cfg(test)]
mod scan_tests {
    use super::ascii_prefix;

    #[test]
    fn word_scans_match_scalar_bounds_for_every_byte_and_alignment() {
        for (last, scan) in [
            (b'^', ascii_prefix::<b'^'> as fn(&[u8]) -> usize),
            (b'~', ascii_prefix::<b'~'>),
        ] {
            for alignment in 0..8 {
                let mut bytes = [b'A'; 48];
                for length in [0, 1, 7, 8, 9, 15, 16, 17, 31, 32, 33] {
                    assert_eq!(scan(&bytes[alignment..alignment + length]), length);
                    for position in 0..length {
                        for value in 0..=255 {
                            bytes[alignment + position] = value;
                            let input = &bytes[alignment..alignment + length];
                            let expected = input
                                .iter()
                                .position(|byte| !(b' '..=last).contains(byte))
                                .unwrap_or(length);
                            assert_eq!(scan(input), expected);
                        }
                        bytes[alignment + position] = b'A';
                    }
                }
            }
        }
    }
}
