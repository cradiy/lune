use crate::utf8::{Decoded, Decoder};

/// Maximum number of numeric fields, including colon subparameters.
pub const MAX_PARAMETERS: usize = 32;
/// Maximum number of intermediate bytes in an ESC, CSI, or DCS header.
pub const MAX_INTERMEDIATES: usize = 4;
/// Maximum number of leading private marker bytes in a CSI or DCS header.
pub const MAX_PRIVATE_MARKERS: usize = 4;
/// Maximum number of bytes after ESC in an ESC, CSI, or DCS header.
/// Embedded C0 controls and DEL count toward this wire-byte limit.
pub const MAX_SEQUENCE_BYTES: usize = 256;

/// A numeric field whose omission is distinct from an explicit zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Parameter {
    /// Numeric value, bounded by `u16::MAX`.
    pub value: Option<u16>,
    /// Whether this field follows a colon instead of a semicolon.
    pub is_subparameter: bool,
}

/// An ESC sequence borrowed for the duration of its callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EscapeSequence<'a> {
    pub intermediates: &'a [u8],
    pub final_byte: u8,
}

/// A CSI or DCS header borrowed for the duration of its callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlSequence<'a> {
    pub private_markers: &'a [u8],
    pub parameters: &'a [Parameter],
    pub intermediates: &'a [u8],
    pub final_byte: u8,
}

/// A framing or capacity error encountered while consuming a stream.
/// Rejected ESC/CSI headers are discarded through the final byte. Rejected DCS
/// strings are discarded through ST; ESC, CAN, and SUB can restart or cancel them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    InvalidSequence,
    SequenceTooLong,
    TooManyParameters,
    ParameterOverflow,
    TooManyIntermediates,
    TooManyPrivateMarkers,
    IncompleteSequence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OscTerminator {
    Bell,
    St,
}

/// Streaming OSC framing. Consumers bound any payload they retain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OscEvent<'a> {
    Start,
    /// Raw payload bytes, including the command selector. C0 and DEL are ignored.
    /// Spans are valid during the callback and may vary with input chunking.
    Data(&'a [u8]),
    /// Terminated by BEL or ST.
    End(OscTerminator),
    /// Interrupted by another escape, CAN, SUB, or end of stream.
    Cancel,
}

/// Streaming DCS framing. Consumers bound retained payload data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DcsEvent<'a> {
    Start(&'a ControlSequence<'a>),
    /// Raw payload, including C0 except ESC, CAN, and SUB. DEL is ignored.
    /// Spans are valid during the callback and may vary with input chunking.
    Data(&'a [u8]),
    /// Terminated by seven-bit ST.
    End,
    /// Interrupted by another escape, CAN, SUB, or end of stream.
    Cancel,
}

/// Streaming APC framing with raw payload spans and seven-bit ST.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApcEvent<'a> {
    Start,
    Data(&'a [u8]),
    End,
    Cancel,
}

/// A lexical event. Sequence bytes retain their syntax for interpretation by a consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// A valid UTF-8 text span borrowed for the duration of the callback.
    Text(&'a str),
    Execute(u8),
    Escape(EscapeSequence<'a>),
    Csi(ControlSequence<'a>),
    Osc(OscEvent<'a>),
    Dcs(DcsEvent<'a>),
    Apc(ApcEvent<'a>),
    Error(ParseError),
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Utf8Pending,
    Escape,
    CsiEntry,
    CsiParameters,
    CsiIntermediate,
    Ignore {
        csi: bool,
    },
    String {
        kind: StringKind,
    },
    StringEscape {
        kind: StringKind,
    },
    DcsEntry,
    DcsParameters,
    DcsIntermediate,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StringKind {
    Discard,
    Osc,
    Dcs,
    Apc,
    Rejected,
}

impl StringKind {
    #[inline]
    fn emit_data(self, bytes: &[u8], emit: &mut impl FnMut(Event<'_>)) {
        match self {
            Self::Osc => emit(Event::Osc(OscEvent::Data(bytes))),
            Self::Dcs => emit(Event::Dcs(DcsEvent::Data(bytes))),
            Self::Apc => emit(Event::Apc(ApcEvent::Data(bytes))),
            _ => {}
        }
    }
}

/// A fixed-capacity parser whose state persists across calls to [`Self::advance`].
#[derive(Default)]
pub struct Parser {
    state: State,
    utf8: Decoder,
    parameters: [Parameter; MAX_PARAMETERS],
    parameter_count: usize,
    intermediates: [u8; MAX_INTERMEDIATES],
    intermediate_count: usize,
    private_markers: [u8; MAX_PRIVATE_MARKERS],
    private_count: usize,
    sequence_bytes: usize,
}

impl Parser {
    /// Whether the parser is in ground state with no incomplete UTF-8 scalar.
    /// Complete valid text can then be consumed without changing parser state.
    #[inline]
    pub fn is_ground(&self) -> bool {
        self.state == State::Ground
    }

    /// Discards a string payload prefix without emitting data events, stopping
    /// before a control or DEL. Returns zero outside a string payload.
    #[inline]
    pub fn discard_string_payload(&mut self, bytes: &[u8]) -> usize {
        if !matches!(self.state, State::String { .. }) {
            return 0;
        }
        let Some(&first) = bytes.first() else {
            return 0;
        };
        if first < 0x20 || first == 0x7f {
            return 0;
        }
        if bytes.len() == 1 {
            return 1;
        }
        text_end(bytes)
    }

    /// Consumes a chunk and invokes `emit` synchronously for each lexical event.
    ///
    /// Invalid UTF-8 in text uses the replacement character. Raw eight-bit C1
    /// bytes follow the UTF-8 error policy; decoded Unicode is emitted as text.
    /// DEL is ignored. CAN and SUB cancel a pending sequence and emit a control.
    /// Text span boundaries may vary with input chunking.
    #[inline]
    pub fn advance(&mut self, bytes: &[u8], mut emit: impl FnMut(Event<'_>)) {
        if let [byte] = bytes {
            if *byte >= 0x20 && *byte != 0x7f {
                match self.state {
                    State::Ground | State::Utf8Pending => {
                        if self.decode(*byte, &mut emit) {
                            // An invalid pending prefix leaves this byte to be decoded again.
                            self.decode(*byte, &mut emit);
                        }
                        return;
                    }
                    State::String { kind } => {
                        kind.emit_data(bytes, &mut emit);
                        return;
                    }
                    _ => {}
                }
            }
            self.advance_byte(*byte, &mut emit);
            return;
        }

        self.advance_chunk(bytes, emit);
    }

    fn advance_chunk(&mut self, bytes: &[u8], mut emit: impl FnMut(Event<'_>)) {
        let mut offset = 0;
        while offset < bytes.len() {
            if matches!(
                self.state,
                State::CsiEntry | State::CsiParameters | State::CsiIntermediate
            ) {
                offset += self.advance_csi(&bytes[offset..], &mut emit);
                continue;
            }
            let first = bytes[offset];
            if first >= 0x20 && first != 0x7f {
                if self.state == State::Ground {
                    let length = text_end(&bytes[offset..]);
                    self.advance_text(&bytes[offset..offset + length], &mut emit);
                    offset += length;
                    continue;
                }
                if let State::String { kind } = self.state {
                    let length = text_end(&bytes[offset..]);
                    kind.emit_data(&bytes[offset..offset + length], &mut emit);
                    offset += length;
                    continue;
                }
                if let State::Ignore { csi } = self.state {
                    let first_final = if csi { 0x40 } else { 0x30 };
                    let length = bytes[offset..]
                        .iter()
                        .position(|&byte| byte < 0x20 || byte >= first_final)
                        .unwrap_or(bytes.len() - offset);
                    if length != 0 {
                        offset += length;
                        continue;
                    }
                }
            }
            self.advance_byte(first, &mut emit);
            offset += 1;
        }
    }

    /// Ends a stream, replacing incomplete UTF-8 and reporting an unfinished sequence.
    ///
    /// A sequence already rejected with an error is silently discarded. The
    /// parser returns to its initial state and can accept another stream.
    pub fn finish(&mut self, mut emit: impl FnMut(Event<'_>)) {
        if self.utf8.finish() {
            emit(Event::Text("\u{fffd}"));
        }
        if matches!(
            self.state,
            State::String {
                kind: StringKind::Osc
            } | State::StringEscape {
                kind: StringKind::Osc
            }
        ) {
            emit(Event::Osc(OscEvent::Cancel));
        }
        if matches!(
            self.state,
            State::String {
                kind: StringKind::Dcs
            } | State::StringEscape {
                kind: StringKind::Dcs
            }
        ) {
            emit(Event::Dcs(DcsEvent::Cancel));
        }
        if matches!(
            self.state,
            State::String {
                kind: StringKind::Apc
            } | State::StringEscape {
                kind: StringKind::Apc
            }
        ) {
            emit(Event::Apc(ApcEvent::Cancel));
        }
        if !matches!(
            self.state,
            State::Ground
                | State::Utf8Pending
                | State::Ignore { .. }
                | State::String {
                    kind: StringKind::Rejected
                }
                | State::StringEscape {
                    kind: StringKind::Rejected
                }
        ) {
            emit(Event::Error(ParseError::IncompleteSequence));
        }
        self.reset();
    }

    /// Discards pending input and returns to the initial state.
    pub fn reset(&mut self) {
        self.state = State::Ground;
        self.utf8.finish();
        self.clear_header();
    }

    fn clear_header(&mut self) {
        self.parameter_count = 0;
        self.intermediate_count = 0;
        self.private_count = 0;
        self.sequence_bytes = 0;
    }

    fn start_escape(&mut self) {
        self.clear_header();
        self.state = State::Escape;
    }

    fn advance_text(&mut self, mut bytes: &[u8], emit: &mut impl FnMut(Event<'_>)) {
        if bytes.is_ascii() {
            // SAFETY: ASCII bytes always form valid UTF-8.
            emit(Event::Text(unsafe { std::str::from_utf8_unchecked(bytes) }));
            return;
        }
        while !bytes.is_empty() {
            match std::str::from_utf8(bytes) {
                Ok(text) => {
                    emit(Event::Text(text));
                    return;
                }
                Err(error) => {
                    let (valid, remaining) = bytes.split_at(error.valid_up_to());
                    if !valid.is_empty() {
                        emit(Event::Text(
                            std::str::from_utf8(valid).expect("The prefix is valid UTF-8"),
                        ));
                    }
                    match error.error_len() {
                        Some(length) => {
                            emit(Event::Text("\u{fffd}"));
                            bytes = &remaining[length..];
                        }
                        None => {
                            for &byte in remaining {
                                self.decode(byte, emit);
                            }
                            return;
                        }
                    }
                }
            }
        }
    }

    fn advance_byte(&mut self, byte: u8, emit: &mut impl FnMut(Event<'_>)) {
        if matches!(
            self.state,
            State::CsiEntry | State::CsiParameters | State::CsiIntermediate
        ) && (0x20..=0x7e).contains(&byte)
        {
            self.sequence_bytes += 1;
            if self.sequence_bytes > MAX_SEQUENCE_BYTES {
                self.reject(ParseError::SequenceTooLong, emit);
                self.dispatch(byte, emit);
            } else {
                self.header::<false>(byte, emit);
            }
            return;
        }

        if self.state == State::Utf8Pending && !self.decode(byte, emit) {
            return;
        }
        if matches!(
            self.state,
            State::String { .. } | State::StringEscape { .. }
        ) {
            self.advance_string(byte, emit);
            return;
        }
        if byte == 0x1b {
            self.start_escape();
            return;
        }
        if self.state == State::Ground {
            match byte {
                0x00..=0x1f => emit(Event::Execute(byte)),
                0x7f => {}
                _ => {
                    self.decode(byte, emit);
                }
            }
            return;
        }
        if matches!(byte, 0x18 | 0x1a) {
            self.state = State::Ground;
            emit(Event::Execute(byte));
            return;
        }

        if !matches!(
            self.state,
            State::Ground | State::Utf8Pending | State::Ignore { .. }
        ) {
            self.sequence_bytes += 1;
            if self.sequence_bytes > MAX_SEQUENCE_BYTES {
                self.reject(ParseError::SequenceTooLong, emit);
                if matches!(self.state, State::String { .. }) {
                    return;
                }
            }
        }

        match byte {
            b'[' if self.state == State::Escape && self.intermediate_count == 0 => {
                self.state = State::CsiEntry;
            }
            0x00..=0x1f => {
                if !matches!(
                    self.state,
                    State::DcsEntry | State::DcsParameters | State::DcsIntermediate
                ) {
                    emit(Event::Execute(byte));
                }
            }
            0x7f => {}
            _ => self.dispatch(byte, emit),
        }
    }

    #[inline(never)]
    fn advance_string(&mut self, byte: u8, emit: &mut impl FnMut(Event<'_>)) {
        if let State::StringEscape { kind } = self.state {
            if byte == b'\\' {
                self.state = State::Ground;
                match kind {
                    StringKind::Osc => emit(Event::Osc(OscEvent::End(OscTerminator::St))),
                    StringKind::Dcs => emit(Event::Dcs(DcsEvent::End)),
                    StringKind::Apc => emit(Event::Apc(ApcEvent::End)),
                    _ => {}
                }
            } else {
                match kind {
                    StringKind::Osc => emit(Event::Osc(OscEvent::Cancel)),
                    StringKind::Dcs => emit(Event::Dcs(DcsEvent::Cancel)),
                    StringKind::Apc => emit(Event::Apc(ApcEvent::Cancel)),
                    _ => {}
                }
                self.start_escape();
                self.advance_byte(byte, emit);
            }
            return;
        }
        let State::String { kind } = self.state else {
            unreachable!();
        };
        match byte {
            0x1b => self.state = State::StringEscape { kind },
            0x18 | 0x1a => {
                match kind {
                    StringKind::Osc => emit(Event::Osc(OscEvent::Cancel)),
                    StringKind::Dcs => emit(Event::Dcs(DcsEvent::Cancel)),
                    StringKind::Apc => emit(Event::Apc(ApcEvent::Cancel)),
                    _ => {}
                }
                self.state = State::Ground;
                emit(Event::Execute(byte));
            }
            0x07 if kind == StringKind::Osc => {
                self.state = State::Ground;
                emit(Event::Osc(OscEvent::End(OscTerminator::Bell)));
            }
            0x00..=0x1f if kind == StringKind::Dcs => emit(Event::Dcs(DcsEvent::Data(&[byte]))),
            0x00..=0x1f if kind == StringKind::Apc => emit(Event::Apc(ApcEvent::Data(&[byte]))),
            0x20..=0x7e | 0x80..=0xff => kind.emit_data(&[byte], emit),
            _ => {}
        }
    }

    fn advance_csi(&mut self, bytes: &[u8], emit: &mut impl FnMut(Event<'_>)) -> usize {
        for (offset, &byte) in bytes.iter().enumerate() {
            if !(0x20..=0x7e).contains(&byte) {
                self.advance_byte(byte, emit);
                return offset + 1;
            }
            self.sequence_bytes += 1;
            if self.sequence_bytes > MAX_SEQUENCE_BYTES {
                self.reject(ParseError::SequenceTooLong, emit);
                self.dispatch(byte, emit);
                return offset + 1;
            }
            self.header::<false>(byte, emit);
            if !matches!(
                self.state,
                State::CsiEntry | State::CsiParameters | State::CsiIntermediate
            ) {
                return offset + 1;
            }
        }
        bytes.len()
    }

    /// Returns true when an invalid prefix leaves the current byte to be reprocessed.
    fn decode(&mut self, byte: u8, emit: &mut impl FnMut(Event<'_>)) -> bool {
        match self.utf8.push(byte) {
            Decoded::Text(text) => {
                self.state = State::Ground;
                emit(Event::Text(text));
            }
            Decoded::Pending => {
                if self.state != State::Utf8Pending {
                    self.state = State::Utf8Pending;
                }
            }
            Decoded::Invalid { retry } => {
                self.state = State::Ground;
                emit(Event::Text("\u{fffd}"));
                return retry;
            }
        }
        false
    }

    fn dispatch(&mut self, byte: u8, emit: &mut impl FnMut(Event<'_>)) {
        match self.state {
            State::Ground => {
                self.decode(byte, emit);
            }
            State::Escape => self.escape(byte, emit),
            State::CsiEntry | State::CsiParameters | State::CsiIntermediate => {
                self.header::<false>(byte, emit)
            }
            State::DcsEntry | State::DcsParameters | State::DcsIntermediate => self.dcs(byte, emit),
            State::Ignore { csi } => {
                let first_final = if csi { 0x40 } else { 0x30 };
                if (first_final..=0x7e).contains(&byte) {
                    self.state = State::Ground;
                }
            }
            State::Utf8Pending | State::String { .. } | State::StringEscape { .. } => {
                unreachable!()
            }
        }
    }

    fn escape(&mut self, byte: u8, emit: &mut impl FnMut(Event<'_>)) {
        if self.intermediate_count == 0 {
            match byte {
                b'[' => {
                    self.state = State::CsiEntry;
                    return;
                }
                b'P' => {
                    self.state = State::DcsEntry;
                    return;
                }
                b']' | b'X' | b'^' | b'_' => {
                    let kind = match byte {
                        b']' => {
                            emit(Event::Osc(OscEvent::Start));
                            StringKind::Osc
                        }
                        b'_' => {
                            emit(Event::Apc(ApcEvent::Start));
                            StringKind::Apc
                        }
                        _ => StringKind::Discard,
                    };
                    self.state = State::String { kind };
                    return;
                }
                _ => {}
            }
        }

        match byte {
            0x20..=0x2f => self.collect_intermediate(byte, emit),
            0x30..=0x7e => {
                emit(Event::Escape(EscapeSequence {
                    intermediates: &self.intermediates[..self.intermediate_count],
                    final_byte: byte,
                }));
                self.state = State::Ground;
            }
            _ => self.reject(ParseError::InvalidSequence, emit),
        }
    }

    #[inline(always)]
    fn header<const DCS: bool>(&mut self, byte: u8, emit: &mut impl FnMut(Event<'_>)) {
        let (entry, parameters, intermediate) = if DCS {
            (
                State::DcsEntry,
                State::DcsParameters,
                State::DcsIntermediate,
            )
        } else {
            (
                State::CsiEntry,
                State::CsiParameters,
                State::CsiIntermediate,
            )
        };
        match byte {
            0x40..=0x7e => {
                let sequence = ControlSequence {
                    private_markers: &self.private_markers[..self.private_count],
                    parameters: &self.parameters[..self.parameter_count],
                    intermediates: &self.intermediates[..self.intermediate_count],
                    final_byte: byte,
                };
                if DCS {
                    emit(Event::Dcs(DcsEvent::Start(&sequence)));
                    self.state = State::String {
                        kind: StringKind::Dcs,
                    };
                } else {
                    emit(Event::Csi(sequence));
                    self.state = State::Ground;
                }
            }
            0x20..=0x2f => {
                self.state = intermediate;
                self.collect_intermediate(byte, emit);
            }
            0x3c..=0x3f if self.state == entry => {
                if self.private_count == MAX_PRIVATE_MARKERS {
                    self.reject(ParseError::TooManyPrivateMarkers, emit);
                } else {
                    self.private_markers[self.private_count] = byte;
                    self.private_count += 1;
                }
            }
            b'0'..=b'9' | b';' | b':' if self.state != intermediate => {
                self.state = parameters;
                self.parameter(byte, emit);
            }
            _ => self.reject(ParseError::InvalidSequence, emit),
        }
    }

    #[inline(never)]
    fn dcs(&mut self, byte: u8, emit: &mut impl FnMut(Event<'_>)) {
        self.header::<true>(byte, emit);
    }

    fn parameter(&mut self, byte: u8, emit: &mut impl FnMut(Event<'_>)) {
        if self.parameter_count == 0 {
            self.push_parameter(false, emit);
        }
        if byte == b';' || byte == b':' {
            self.push_parameter(byte == b':', emit);
            return;
        }

        let parameter = &mut self.parameters[self.parameter_count - 1];
        match parameter
            .value
            .unwrap_or(0)
            .checked_mul(10)
            .and_then(|value| value.checked_add(u16::from(byte - b'0')))
        {
            Some(value) => parameter.value = Some(value),
            None => self.reject(ParseError::ParameterOverflow, emit),
        }
    }

    fn push_parameter(&mut self, is_subparameter: bool, emit: &mut impl FnMut(Event<'_>)) {
        if self.parameter_count == MAX_PARAMETERS {
            self.reject(ParseError::TooManyParameters, emit);
        } else {
            self.parameters[self.parameter_count] = Parameter {
                value: None,
                is_subparameter,
            };
            self.parameter_count += 1;
        }
    }

    fn collect_intermediate(&mut self, byte: u8, emit: &mut impl FnMut(Event<'_>)) {
        if self.intermediate_count == MAX_INTERMEDIATES {
            self.reject(ParseError::TooManyIntermediates, emit);
        } else {
            self.intermediates[self.intermediate_count] = byte;
            self.intermediate_count += 1;
        }
    }

    #[cold]
    fn reject(&mut self, error: ParseError, emit: &mut impl FnMut(Event<'_>)) {
        let csi = matches!(
            self.state,
            State::CsiEntry | State::CsiParameters | State::CsiIntermediate
        );
        self.state = if matches!(
            self.state,
            State::DcsEntry | State::DcsParameters | State::DcsIntermediate
        ) {
            State::String {
                kind: StringKind::Rejected,
            }
        } else {
            State::Ignore { csi }
        };
        emit(Event::Error(error));
    }
}

fn text_end(bytes: &[u8]) -> usize {
    const WORD_BYTES: usize = size_of::<usize>();
    const LOW_BITS: usize = usize::MAX / 255;
    const HIGH_BITS: usize = LOW_BITS * 128;

    let mut offset = 0;
    while bytes.len() - offset >= WORD_BYTES {
        let word = usize::from_ne_bytes(
            bytes[offset..offset + WORD_BYTES]
                .try_into()
                .expect("A complete word is present"),
        );
        // These masks detect C0 and DEL while allowing UTF-8 bytes through.
        let c0 = word.wrapping_sub(LOW_BITS * 32) & !word & HIGH_BITS;
        let del = word ^ (LOW_BITS * 127);
        let del = del.wrapping_sub(LOW_BITS) & !del & HIGH_BITS;
        if c0 != 0 || del != 0 {
            break;
        }
        offset += WORD_BYTES;
    }

    offset
        + bytes[offset..]
            .iter()
            .position(|&byte| byte < 0x20 || byte == 0x7f)
            .unwrap_or(bytes.len() - offset)
}

#[cfg(test)]
mod scan_tests {
    use super::{Parser, text_end};

    #[test]
    fn word_scan_matches_scalar_control_detection_at_all_alignments() {
        let mut bytes = [b'A'; 64];
        let mut parser = Parser::default();
        parser.advance(b"\x1b]", |_| {});
        for value in 0..=u8::MAX {
            assert_eq!(parser.discard_string_payload(&[value]), text_end(&[value]));
            for position in 0..bytes.len() {
                bytes[position] = value;
                for alignment in 0..size_of::<usize>() {
                    let input = &bytes[alignment..];
                    let expected = input
                        .iter()
                        .position(|&byte| byte < 0x20 || byte == 0x7f)
                        .unwrap_or(input.len());
                    assert_eq!(text_end(input), expected);
                }
                bytes[position] = b'A';
            }
        }
    }
}
