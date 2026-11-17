use std::{error::Error, fmt};

use lune_vt::{ControlSequence, EscapeSequence, Event, OscEvent, Parameter, Parser};

use crate::charsets::Charsets;
use crate::osc::{Osc, Outcome};
use crate::reply::{self, ColorReport, ModeReport, Reply, Report};
use crate::{
    CursorShape, EraseMode, InputModes, Palette, Rgb, Screen, ScreenBuffer, StorageError, Style,
    StyleError, StyleId, StyleTable, TextSink,
};

/// A failure while applying a completed text, screen, rendition, or reply event.
#[derive(Debug)]
pub enum ProcessFailure<E> {
    Text(E),
    Screen(StorageError),
    Style(StyleError),
    /// A completed reply is waiting for the occupied output slot. Drain
    /// [`Processor::reply_bytes`] and confirm delivery with [`Processor::consume_reply`].
    ReplyPending,
}

impl<E: fmt::Display> fmt::Display for ProcessFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(error) => write!(f, "Text processing failed: {error}"),
            Self::Screen(error) => error.fmt(f),
            Self::Style(error) => error.fmt(f),
            Self::ReplyPending => f.write_str("Reply output requires draining"),
        }
    }
}

impl<E: Error + 'static> Error for ProcessFailure<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Text(error) => Some(error),
            Self::Screen(error) => Some(error),
            Self::Style(error) => Some(error),
            Self::ReplyPending => None,
        }
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
    Boundary,
    Execute(u8),
    Index(bool),
    ScrollUp(usize),
    Rendition(Style),
    Reply(Report),
    ModeReply(ModeReport),
    ColorReply(ColorReport),
    OscReplies,
}

struct State {
    style: StyleId,
    saved_style: [StyleId; 2],
    charsets: Charsets,
    saved_charsets: [Charsets; 2],
    input: InputModes,
    osc: Osc,
}

impl State {
    fn new(palette: Palette) -> Self {
        Self {
            style: StyleId(0),
            saved_style: [StyleId(0); 2],
            charsets: Charsets::default(),
            saved_charsets: [Charsets::default(); 2],
            input: InputModes::default(),
            osc: Osc::new(palette),
        }
    }

    fn reset(&mut self) {
        self.style = StyleId(0);
        self.saved_style = [StyleId(0); 2];
        self.charsets = Charsets::default();
        self.saved_charsets = [Charsets::default(); 2];
        self.input = InputModes::default();
        self.osc.reset();
    }

    #[cold]
    fn soft_reset(&mut self, screen: &mut Screen) {
        let index = screen.active_buffer() as usize;
        self.style = StyleId(0);
        self.saved_style[index] = StyleId(0);
        self.charsets = Charsets::default();
        self.saved_charsets[index] = Charsets::default();
        self.input.application_cursor = false;
        self.input.application_keypad = false;
        self.osc.reset_indexed();
        screen.soft_reset();
    }
}

/// Incremental screen execution with an atomic [`TextSink`]. Pair one processor,
/// screen, and sink across calls. Writes receive printable ASCII or one Unicode
/// scalar. Controls, ESC/DEL bytes, successful finish, and reset notify the sink
/// of text boundaries in execution order. Unfinished byte chunks retain text
/// state. [`crate::UnicodeWriter`] supplies grapheme composition and column widths;
/// closures supply complete units through the same sink contract.
///
/// A byte can complete a replacement and a second text/control/boundary event.
/// Two fixed slots retain these events across writer failures; sequence headers remain in
/// the fixed-capacity parser. Rendition changes use a bounded, owned style table.
/// Include retained screen/rendering snapshots when collecting style storage.
/// Screen edits use keys from this processor's table; callbacks receive the
/// current key.
/// G0/G1 designation supports ASCII (`B`) and DEC Special Graphics (`0`), selected
/// by SI/SO. Graphics bytes map to Unicode scalars before text callbacks; literal
/// non-ASCII Unicode retains its identity. Cursor saves include both designated
/// sets and their selection. Reset restores both sets to ASCII and selects G0.
pub struct Processor {
    parser: Parser,
    styles: StyleTable,
    state: State,
    reply: Reply,
    pending: [Option<Pending>; 2],
    head: usize,
    end: usize,
    parse_errors: u64,
    ignored_sequences: u64,
}

impl Processor {
    /// Starts with default rendition and a bounded style table. Construction
    /// makes no allocation requests; the limit includes permanent default.
    pub fn new(style_limit: usize, palette: Palette) -> Result<Self, StyleError> {
        Ok(Self {
            parser: Parser::default(),
            styles: StyleTable::new(style_limit)?,
            state: State::new(palette),
            reply: Reply::default(),
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

    /// Input modes at the end of the accepted input prefix.
    pub fn input_modes(&self) -> InputModes {
        self.state.input
    }

    /// Current OSC 0/2 window title, initially empty. Valid UTF-8 payloads up to
    /// [`crate::MAX_TITLE_BYTES`] commit at BEL/ST; rejected or cancelled commands
    /// preserve the current title. RIS and explicit reset clear it.
    pub fn window_title(&self) -> &str {
        self.state.osc.title()
    }

    /// Acknowledges changes and borrows the latest title. Intermediate changes coalesce.
    pub fn take_window_title_change(&mut self) -> Option<&str> {
        self.state.osc.take_title_change()
    }

    /// Current theme and runtime colors. OSC 10/11/12 accepts consecutive numeric
    /// RGB specifications or `?` through role 12 (cursor), validated atomically.
    /// OSC 110/111/112 restores the corresponding base.
    /// OSC 4/104 supports up to [`crate::MAX_OSC_PALETTE_ENTRIES`] indexed operations
    /// per command, validated atomically before execution.
    /// Components are stored at eight-bit precision.
    pub fn palette(&self) -> &Palette {
        self.state.osc.palette()
    }

    /// Updates the theme while retaining application color overrides.
    pub fn set_palette_base(
        &mut self,
        indexed: [Rgb; 256],
        foreground: Rgb,
        background: Rgb,
        cursor: Rgb,
    ) -> bool {
        self.state
            .osc
            .set_base(indexed, foreground, background, cursor)
    }

    /// Acknowledges coalesced visible palette changes.
    pub fn take_palette_change(&mut self) -> bool {
        self.state.osc.take_palette_change()
    }

    /// Unsent bytes from the active reply, in wire order. The fixed output slot
    /// holds one reply of at most 28 bytes. Bytes remain stable until acknowledged
    /// or explicitly reset. RIS preserves replies issued before it.
    #[inline]
    pub fn reply_bytes(&self) -> &[u8] {
        self.reply.bytes()
    }

    /// Acknowledges exactly the prefix accepted by the transport. Partial writes
    /// retain the remaining suffix. Returns false without changing state if
    /// `count` exceeds the unsent length; zero is accepted. After clearing a slot,
    /// retry processing, including an empty suffix, to publish a blocked reply.
    #[inline]
    pub fn consume_reply(&mut self, count: usize) -> bool {
        self.reply.consume(count)
    }

    /// Resolves keys issued for this processor's screen and retained snapshots.
    pub fn styles(&self) -> &StyleTable {
        &self.styles
    }

    /// Traces current rendition, both saved renditions, and both display buffers.
    /// Supply every externally retained ID as an additional root. Reclaimed
    /// history can make room for a pending rendition change before retrying it.
    pub fn collect_styles(
        &mut self,
        screen: &Screen,
        external: impl IntoIterator<Item = StyleId>,
    ) -> Result<usize, StyleError> {
        self.styles.collect(
            [
                self.state.style,
                self.state.saved_style[0],
                self.state.saved_style[1],
            ]
            .into_iter()
            .chain(screen.style_ids())
            .chain(external),
        )
    }

    pub fn parse_errors(&self) -> u64 {
        self.parse_errors
    }

    /// Number of completed ESC/CSI/OSC commands with unsupported or invalid contents.
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
    /// Reply backpressure retains a complete reply captured at query execution;
    /// drain and acknowledge active output before retrying the unconsumed input.
    #[inline(always)]
    pub fn advance<E, T: TextSink<Error = E>>(
        &mut self,
        input: &[u8],
        screen: &mut Screen,
        text: &mut T,
    ) -> Result<usize, ProcessError<E>> {
        self.drain(screen, text)
            .map_err(|error| ProcessError { consumed: 0, error })?;
        if input.len() == 1 && self.parser.is_ground() {
            let byte = input[0];
            let last = if self.state.charsets.is_graphics() {
                b'^'
            } else {
                b'~'
            };
            if byte.wrapping_sub(b' ') <= last - b' ' {
                // SAFETY: this slice contains exactly one printable ASCII byte.
                let value = unsafe { std::str::from_utf8_unchecked(input) };
                text.write(screen, value, self.state.style)
                    .map_err(|error| ProcessError {
                        consumed: 0,
                        error: ProcessFailure::Text(error),
                    })?;
                return Ok(1);
            }
        }
        let mut consumed = 0;
        while consumed < input.len() {
            if self.parser.is_ground() {
                let first = input[consumed];
                if (b' '..=b'~').contains(&first) {
                    let length = if self.state.charsets.is_graphics() {
                        if first >= b'_' {
                            let character = self.state.charsets.map(char::from(first));
                            consumed += 1;
                            if let Err(error) = text.write(
                                screen,
                                character.encode_utf8(&mut [0; 4]),
                                self.state.style,
                            ) {
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
                    // SAFETY: the first byte is checked above. The prefix scanner
                    // accepts only ASCII after it. The immutable run is valid UTF-8.
                    let run = unsafe {
                        std::str::from_utf8_unchecked(&input[consumed..consumed + length])
                    };
                    text.write(screen, run, self.state.style)
                        .map_err(|error| ProcessError {
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
                    if let Err(error) = text.write(screen, value, self.state.style) {
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
                if self.state.osc.needs_data() {
                    self.state.osc.data(&input[consumed..consumed + discarded]);
                }
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
                    if sequence.intermediates.is_empty()
                        && matches!(sequence.final_byte, b'D' | b'E')
                    {
                        pending[*end] = Some(Pending::Index(sequence.final_byte == b'E'));
                        *end += 1;
                    } else if !escape(screen, sequence, state) {
                        *ignored_sequences = ignored_sequences.saturating_add(1);
                    }
                }
                Event::Csi(sequence) => {
                    debug_assert_eq!(*end, 0);
                    let accepted = if sequence.final_byte == b'm'
                        && sequence.private_markers.is_empty()
                        && sequence.intermediates.is_empty()
                    {
                        if matches!(
                            sequence.parameters,
                            [] | [Parameter {
                                value: None | Some(0),
                                is_subparameter: false
                            }]
                        ) {
                            state.style = StyleId(0);
                            true
                        } else {
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
                        }
                    } else if sequence.final_byte == b'S' {
                        if sequence.private_markers.is_empty()
                            && sequence.intermediates.is_empty()
                            && sequence.parameters.len() <= 1
                            && sequence.parameters.iter().all(|p| !p.is_subparameter)
                        {
                            pending[*end] = Some(Pending::ScrollUp(count(sequence.parameters)));
                            *end += 1;
                            true
                        } else {
                            false
                        }
                    } else if sequence.final_byte == b'n' {
                        if let Some(reply) = reply::query(screen, sequence) {
                            pending[*end] = Some(Pending::Reply(reply));
                            *end += 1;
                            true
                        } else {
                            false
                        }
                    } else if sequence.final_byte == b'p' {
                        if let Some(reply) = reply::mode_query(screen, state.input, sequence) {
                            pending[*end] = Some(Pending::ModeReply(reply));
                            *end += 1;
                            true
                        } else {
                            csi(screen, sequence, state)
                        }
                    } else {
                        csi(screen, sequence, state)
                    };
                    if !accepted {
                        *ignored_sequences = ignored_sequences.saturating_add(1);
                    }
                }
                Event::Osc(event) => match state.osc.event(event) {
                    Outcome::Done => {}
                    Outcome::Ignored => *ignored_sequences = ignored_sequences.saturating_add(1),
                    Outcome::Reply(reply) => {
                        pending[*end] = Some(Pending::ColorReply(reply));
                        *end += 1;
                    }
                    Outcome::Replies => {
                        pending[*end] = Some(Pending::OscReplies);
                        *end += 1;
                    }
                },
                Event::Dcs(_) => {}
                Event::Error(_) => *parse_errors = parse_errors.saturating_add(1),
            });
            if T::STATEFUL && matches!(input[consumed], 0x1b | 0x7f) {
                // These bytes emit no Execute event. A replacement from a broken
                // UTF-8 prefix belongs before their boundary, including on retry.
                debug_assert!(self.end < self.pending.len());
                self.pending[self.end] = Some(Pending::Boundary);
                self.end += 1;
            }
            consumed += 1;
            self.drain(screen, text)
                .map_err(|error| ProcessError { consumed, error })?;
        }
        Ok(consumed)
    }

    /// Ends the lexical stream. Failed text, rendition, or replies remain for retry;
    /// repeated successful calls emit no further replacements or diagnostics.
    /// Active reply bytes remain available after a successful finish.
    pub fn finish<E>(
        &mut self,
        screen: &mut Screen,
        text: &mut impl TextSink<Error = E>,
    ) -> Result<(), ProcessFailure<E>> {
        self.drain(screen, text)?;
        let Self {
            parser,
            pending,
            end,
            parse_errors,
            state,
            ..
        } = self;
        parser.finish(|event| match event {
            Event::Text(_) => {
                pending[*end] = Some(Pending::Text(char::REPLACEMENT_CHARACTER));
                *end += 1;
            }
            Event::Error(_) => *parse_errors = parse_errors.saturating_add(1),
            Event::Osc(OscEvent::Cancel) => {
                state.osc.event(OscEvent::Cancel);
            }
            Event::Dcs(_) => {}
            _ => unreachable!("Finishing emits text, string cancellation, or a lexical error"),
        });
        self.drain(screen, text)?;
        text.boundary();
        Ok(())
    }

    /// Discards buffered input, replies, and events; clears diagnostics and resets
    /// the screen and establishes a sink boundary. The transport owns its output
    /// state. Style storage remains available for retained snapshots until collection.
    pub fn reset(&mut self, screen: &mut Screen, text: &mut impl TextSink) {
        text.boundary();
        self.parser.reset();
        self.pending = [None; 2];
        self.head = 0;
        self.end = 0;
        self.parse_errors = 0;
        self.ignored_sequences = 0;
        self.state.reset();
        self.reply = Reply::default();
        screen.reset(StyleId(0));
    }

    #[inline]
    fn drain<E>(
        &mut self,
        screen: &mut Screen,
        text: &mut impl TextSink<Error = E>,
    ) -> Result<(), ProcessFailure<E>> {
        if self.head == self.end {
            return Ok(());
        }
        self.drain_pending(screen, text)
    }

    #[inline(never)]
    fn drain_pending<E>(
        &mut self,
        screen: &mut Screen,
        text: &mut impl TextSink<Error = E>,
    ) -> Result<(), ProcessFailure<E>> {
        while self.head < self.end {
            match self.pending[self.head].expect("A pending action is present") {
                Pending::Text(character) => text
                    .write(screen, character.encode_utf8(&mut [0; 4]), self.state.style)
                    .map_err(ProcessFailure::Text)?,
                Pending::Boundary => text.boundary(),
                Pending::Execute(control) => {
                    text.boundary();
                    match control {
                        0x0e => self.state.charsets.shift(true),
                        0x0f => self.state.charsets.shift(false),
                        _ => execute(screen, control, self.state.style, self.state.input.new_line)
                            .map_err(ProcessFailure::Screen)?,
                    }
                }
                Pending::Index(carriage) => {
                    text.boundary();
                    screen
                        .line_feed(self.state.style)
                        .map_err(ProcessFailure::Screen)?;
                    if carriage {
                        screen.carriage_return();
                    }
                }
                Pending::ScrollUp(count) => {
                    text.boundary();
                    screen
                        .scroll_up(count, self.state.style)
                        .map_err(ProcessFailure::Screen)?;
                }
                Pending::Rendition(style) => {
                    self.state.style = self.styles.intern(style).map_err(ProcessFailure::Style)?;
                }
                Pending::Reply(reply) => {
                    if !self.reply.is_empty() {
                        return Err(ProcessFailure::ReplyPending);
                    }
                    reply.encode(&mut self.reply);
                }
                Pending::ModeReply(reply) => {
                    if !self.reply.is_empty() {
                        return Err(ProcessFailure::ReplyPending);
                    }
                    reply.encode(&mut self.reply);
                }
                Pending::ColorReply(reply) => {
                    if !self.reply.is_empty() {
                        return Err(ProcessFailure::ReplyPending);
                    }
                    reply.encode(&mut self.reply);
                }
                Pending::OscReplies => {
                    if !self.reply.is_empty() {
                        return Err(ProcessFailure::ReplyPending);
                    }
                    self.state
                        .osc
                        .next_reply()
                        .expect("A captured OSC reply is pending")
                        .encode(&mut self.reply);
                    if self.state.osc.has_replies() {
                        return Err(ProcessFailure::ReplyPending);
                    }
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
fn ascii_range_mask<const LAST: u8>(word: u64) -> u64 {
    const ONES: u64 = u64::MAX / 255;
    const HIGH: u64 = ONES * 128;
    let below = word.wrapping_sub(ONES * 32) & !word;
    let above = word.wrapping_add(ONES * u64::from(127 - LAST)) | word;
    (below | above) & HIGH
}

#[inline]
fn ascii_prefix<const LAST: u8>(input: &[u8]) -> usize {
    let mut offset = 0;
    // Block-sized inputs enter scanning before short-input dispatch.
    if input.len() >= 32 {
        while input.len() - offset >= 32 {
            // Check four words together, then locate the first failing word.
            let invalid = [0, 8, 16, 24].map(|index| {
                let word = u64::from_ne_bytes(
                    input[offset + index..offset + index + 8]
                        .try_into()
                        .unwrap(),
                );
                ascii_range_mask::<LAST>(word)
            });
            if invalid.into_iter().fold(0, |all, word| all | word) != 0 {
                offset += invalid.iter().take_while(|&&mask| mask == 0).count() * 8;
                break;
            }
            offset += 32;
        }
    } else if let [byte] = input {
        // Classify a single byte before preparing word scans.
        return usize::from((b' '..=LAST).contains(byte));
    }
    while input.len() - offset >= 8 {
        let word = u64::from_ne_bytes(input[offset..offset + 8].try_into().unwrap());
        // Out-of-range bytes set a high bit; the scalar tail locates the boundary.
        if ascii_range_mask::<LAST>(word) != 0 {
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

fn execute(
    screen: &mut Screen,
    control: u8,
    style: StyleId,
    new_line_mode: bool,
) -> Result<(), StorageError> {
    match control {
        0x08 => screen.backspace(),
        0x09 => screen.tab_forward(1),
        0x0a..=0x0c => {
            screen.line_feed(style)?;
            if new_line_mode {
                screen.carriage_return();
            }
        }
        0x0d => screen.carriage_return(),
        _ => {}
    }
    Ok(())
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
            save_cursor(screen, state);
        }
        b'8' => {
            restore_cursor(screen, state);
        }
        b'H' => screen.set_tab_stop(),
        b'=' => state.input.application_keypad = true,
        b'>' => state.input.application_keypad = false,
        b'M' => screen.reverse_index(state.style),
        b'c' => {
            state.reset();
            screen.reset(state.style);
        }
        _ => return false,
    }
    true
}

fn save_cursor(screen: &mut Screen, state: &mut State) {
    screen.save_cursor();
    let index = screen.active_buffer() as usize;
    state.saved_style[index] = state.style;
    state.saved_charsets[index] = state.charsets;
}

fn restore_cursor(screen: &mut Screen, state: &mut State) {
    screen.restore_cursor();
    let index = screen.active_buffer() as usize;
    state.style = state.saved_style[index];
    state.charsets = state.saved_charsets[index];
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

fn csi(screen: &mut Screen, sequence: ControlSequence<'_>, state: &mut State) -> bool {
    let style = state.style;
    let p = sequence.parameters;
    if p.iter().any(|p| p.is_subparameter) {
        return false;
    }
    if !sequence.intermediates.is_empty() {
        if sequence.intermediates == b"!"
            && sequence.final_byte == b'p'
            && sequence.private_markers.is_empty()
            && p.is_empty()
        {
            state.soft_reset(screen);
            return true;
        }
        if sequence.intermediates != b" "
            || sequence.final_byte != b'q'
            || !sequence.private_markers.is_empty()
            || p.len() > 1
        {
            return false;
        }
        let (shape, blinking) = match value(p, 0, 0) {
            0 | 1 => (CursorShape::Block, true),
            2 => (CursorShape::Block, false),
            3 => (CursorShape::Underline, true),
            4 => (CursorShape::Underline, false),
            5 => (CursorShape::Bar, true),
            6 => (CursorShape::Bar, false),
            _ => return false,
        };
        let mut appearance = screen.cursor_appearance();
        appearance.shape = shape;
        appearance.blinking = blinking;
        screen.set_cursor_appearance(appearance);
        return true;
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
                    (false, Some(4 | 20))
                        | (
                            true,
                            Some(
                                1 | 6
                                    | 7
                                    | 12
                                    | 25
                                    | 47
                                    | 66
                                    | 67
                                    | 1000
                                    | 1002
                                    | 1003
                                    | 1004
                                    | 1006
                                    | 1047
                                    | 1048
                                    | 1049
                                    | 2004
                            )
                        )
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
                (true, Some(66)) => state.input.application_keypad = enabled,
                (true, Some(67)) => state.input.backarrow = enabled,
                (true, Some(2004)) => state.input.bracketed_paste = enabled,
                (true, Some(1004)) => state.input.focus_reporting = enabled,
                (true, Some(6)) => screen.set_origin_mode(enabled),
                (true, Some(7)) => screen.set_auto_wrap(enabled),
                (true, Some(47 | 1047 | 1049)) => {
                    if parameter.value == Some(1049) && enabled {
                        save_cursor(screen, state);
                    }
                    if parameter.value == Some(1047)
                        && !enabled
                        && screen.active_buffer() == ScreenBuffer::Alternate
                    {
                        screen.erase_display(EraseMode::All, state.style);
                    }
                    screen.select_buffer(if enabled {
                        ScreenBuffer::Alternate
                    } else {
                        ScreenBuffer::Primary
                    });
                    if parameter.value == Some(1049) {
                        if enabled {
                            screen.erase_display(EraseMode::All, state.style);
                        } else {
                            restore_cursor(screen, state);
                        }
                    }
                }
                (true, Some(1048)) => {
                    if enabled {
                        save_cursor(screen, state);
                    } else {
                        restore_cursor(screen, state);
                    }
                }
                (true, Some(12 | 25)) => {
                    let mut appearance = screen.cursor_appearance();
                    if parameter.value == Some(12) {
                        appearance.blinking = enabled;
                    } else {
                        appearance.visible = enabled;
                    }
                    screen.set_cursor_appearance(appearance);
                }
                _ => mouse_mode(&mut state.input, parameter.value.unwrap(), enabled),
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
        b'B' | b'e' => screen.move_cursor_down(n),
        b'C' | b'a' => screen.move_cursor_right(n),
        b'D' => screen.move_cursor_left(n),
        b'E' => {
            screen.move_cursor_down(n);
            screen.carriage_return();
        }
        b'F' => {
            screen.move_cursor_up(n);
            screen.carriage_return();
        }
        b'G' | b'`' => {
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

#[cold]
fn mouse_mode(input: &mut InputModes, mode: u16, enabled: bool) {
    use crate::MouseTracking;
    match mode {
        1000 | 1002 | 1003 => {
            input.mouse_tracking = if !enabled {
                MouseTracking::Disabled
            } else {
                match mode {
                    1000 => MouseTracking::Buttons,
                    1002 => MouseTracking::ButtonMotion,
                    1003 => MouseTracking::AnyMotion,
                    _ => unreachable!("Mouse mode was validated"),
                }
            };
        }
        1006 => input.sgr_mouse = enabled,
        _ => unreachable!("Mode parameters were validated"),
    }
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
                let mut bytes = [b'A'; 80];
                for length in [0, 1, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65] {
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
            for fill in [b' ', last] {
                let mut bytes = [fill; 65];
                for position in [6, 7, 14, 15, 22, 23, 30, 31, 62, 63] {
                    bytes.fill(fill);
                    for pair in 0..=u16::MAX {
                        bytes[position..position + 2].copy_from_slice(&pair.to_le_bytes());
                        let expected = bytes
                            .iter()
                            .position(|byte| !(b' '..=last).contains(byte))
                            .unwrap_or(bytes.len());
                        assert_eq!(scan(&bytes), expected);
                    }
                }
            }
        }
    }
}
