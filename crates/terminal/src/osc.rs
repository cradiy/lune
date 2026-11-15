use lune_vt::OscEvent;

/// Maximum UTF-8 byte length of an OSC 0/2 window title.
pub const MAX_TITLE_BYTES: usize = 4096;

#[derive(Clone, Copy, Default)]
enum Phase {
    #[default]
    Idle,
    Selector {
        value: u16,
        digits: u8,
    },
    Title,
    Discard,
}

pub(crate) struct Osc {
    buffers: [[u8; MAX_TITLE_BYTES]; 2],
    lengths: [usize; 2],
    active: usize,
    phase: Phase,
    changed: bool,
}

impl Default for Osc {
    fn default() -> Self {
        Self {
            buffers: [[0; MAX_TITLE_BYTES]; 2],
            lengths: [0; 2],
            active: 0,
            phase: Phase::Idle,
            changed: false,
        }
    }
}

impl Osc {
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
    }

    #[inline]
    pub(crate) fn needs_data(&self) -> bool {
        matches!(self.phase, Phase::Selector { .. } | Phase::Title)
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
        if !matches!(self.phase, Phase::Title) {
            return;
        }
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

    /// Returns true for a completed unsupported or invalid command.
    #[inline(never)]
    pub(crate) fn event(&mut self, event: OscEvent<'_>) -> bool {
        match event {
            OscEvent::Start => {
                self.lengths[self.active ^ 1] = 0;
                self.phase = Phase::Selector {
                    value: 0,
                    digits: 0,
                };
            }
            OscEvent::Data(bytes) => self.data(bytes),
            OscEvent::End(_) => {
                let candidate = self.active ^ 1;
                let bytes = &self.buffers[candidate][..self.lengths[candidate]];
                let accepted =
                    matches!(self.phase, Phase::Title) && std::str::from_utf8(bytes).is_ok();
                if accepted && bytes != &self.buffers[self.active][..self.lengths[self.active]] {
                    self.active = candidate;
                    self.changed = true;
                }
                self.phase = Phase::Idle;
                return !accepted;
            }
            OscEvent::Cancel => self.phase = Phase::Idle,
        }
        false
    }
}
