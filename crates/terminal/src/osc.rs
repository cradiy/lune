use std::collections::{TryReserveError, VecDeque};

use lune_vt::{OscEvent, OscTerminator};

use crate::reply::{ColorReport, ColorTarget};
use crate::sizing::{self, MAX_SIZED_TEXT_BYTES, MAX_SIZING_METADATA_BYTES};
use crate::{Palette, PaletteEntry, Rgb};

/// Maximum UTF-8 byte length of an OSC 0/2 window title.
pub const MAX_TITLE_BYTES: usize = 4096;
pub(crate) const MAX_SAVED_TITLES: usize = 10;
/// Maximum encoded byte length of an OSC 7 working directory URI.
pub const MAX_WORKING_DIRECTORY_URI_BYTES: usize = MAX_TITLE_BYTES;
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
    WorkingDirectory,
    SizedText,
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
    Replies,
    SizedText,
}

#[derive(Clone, Copy)]
enum Operation {
    Set(u8, Rgb),
    Query(u8),
    Reset(u8),
    Reply(u8, Rgb),
    SetDynamic(u8, Rgb),
    QueryDynamic(u8),
    ReplyDynamic(u8, Rgb),
}

pub(crate) struct Osc {
    buffers: [[u8; MAX_TITLE_BYTES]; 2],
    lengths: [usize; 2],
    active: usize,
    phase: Phase,
    changed: bool,
    saved_titles: VecDeque<Vec<u8>>,
    palette: Palette,
    palette_changed: bool,
    operations: [Operation; MAX_OSC_PALETTE_ENTRIES],
    operation_count: usize,
    reply_head: usize,
    reply_end: usize,
    terminator: OscTerminator,
    index: u16,
    index_digits: u8,
    directory: [u8; MAX_WORKING_DIRECTORY_URI_BYTES],
    directory_length: usize,
    directory_changed: bool,
    sized_text: Vec<u8>,
}

impl Osc {
    pub(crate) fn new(palette: Palette) -> Self {
        Self {
            buffers: [[0; MAX_TITLE_BYTES]; 2],
            lengths: [0; 2],
            active: 0,
            phase: Phase::Idle,
            changed: false,
            saved_titles: VecDeque::new(),
            palette,
            palette_changed: false,
            operations: [Operation::Query(0); MAX_OSC_PALETTE_ENTRIES],
            operation_count: 0,
            reply_head: 0,
            reply_end: 0,
            terminator: OscTerminator::St,
            index: 0,
            index_digits: 0,
            directory: [0; MAX_WORKING_DIRECTORY_URI_BYTES],
            directory_length: 0,
            directory_changed: false,
            sized_text: Vec::new(),
        }
    }

    pub(crate) fn palette(&self) -> &Palette {
        &self.palette
    }

    pub(crate) fn take_palette_change(&mut self) -> bool {
        std::mem::take(&mut self.palette_changed)
    }

    pub(crate) fn reset_indexed(&mut self) {
        self.palette_changed |= self.palette.reset_indexed();
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

    pub(crate) fn set_base_from(&mut self, theme: &Palette) -> bool {
        let changed = self.palette.set_base_from(theme);
        self.palette_changed |= changed;
        changed
    }

    pub(crate) fn title(&self) -> &str {
        std::str::from_utf8(&self.buffers[self.active][..self.lengths[self.active]])
            .expect("Committed window titles are valid UTF-8")
    }

    pub(crate) fn take_title_change(&mut self) -> Option<&str> {
        self.take_title_changed().then(|| self.title())
    }

    pub(crate) fn take_title_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    #[cold]
    pub(crate) fn push_title(&mut self) -> Result<(), TryReserveError> {
        let title = &self.buffers[self.active][..self.lengths[self.active]];
        if self.saved_titles.len() == MAX_SAVED_TITLES {
            let oldest = self
                .saved_titles
                .front_mut()
                .expect("The title stack is full");
            oldest.try_reserve_exact(title.len().saturating_sub(oldest.len()))?;
            oldest.clear();
            oldest.extend_from_slice(title);
            self.saved_titles.rotate_left(1);
        } else {
            self.saved_titles
                .try_reserve_exact(MAX_SAVED_TITLES - self.saved_titles.len())?;
            let mut saved = Vec::new();
            saved.try_reserve_exact(title.len())?;
            saved.extend_from_slice(title);
            self.saved_titles.push_back(saved);
        }
        Ok(())
    }

    #[cold]
    pub(crate) fn pop_title(&mut self) {
        let Some(title) = self.saved_titles.pop_back() else {
            return;
        };
        let active = &mut self.buffers[self.active];
        if active[..self.lengths[self.active]] != title {
            active[..title.len()].copy_from_slice(&title);
            self.lengths[self.active] = title.len();
            self.changed = true;
        }
    }

    pub(crate) fn working_directory_uri(&self) -> Option<&str> {
        (self.directory_length != 0).then(|| {
            std::str::from_utf8(&self.directory[..self.directory_length])
                .expect("Committed directory URIs are valid ASCII")
        })
    }

    pub(crate) fn take_working_directory_change(&mut self) -> bool {
        std::mem::take(&mut self.directory_changed)
    }

    pub(crate) fn reset(&mut self) {
        self.directory_changed |= self.directory_length != 0;
        self.directory_length = 0;
        self.changed |= self.lengths[self.active] != 0;
        self.lengths = [0; 2];
        self.saved_titles.clear();
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
                | Phase::WorkingDirectory
                | Phase::SizedText
                | Phase::Color(_)
                | Phase::Index
                | Phase::IndexedColor(_)
                | Phase::ResetIndex
        )
    }

    #[inline]
    pub(crate) fn data(&mut self, bytes: &[u8]) {
        if matches!(self.phase, Phase::Title) {
            self.append_text(bytes);
        } else if matches!(self.phase, Phase::SizedText) {
            self.append_sized_text(bytes);
        } else if matches!(self.phase, Phase::Selector { .. }) {
            self.command_data(bytes);
        } else {
            self.batch_data(bytes);
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
                b';' if value == 7 => self.phase = Phase::WorkingDirectory,
                b';' if value == 66 => {
                    self.sized_text.clear();
                    self.phase = Phase::SizedText;
                }
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
        if matches!(self.phase, Phase::Title) {
            self.append_text(bytes);
        } else if matches!(self.phase, Phase::SizedText) {
            self.append_sized_text(bytes);
        } else {
            self.batch_data(bytes);
        }
    }

    #[inline]
    fn append_text(&mut self, bytes: &[u8]) {
        let candidate = self.active ^ 1;
        let length = self.lengths[candidate];
        if bytes.len() > MAX_TITLE_BYTES - length {
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

    fn append_sized_text(&mut self, bytes: &[u8]) {
        const LIMIT: usize = MAX_SIZED_TEXT_BYTES + MAX_SIZING_METADATA_BYTES + 1;
        if bytes.len() > LIMIT - self.sized_text.len()
            || self.sized_text.try_reserve(bytes.len()).is_err()
        {
            self.phase = Phase::Discard;
            return;
        }
        self.sized_text.extend_from_slice(bytes);
    }

    pub(crate) fn sized_text(&self) -> Option<(crate::TextSize, usize, &str)> {
        sizing::parse(&self.sized_text)
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
                    Phase::WorkingDirectory => self.commit_directory(candidate),
                    Phase::SizedText if self.sized_text().is_some() => Outcome::SizedText,
                    Phase::Color(selector) if self.operation_count == 0 => {
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
                    Phase::Color(selector) => {
                        if !self.finish_color(ColorTarget::Dynamic(selector)) {
                            return Outcome::Ignored;
                        }
                        self.execute_operations(terminator)
                    }
                    Phase::Selector {
                        value: selector @ 110..=112,
                        digits: 1..=5,
                    } => {
                        self.palette_changed |= self.palette.reset(role(selector - 100));
                        Outcome::Done
                    }
                    Phase::IndexedColor(index) => {
                        if !self.finish_color(ColorTarget::Indexed(index)) {
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

    #[cold]
    fn commit_directory(&mut self, candidate: usize) -> Outcome {
        let length = self.lengths[candidate];
        let bytes = &self.buffers[candidate][..length];
        if !bytes.is_empty() && !valid_directory_uri(bytes) {
            return Outcome::Ignored;
        }
        if bytes != &self.directory[..self.directory_length] {
            self.directory[..length].copy_from_slice(bytes);
            self.directory_length = length;
            self.directory_changed = true;
        }
        Outcome::Done
    }

    #[inline(never)]
    fn batch_data(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match self.phase {
                Phase::WorkingDirectory => {
                    self.append_text(bytes);
                    return;
                }
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
                phase @ (Phase::IndexedColor(_) | Phase::Color(_)) => {
                    let candidate = self.active ^ 1;
                    let offset = self.lengths[candidate];
                    if let [byte] = bytes
                        && *byte != b';'
                    {
                        if offset >= MAX_COLOR_BYTES {
                            self.phase = Phase::Discard;
                            return;
                        }
                        self.buffers[candidate][offset] = *byte;
                        self.lengths[candidate] += 1;
                        return;
                    }
                    let Some(capacity) = MAX_COLOR_BYTES.checked_sub(offset) else {
                        self.phase = Phase::Discard;
                        return;
                    };
                    let field = &bytes[..bytes.len().min(capacity + 1)];
                    let length = field
                        .iter()
                        .position(|&byte| byte == b';')
                        .unwrap_or(field.len());
                    if length > capacity {
                        self.phase = Phase::Discard;
                        return;
                    }
                    self.buffers[candidate][offset..offset + length]
                        .copy_from_slice(&bytes[..length]);
                    self.lengths[candidate] += length;
                    if length == bytes.len() {
                        return;
                    }
                    let target = match phase {
                        Phase::IndexedColor(index) => ColorTarget::Indexed(index),
                        Phase::Color(selector) => ColorTarget::Dynamic(selector),
                        _ => unreachable!("A color field is present"),
                    };
                    if !self.finish_color(target) {
                        self.phase = Phase::Discard;
                        return;
                    }
                    self.phase = match target {
                        ColorTarget::Indexed(_) => Phase::Index,
                        ColorTarget::Dynamic(selector) if selector < 12 => {
                            self.lengths[candidate] = 0;
                            Phase::Color(selector + 1)
                        }
                        ColorTarget::Dynamic(_) => Phase::Discard,
                    };
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

    fn finish_color(&mut self, target: ColorTarget) -> bool {
        let candidate = self.active ^ 1;
        let bytes = &self.buffers[candidate][..self.lengths[candidate]];
        let operation = if bytes == b"?" {
            match target {
                ColorTarget::Indexed(index) => Operation::Query(index),
                ColorTarget::Dynamic(selector) => Operation::QueryDynamic(selector),
            }
        } else if let Some(color) = parse_color(bytes) {
            match target {
                ColorTarget::Indexed(index) => Operation::Set(index, color),
                ColorTarget::Dynamic(selector) => Operation::SetDynamic(selector, color),
            }
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
                Operation::SetDynamic(selector, color) => {
                    self.palette_changed |=
                        self.palette.set_override(role(u16::from(selector)), color)
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
                Operation::QueryDynamic(selector) => {
                    let color = self.palette.color(role(u16::from(selector)));
                    self.operations[replies] = Operation::ReplyDynamic(selector, color);
                    replies += 1;
                }
                Operation::Reply(..) | Operation::ReplyDynamic(..) => {
                    unreachable!("A command contains unexecuted operations")
                }
            }
        }
        self.operation_count = 0;
        self.reply_head = 0;
        self.reply_end = replies;
        self.terminator = terminator;
        if replies != 0 {
            Outcome::Replies
        } else {
            Outcome::Done
        }
    }

    #[inline]
    pub(crate) fn has_replies(&self) -> bool {
        self.reply_head < self.reply_end
    }

    #[inline]
    pub(crate) fn next_reply(&mut self) -> Option<ColorReport> {
        if !self.has_replies() {
            return None;
        }
        let (target, color) = match self.operations[self.reply_head] {
            Operation::Reply(index, color) => (ColorTarget::Indexed(index), color),
            Operation::ReplyDynamic(selector, color) => (ColorTarget::Dynamic(selector), color),
            _ => unreachable!("A captured color reply is present"),
        };
        self.reply_head += 1;
        Some(ColorReport {
            target,
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

fn valid_directory_uri(bytes: &[u8]) -> bool {
    let Some((prefix, rest)) = bytes.split_at_checked(b"file://".len()) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case(b"file://") {
        return false;
    }
    let Some(path_start) = rest.iter().position(|&byte| byte == b'/') else {
        return false;
    };
    let (host, path) = rest.split_at(path_start);
    let valid_host = if let Some(address) = host.strip_prefix(b"[") {
        address
            .strip_suffix(b"]")
            .and_then(|address| std::str::from_utf8(address).ok())
            .is_some_and(|address| address.parse::<std::net::Ipv6Addr>().is_ok())
    } else {
        valid_uri_component(host, false)
    };
    valid_host && valid_uri_component(path, true)
}

fn valid_uri_component(mut bytes: &[u8], path: bool) -> bool {
    while let Some((&byte, rest)) = bytes.split_first() {
        bytes = match byte {
            b'%' => match rest {
                [high, low, remaining @ ..]
                    if high.is_ascii_hexdigit()
                        && low.is_ascii_hexdigit()
                        && (*high, *low) != (b'0', b'0') =>
                {
                    remaining
                }
                _ => return false,
            },
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'=' => rest,
            b'/' | b':' | b'@' if path => rest,
            _ => return false,
        };
    }
    true
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
