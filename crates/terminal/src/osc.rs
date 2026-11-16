use lune_vt::{OscEvent, OscTerminator};

use crate::reply::{ColorReport, ColorTarget};
use crate::{Palette, PaletteEntry, Rgb};

/// Maximum UTF-8 byte length of an OSC 0/2 window title.
pub const MAX_TITLE_BYTES: usize = 4096;
const MAX_COLOR_BYTES: usize = 18;
/// Maximum number of indexed operations in one OSC 4/104 command.
pub const MAX_OSC_PALETTE_ENTRIES: usize = 256;

#[derive(Clone, Copy, Default)]
enum Phase {
    #[default]
    Idle,
    Selector {
        value: u16,
        digits: u8,
    },
    Title,
    Color(u8),
    Index,
    IndexedColor(u8),
    ResetIndex,
    Discard,
}

pub(crate) enum Outcome {
    Done,
    Ignored,
    Reply(ColorReport),
    PaletteReplies,
}

#[derive(Clone, Copy)]
enum Operation {
    Set(u8, Rgb),
    Query(u8),
    Reset(u8),
    Reply(u8, Rgb),
}

pub(crate) struct Osc {
    buffers: [[u8; MAX_TITLE_BYTES]; 2],
    lengths: [usize; 2],
    active: usize,
    phase: Phase,
    changed: bool,
    palette: Palette,
    palette_changed: bool,
    operations: [Operation; MAX_OSC_PALETTE_ENTRIES],
    operation_count: usize,
    reply_head: usize,
    reply_end: usize,
    terminator: OscTerminator,
    index: u16,
    index_digits: u8,
}

impl Osc {
    pub(crate) fn new(palette: Palette) -> Self {
        Self {
            buffers: [[0; MAX_TITLE_BYTES]; 2],
            lengths: [0; 2],
            active: 0,
            phase: Phase::Idle,
            changed: false,
            palette,
            palette_changed: false,
            operations: [Operation::Query(0); MAX_OSC_PALETTE_ENTRIES],
            operation_count: 0,
            reply_head: 0,
            reply_end: 0,
            terminator: OscTerminator::St,
            index: 0,
            index_digits: 0,
        }
    }

    pub(crate) fn palette(&self) -> &Palette {
        &self.palette
    }

    pub(crate) fn take_palette_change(&mut self) -> bool {
        std::mem::take(&mut self.palette_changed)
    }

    pub(crate) fn set_base(
        &mut self,
        indexed: [Rgb; 256],
        foreground: Rgb,
        background: Rgb,
        cursor: Rgb,
    ) -> bool {
        let changed = self
            .palette
            .set_base(indexed, foreground, background, cursor);
        self.palette_changed |= changed;
        changed
    }

    pub(crate) fn title(&self) -> &str {
        std::str::from_utf8(&self.buffers[self.active][..self.lengths[self.active]])
            .expect("Committed window titles are valid UTF-8")
    }

    pub(crate) fn take_title_change(&mut self) -> Option<&str> {
        std::mem::take(&mut self.changed).then(|| self.title())
    }

    pub(crate) fn reset(&mut self) {
        self.changed |= self.lengths[self.active] != 0;
        self.lengths = [0; 2];
        self.phase = Phase::Idle;
        self.palette_changed |= self.palette.reset_all();
        self.operation_count = 0;
        self.reply_head = 0;
        self.reply_end = 0;
    }

    #[inline]
    pub(crate) fn needs_data(&self) -> bool {
        matches!(
            self.phase,
            Phase::Selector { .. }
                | Phase::Title
                | Phase::Color(_)
                | Phase::Index
                | Phase::IndexedColor(_)
                | Phase::ResetIndex
        )
    }

    #[inline]
    pub(crate) fn data(&mut self, bytes: &[u8]) {
        if matches!(self.phase, Phase::Title) {
            self.append_data(bytes, MAX_TITLE_BYTES);
        } else {
            self.command_data(bytes);
        }
    }

    #[inline(never)]
    fn command_data(&mut self, mut bytes: &[u8]) {
        while let Phase::Selector { value, digits } = self.phase {
            let Some((&byte, rest)) = bytes.split_first() else {
                return;
            };
            bytes = rest;
            match byte {
                b';' if matches!(value, 0 | 2) => self.phase = Phase::Title,
                b';' if matches!(value, 10..=12) => self.phase = Phase::Color(value as u8),
                b';' if value == 4 => self.phase = Phase::Index,
                b';' if value == 104 => self.phase = Phase::ResetIndex,
                b'0'..=b'9' if digits < 5 => {
                    let Some(value) = value
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(u16::from(byte - b'0')))
                    else {
                        self.phase = Phase::Discard;
                        return;
                    };
                    self.phase = Phase::Selector {
                        value,
                        digits: digits + 1,
                    };
                }
                _ => {
                    self.phase = Phase::Discard;
                    return;
                }
            }
        }
        if matches!(
            self.phase,
            Phase::Index | Phase::IndexedColor(_) | Phase::ResetIndex
        ) {
            self.indexed_data(bytes);
            return;
        }
        let capacity = match self.phase {
            Phase::Title => MAX_TITLE_BYTES,
            Phase::Color(_) => MAX_COLOR_BYTES,
            _ => return,
        };
        self.append_data(bytes, capacity);
    }

    #[inline]
    fn append_data(&mut self, bytes: &[u8], capacity: usize) {
        let candidate = self.active ^ 1;
        let length = self.lengths[candidate];
        if bytes.len() > capacity - length {
            self.phase = Phase::Discard;
            return;
        }
        match bytes {
            [byte] => self.buffers[candidate][length] = *byte,
            [] => return,
            _ => self.buffers[candidate][length..length + bytes.len()].copy_from_slice(bytes),
        }
        self.lengths[candidate] += bytes.len();
    }

    #[inline(never)]
    pub(crate) fn event(&mut self, event: OscEvent<'_>) -> Outcome {
        match event {
            OscEvent::Start => {
                self.operation_count = 0;
                self.reply_head = 0;
                self.reply_end = 0;
                self.index = 0;
                self.index_digits = 0;
                self.lengths[self.active ^ 1] = 0;
                self.phase = Phase::Selector {
                    value: 0,
                    digits: 0,
                };
            }
            OscEvent::Data(bytes) => self.data(bytes),
            OscEvent::End(terminator) => {
                let candidate = self.active ^ 1;
                let bytes = &self.buffers[candidate][..self.lengths[candidate]];
                let phase = std::mem::take(&mut self.phase);
                return match phase {
                    Phase::Title if std::str::from_utf8(bytes).is_ok() => {
                        if bytes != &self.buffers[self.active][..self.lengths[self.active]] {
                            self.active = candidate;
                            self.changed = true;
                        }
                        Outcome::Done
                    }
                    Phase::Color(selector) => {
                        let entry = role(u16::from(selector));
                        if bytes == b"?" {
                            Outcome::Reply(ColorReport {
                                target: ColorTarget::Dynamic(selector),
                                color: self.palette.color(entry),
                                terminator,
                            })
                        } else if let Some(color) = parse_color(bytes) {
                            self.palette_changed |= self.palette.set_override(entry, color);
                            Outcome::Done
                        } else {
                            Outcome::Ignored
                        }
                    }
                    Phase::Selector {
                        value: selector @ 110..=112,
                        digits: 1..=5,
                    } => {
                        self.palette_changed |= self.palette.reset(role(selector - 100));
                        Outcome::Done
                    }
                    Phase::IndexedColor(index) => {
                        if !self.finish_color(index) {
                            return Outcome::Ignored;
                        }
                        self.execute_operations(terminator)
                    }
                    Phase::ResetIndex => {
                        let Some(index) = self.finished_index() else {
                            return Outcome::Ignored;
                        };
                        if !self.push(Operation::Reset(index)) {
                            return Outcome::Ignored;
                        }
                        self.execute_operations(terminator)
                    }
                    Phase::Selector {
                        value: 104,
                        digits: 1..=5,
                    } => {
                        self.palette_changed |= self.palette.reset_indexed();
                        Outcome::Done
                    }
                    _ => Outcome::Ignored,
                };
            }
            OscEvent::Cancel => {
                self.phase = Phase::Idle;
                self.operation_count = 0;
            }
        }
        Outcome::Done
    }

    #[inline(never)]
    fn indexed_data(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match self.phase {
                Phase::Index | Phase::ResetIndex => {
                    let byte = bytes[0];
                    bytes = &bytes[1..];
                    match byte {
                        b'0'..=b'9' if self.index_digits < 3 => {
                            self.index = self.index * 10 + u16::from(byte - b'0');
                            self.index_digits += 1;
                            if self.index > 255 {
                                self.phase = Phase::Discard;
                                return;
                            }
                        }
                        b';' => {
                            let Some(index) = self.finished_index() else {
                                self.phase = Phase::Discard;
                                return;
                            };
                            self.index = 0;
                            self.index_digits = 0;
                            if matches!(self.phase, Phase::ResetIndex) {
                                if !self.push(Operation::Reset(index)) {
                                    self.phase = Phase::Discard;
                                    return;
                                }
                            } else {
                                self.lengths[self.active ^ 1] = 0;
                                self.phase = Phase::IndexedColor(index);
                            }
                        }
                        _ => {
                            self.phase = Phase::Discard;
                            return;
                        }
                    }
                }
                Phase::IndexedColor(index) => {
                    let length = bytes
                        .iter()
                        .position(|&byte| byte == b';')
                        .unwrap_or(bytes.len());
                    let candidate = self.active ^ 1;
                    let offset = self.lengths[candidate];
                    if length > MAX_COLOR_BYTES - offset {
                        self.phase = Phase::Discard;
                        return;
                    }
                    self.buffers[candidate][offset..offset + length]
                        .copy_from_slice(&bytes[..length]);
                    self.lengths[candidate] += length;
                    if length == bytes.len() {
                        return;
                    }
                    if !self.finish_color(index) {
                        self.phase = Phase::Discard;
                        return;
                    }
                    self.phase = Phase::Index;
                    bytes = &bytes[length + 1..];
                }
                _ => return,
            }
        }
    }

    fn finished_index(&self) -> Option<u8> {
        (self.index_digits != 0).then_some(self.index as u8)
    }

    fn push(&mut self, operation: Operation) -> bool {
        if self.operation_count == self.operations.len() {
            return false;
        }
        self.operations[self.operation_count] = operation;
        self.operation_count += 1;
        true
    }

    fn finish_color(&mut self, index: u8) -> bool {
        let candidate = self.active ^ 1;
        let bytes = &self.buffers[candidate][..self.lengths[candidate]];
        let operation = if bytes == b"?" {
            Operation::Query(index)
        } else if let Some(color) = parse_color(bytes) {
            Operation::Set(index, color)
        } else {
            return false;
        };
        self.push(operation)
    }

    #[inline(never)]
    fn execute_operations(&mut self, terminator: OscTerminator) -> Outcome {
        let mut replies = 0;
        for index in 0..self.operation_count {
            match self.operations[index] {
                Operation::Set(index, color) => {
                    self.palette_changed |= self
                        .palette
                        .set_override(PaletteEntry::Indexed(index), color)
                }
                Operation::Reset(index) => {
                    self.palette_changed |= self.palette.reset(PaletteEntry::Indexed(index))
                }
                Operation::Query(index) => {
                    let color = self.palette.color(PaletteEntry::Indexed(index));
                    // Captured replies compact into consumed operation slots.
                    self.operations[replies] = Operation::Reply(index, color);
                    replies += 1;
                }
                Operation::Reply(..) => unreachable!("A command contains unexecuted operations"),
            }
        }
        self.operation_count = 0;
        self.reply_head = 0;
        self.reply_end = replies;
        self.terminator = terminator;
        if replies != 0 {
            Outcome::PaletteReplies
        } else {
            Outcome::Done
        }
    }

    pub(crate) fn has_replies(&self) -> bool {
        self.reply_head < self.reply_end
    }

    pub(crate) fn next_reply(&mut self) -> Option<ColorReport> {
        if !self.has_replies() {
            return None;
        }
        let Operation::Reply(index, color) = self.operations[self.reply_head] else {
            unreachable!("A captured palette reply is present");
        };
        self.reply_head += 1;
        Some(ColorReport {
            target: ColorTarget::Indexed(index),
            color,
            terminator: self.terminator,
        })
    }
}

fn role(selector: u16) -> PaletteEntry {
    match selector {
        10 => PaletteEntry::Foreground,
        11 => PaletteEntry::Background,
        12 => PaletteEntry::Cursor,
        _ => unreachable!("A supported dynamic color selector is present"),
    }
}

fn parse_color(bytes: &[u8]) -> Option<Rgb> {
    // Sharp notation supplies high bits; rgb: scales each field to 16 bits.
    let channels = if let Some(hex) = bytes.strip_prefix(b"#") {
        let width = match hex.len() {
            3 => 1,
            6 => 2,
            9 => 3,
            12 => 4,
            _ => return None,
        };
        let mut channels = [0; 3];
        for (channel, digits) in channels.iter_mut().zip(hex.chunks_exact(width)) {
            *channel = (hex_value(digits)? << ((4 - width) * 4) >> 8) as u8;
        }
        channels
    } else if bytes.get(..4)?.eq_ignore_ascii_case(b"rgb:") {
        let mut parts = bytes[4..].split(|&byte| byte == b'/');
        let mut channels = [0; 3];
        for channel in &mut channels {
            let digits = parts.next()?;
            let value = u32::from(hex_value(digits)?);
            let maximum = (1_u32 << (4 * digits.len())) - 1;
            *channel = ((value * 65535 / maximum) >> 8) as u8;
        }
        if parts.next().is_some() {
            return None;
        }
        channels
    } else {
        return None;
    };
    Some(Rgb {
        red: channels[0],
        green: channels[1],
        blue: channels[2],
    })
}

fn hex_value(bytes: &[u8]) -> Option<u16> {
    if !(1..=4).contains(&bytes.len()) {
        return None;
    }
    let mut value = 0_u16;
    for &byte in bytes {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        value = (value << 4) | u16::from(digit);
    }
    Some(value)
}
