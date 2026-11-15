use lune_vt::OscEvent;

use crate::reply::ColorReport;
use crate::{Palette, PaletteEntry, Rgb};

/// Maximum UTF-8 byte length of an OSC 0/2 window title.
pub const MAX_TITLE_BYTES: usize = 4096;
const MAX_COLOR_BYTES: usize = 18;

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
    Discard,
}

pub(crate) enum Outcome {
    Done,
    Ignored,
    Reply(ColorReport),
}

pub(crate) struct Osc {
    buffers: [[u8; MAX_TITLE_BYTES]; 2],
    lengths: [usize; 2],
    active: usize,
    phase: Phase,
    changed: bool,
    palette: Palette,
    palette_changed: bool,
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
    }

    #[inline]
    pub(crate) fn needs_data(&self) -> bool {
        matches!(
            self.phase,
            Phase::Selector { .. } | Phase::Title | Phase::Color(_)
        )
    }

    #[inline(never)]
    pub(crate) fn data(&mut self, mut bytes: &[u8]) {
        while let Phase::Selector { value, digits } = self.phase {
            let Some((&byte, rest)) = bytes.split_first() else {
                return;
            };
            bytes = rest;
            match byte {
                b';' if matches!(value, 0 | 2) => self.phase = Phase::Title,
                b';' if matches!(value, 10..=12) => self.phase = Phase::Color(value as u8),
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
        let capacity = match self.phase {
            Phase::Title => MAX_TITLE_BYTES,
            Phase::Color(_) => MAX_COLOR_BYTES,
            _ => return,
        };
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
                                selector,
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
                    _ => Outcome::Ignored,
                };
            }
            OscEvent::Cancel => self.phase = Phase::Idle,
        }
        Outcome::Done
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
