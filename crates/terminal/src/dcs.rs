use lune_vt::DcsEvent;

/// Maximum capability queries accepted in one XTGETTCAP (`DCS + q`) batch.
pub const MAX_TERMCAP_QUERIES: usize = 32;
pub(crate) const MAX_TERMCAP_NAME_BYTES: usize = 32;

#[derive(Clone, Copy)]
pub(crate) enum StatusQuery {
    Rendition,
    Margins,
    CursorStyle,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Termcap {
    Tn,
    Co,
    Colors,
    Rgb,
    Tc,
    Sync,
    Smulx,
    Setulc,
    Ms,
}

impl Termcap {
    fn from_name(name: &[u8]) -> Option<Self> {
        match name {
            b"TN" => Some(Self::Tn),
            b"Co" => Some(Self::Co),
            b"colors" => Some(Self::Colors),
            b"RGB" => Some(Self::Rgb),
            b"Tc" => Some(Self::Tc),
            b"Sync" => Some(Self::Sync),
            b"Smulx" => Some(Self::Smulx),
            b"Setulc" => Some(Self::Setulc),
            b"Ms" => Some(Self::Ms),
            _ => None,
        }
    }

    pub(crate) const fn name(self) -> &'static [u8] {
        match self {
            Self::Tn => b"TN",
            Self::Co => b"Co",
            Self::Colors => b"colors",
            Self::Rgb => b"RGB",
            Self::Tc => b"Tc",
            Self::Sync => b"Sync",
            Self::Smulx => b"Smulx",
            Self::Setulc => b"Setulc",
            Self::Ms => b"Ms",
        }
    }

    pub(crate) const fn value(self) -> Option<&'static [u8]> {
        match self {
            Self::Tn => Some(b"xterm-256color"),
            Self::Co | Self::Colors => Some(b"256"),
            Self::Rgb | Self::Tc | Self::Sync => None,
            Self::Smulx => Some(b"\x1b[4:%p1%dm"),
            Self::Setulc => {
                Some(b"\x1b[58:2::%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%d%;m")
            }
            Self::Ms => Some(b"\x1b]52;%p1%s;%p2%s\x1b\\"),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum TermcapReply {
    Supported(Termcap),
    Unsupported {
        name: [u8; MAX_TERMCAP_NAME_BYTES],
        len: u8,
    },
    Invalid,
}

pub(crate) enum DcsOutcome {
    Status(StatusQuery),
    Termcap,
}

#[derive(Clone, Copy, Default)]
enum Phase {
    #[default]
    Inactive,
    StatusEmpty,
    StatusRendition,
    StatusMargins,
    StatusSpace,
    StatusCursorStyle,
    StatusUnsupported,
    Termcap,
    TermcapDiscard,
}

pub(crate) struct Dcs {
    phase: Phase,
    name: [u8; MAX_TERMCAP_NAME_BYTES],
    name_len: u8,
    high_nibble: Option<u8>,
    item_invalid: bool,
    replies: [TermcapReply; MAX_TERMCAP_QUERIES],
    query_count: usize,
    reply_head: usize,
    reply_end: usize,
}

impl Default for Dcs {
    fn default() -> Self {
        Self {
            phase: Phase::Inactive,
            name: [0; MAX_TERMCAP_NAME_BYTES],
            name_len: 0,
            high_nibble: None,
            item_invalid: false,
            replies: [TermcapReply::Invalid; MAX_TERMCAP_QUERIES],
            query_count: 0,
            reply_head: 0,
            reply_end: 0,
        }
    }
}

impl Dcs {
    #[inline]
    pub(crate) fn needs_data(&self) -> bool {
        !matches!(
            self.phase,
            Phase::Inactive | Phase::StatusUnsupported | Phase::TermcapDiscard
        )
    }

    #[inline]
    pub(crate) fn has_replies(&self) -> bool {
        self.reply_head < self.reply_end
    }

    #[inline]
    pub(crate) fn next_reply(&mut self) -> Option<TermcapReply> {
        if !self.has_replies() {
            return None;
        }
        let reply = self.replies[self.reply_head];
        self.reply_head += 1;
        Some(reply)
    }

    #[inline]
    pub(crate) fn data(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if matches!(self.phase, Phase::Termcap) {
            self.termcap_data(bytes);
            return;
        }
        self.phase = match (self.phase, bytes) {
            (Phase::Inactive | Phase::StatusUnsupported | Phase::TermcapDiscard, _) => return,
            (Phase::StatusEmpty, b"m") => Phase::StatusRendition,
            (Phase::StatusEmpty, b"r") => Phase::StatusMargins,
            (Phase::StatusEmpty, b" ") => Phase::StatusSpace,
            (Phase::StatusEmpty, b" q") | (Phase::StatusSpace, b"q") => Phase::StatusCursorStyle,
            _ => Phase::StatusUnsupported,
        };
    }

    #[cold]
    fn termcap_data(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b';' {
                if !self.finish_termcap_item() {
                    self.phase = Phase::TermcapDiscard;
                    return;
                }
                continue;
            }
            if self.item_invalid {
                continue;
            }
            let Some(nibble) = hex_nibble(byte) else {
                self.item_invalid = true;
                continue;
            };
            if let Some(high) = self.high_nibble.take() {
                let index = usize::from(self.name_len);
                if index == MAX_TERMCAP_NAME_BYTES {
                    self.item_invalid = true;
                } else {
                    self.name[index] = (high << 4) | nibble;
                    self.name_len += 1;
                }
            } else {
                self.high_nibble = Some(nibble);
            }
        }
    }

    #[cold]
    fn finish_termcap_item(&mut self) -> bool {
        if self.query_count == MAX_TERMCAP_QUERIES {
            return false;
        }
        let len = usize::from(self.name_len);
        let reply = if self.item_invalid || self.high_nibble.is_some() || len == 0 {
            TermcapReply::Invalid
        } else if let Some(cap) = Termcap::from_name(&self.name[..len]) {
            TermcapReply::Supported(cap)
        } else {
            TermcapReply::Unsupported {
                name: self.name,
                len: self.name_len,
            }
        };
        self.replies[self.query_count] = reply;
        self.query_count += 1;
        self.name_len = 0;
        self.high_nibble = None;
        self.item_invalid = false;
        true
    }

    #[inline(always)]
    pub(crate) fn event(&mut self, event: DcsEvent<'_>) -> Option<DcsOutcome> {
        match event {
            DcsEvent::Start(header) => {
                self.phase = if header.final_byte == b'q'
                    && header.private_markers.is_empty()
                    && header.parameters.is_empty()
                {
                    match header.intermediates {
                        b"$" => Phase::StatusEmpty,
                        b"+" => {
                            self.name_len = 0;
                            self.high_nibble = None;
                            self.item_invalid = false;
                            self.query_count = 0;
                            self.reply_head = 0;
                            self.reply_end = 0;
                            Phase::Termcap
                        }
                        _ => Phase::Inactive,
                    }
                } else {
                    Phase::Inactive
                };
            }
            DcsEvent::Data(bytes) => self.data(bytes),
            DcsEvent::Cancel => {
                self.phase = Phase::Inactive;
                self.query_count = 0;
                self.name_len = 0;
                self.high_nibble = None;
                self.item_invalid = false;
            }
            DcsEvent::End => {
                return match std::mem::take(&mut self.phase) {
                    Phase::Inactive | Phase::TermcapDiscard => None,
                    Phase::StatusRendition => Some(DcsOutcome::Status(StatusQuery::Rendition)),
                    Phase::StatusMargins => Some(DcsOutcome::Status(StatusQuery::Margins)),
                    Phase::StatusCursorStyle => Some(DcsOutcome::Status(StatusQuery::CursorStyle)),
                    Phase::StatusEmpty | Phase::StatusSpace | Phase::StatusUnsupported => {
                        Some(DcsOutcome::Status(StatusQuery::Unsupported))
                    }
                    Phase::Termcap => {
                        if !self.finish_termcap_item() {
                            self.query_count = 0;
                            return None;
                        }
                        self.reply_head = 0;
                        self.reply_end = self.query_count;
                        self.query_count = 0;
                        (self.reply_end != 0).then_some(DcsOutcome::Termcap)
                    }
                };
            }
        }
        None
    }
}

#[inline]
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
