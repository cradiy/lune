const START: &[u8] = b"\x1b[200~";
const END: &[u8] = b"\x1b[201~";

#[derive(Clone, Copy, Debug)]
enum Stage {
    Start,
    Payload,
    End,
    Done,
}

/// A borrowed paste stream with framing fixed at creation.
/// Send [`Self::bytes`] and acknowledge accepted prefixes with [`Self::consume`].
/// Framing and acknowledgement take constant time without allocations.
#[derive(Clone, Copy, Debug)]
#[must_use]
pub struct Paste<'a> {
    payload: &'a [u8],
    offset: usize,
    stage: Stage,
    bracketed: bool,
}

impl<'a> Paste<'a> {
    pub(crate) fn new(payload: &'a str, bracketed: bool) -> Self {
        Self {
            payload: payload.as_bytes(),
            offset: 0,
            stage: if bracketed {
                Stage::Start
            } else if payload.is_empty() {
                Stage::Done
            } else {
                Stage::Payload
            },
            bracketed,
        }
    }

    /// Current unsent chunk; empty means completion. Empty bracketed payloads
    /// still emit both markers.
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        let bytes = match self.stage {
            Stage::Start => START,
            Stage::Payload => self.payload,
            Stage::End => END,
            Stage::Done => return &[],
        };
        &bytes[self.offset..]
    }

    /// Whether every byte, including any end marker, has been acknowledged.
    #[inline]
    pub fn is_empty(&self) -> bool {
        matches!(self.stage, Stage::Done)
    }

    /// Unsent byte count across the payload and any remaining markers.
    #[inline]
    pub fn remaining_bytes(&self) -> usize {
        match self.stage {
            Stage::Start => START.len() - self.offset + self.payload.len() + END.len(),
            Stage::Payload => {
                self.payload.len() - self.offset + if self.bracketed { END.len() } else { 0 }
            }
            Stage::End => END.len() - self.offset,
            Stage::Done => 0,
        }
    }

    /// Acknowledges current-chunk bytes, including partial UTF-8 scalars.
    /// Oversized counts fail unchanged; zero succeeds without advancing.
    #[inline]
    #[must_use]
    pub fn consume(&mut self, count: usize) -> bool {
        let remaining = self.bytes().len();
        if count > remaining {
            return false;
        }
        self.offset += count;
        if count == remaining && remaining != 0 {
            self.offset = 0;
            self.stage = match self.stage {
                Stage::Start if self.payload.is_empty() => Stage::End,
                Stage::Start => Stage::Payload,
                Stage::Payload if self.bracketed => Stage::End,
                Stage::Payload | Stage::End | Stage::Done => Stage::Done,
            };
        }
        true
    }
}
