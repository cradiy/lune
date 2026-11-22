use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use super::Animation;
use crate::pixels::QUOTA;

const WORD_BITS: usize = u64::BITS as usize;

#[derive(Debug, Default)]
pub(super) struct TimedFrames(Vec<u64>);

impl TimedFrames {
    pub(super) fn push(&mut self, index: usize, timed: bool) -> Result<(), &'static [u8]> {
        if index.is_multiple_of(WORD_BITS) {
            self.0.try_reserve(1).map_err(|_| QUOTA)?;
            self.0.push(0);
        }
        self.set(index, timed);
        Ok(())
    }

    pub(super) fn set(&mut self, index: usize, timed: bool) {
        let word = &mut self.0[index / WORD_BITS];
        let bit = 1 << (index % WORD_BITS);
        *word = if timed { *word | bit } else { *word & !bit };
    }

    pub(super) fn remove(&mut self, index: usize, remaining: usize) {
        let start = index / WORD_BITS;
        let keep = (1 << (index % WORD_BITS)) - 1;
        let prefix = self.0[start] & keep;
        // Shift the suffix one bit toward the deleted frame, including word boundaries.
        for word in start..self.0.len() {
            let next = self.0.get(word + 1).copied().unwrap_or_default();
            self.0[word] = (self.0[word] >> 1) | (next << (WORD_BITS - 1));
        }
        self.0[start] = prefix | (self.0[start] & !keep);
        self.0.truncate(remaining.div_ceil(WORD_BITS));
    }

    pub(super) fn shrink_to_fit(&mut self) {
        self.0.shrink_to_fit();
    }

    #[cold]
    fn first_from(&self, index: usize) -> Option<usize> {
        let word = index / WORD_BITS;
        let bits = self.0.get(word)? & (u64::MAX << (index % WORD_BITS));
        if bits != 0 {
            return Some(word * WORD_BITS + bits.trailing_zeros() as usize);
        }
        let (offset, &bits) = self.0[word + 1..]
            .iter()
            .enumerate()
            .find(|&(_, &bits)| bits != 0)?;
        Some((word + 1 + offset) * WORD_BITS + bits.trailing_zeros() as usize)
    }

    #[cold]
    fn last(&self) -> Option<usize> {
        let (word, &bits) = self.0.iter().enumerate().rfind(|&(_, &bits)| bits != 0)?;
        Some(word * WORD_BITS + (WORD_BITS - 1) - bits.leading_zeros() as usize)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum PlaybackMode {
    #[default]
    Stopped,
    Loading,
    Running,
}

#[derive(Clone, Copy, Debug, Default)]
enum Timing {
    #[default]
    Start,
    Showing(Instant),
    Waiting(Instant),
}

#[derive(Debug, Default)]
pub(super) struct Playback {
    mode: PlaybackMode,
    timing: Timing,
    loop_limit: Option<NonZeroU32>,
    completed_loops: u32,
}

impl Playback {
    pub(super) fn retains_settings(&self) -> bool {
        self.mode != PlaybackMode::Stopped || self.loop_limit.is_some()
    }

    pub(super) fn restart_frame(&mut self) {
        self.timing = Timing::Start;
    }

    pub(super) fn configure(&mut self, mode: Option<PlaybackMode>, loops: u32) {
        if let Some(mode) = mode {
            if mode == PlaybackMode::Stopped || self.mode == PlaybackMode::Stopped {
                self.restart_frame();
            }
            self.mode = mode;
            self.completed_loops = 0;
        }
        if loops != 0 {
            self.loop_limit = NonZeroU32::new(loops - 1);
        }
    }

    fn loops_finished(&self) -> bool {
        self.loop_limit
            .is_some_and(|limit| self.completed_loops >= limit.get())
    }
}

impl Animation {
    pub(crate) fn suspend(&mut self) {
        if let Timing::Showing(shown) = self.playback.timing {
            self.playback.timing = Timing::Waiting(shown);
        }
    }

    #[inline]
    pub(crate) fn awaiting_frame(&self) -> bool {
        self.playback.mode == PlaybackMode::Loading
            && matches!(self.playback.timing, Timing::Showing(_))
            && self.timed_frame_at_or_after(self.current + 1).is_none()
    }

    pub(crate) fn playing(&self) -> bool {
        self.playback.mode != PlaybackMode::Stopped
    }

    pub(crate) fn advance(
        &mut self,
        now: Instant,
        original_revision: u64,
    ) -> (Option<Instant>, bool) {
        let previous = self.current;
        let next = self.advance_to(now);
        let changed = self.current != previous
            && self.frame_revision(previous).unwrap_or(original_revision)
                != self.revision().unwrap_or(original_revision);
        (next, changed)
    }

    fn advance_to(&mut self, now: Instant) -> Option<Instant> {
        if !self.playing() {
            return None;
        }
        let timing = self.playback.timing;
        if self.playback.loops_finished() || self.extra.is_empty() || self.total_gap_ms == 0 {
            let shown = match timing {
                Timing::Start => now,
                Timing::Showing(shown) | Timing::Waiting(shown) => shown,
            };
            self.playback.timing = Timing::Waiting(shown);
            return None;
        }

        let gap = self
            .frame(self.current)
            .expect("Current animation frame")
            .gap_ms();
        let gap = Duration::from_millis(u64::from(gap));
        let (mut shown, mut next_at) = match timing {
            Timing::Showing(shown) => (shown, shown.checked_add(gap)?),
            Timing::Start => (now, now.checked_add(gap)?),
            Timing::Waiting(shown) => (shown, shown.checked_add(gap)?.max(now)),
        };
        let mut index = self.current + 1;
        while next_at <= now {
            let Some((frame, gap)) = self.timed_frame_at_or_after(index) else {
                if self.playback.mode == PlaybackMode::Loading {
                    self.playback.timing = Timing::Waiting(shown);
                    return None;
                }
                self.playback.completed_loops = self.playback.completed_loops.saturating_add(1);
                if self.playback.loops_finished() {
                    self.playback.timing = Timing::Waiting(shown);
                    return None;
                }
                next_at = self.skip_elapsed_loops(next_at, now)?;
                index = 0;
                continue;
            };
            self.current = frame;
            shown = next_at;
            next_at = next_at.checked_add(Duration::from_millis(u64::from(gap)))?;
            index = frame + 1;
        }
        self.playback.timing = Timing::Showing(shown);
        Some(next_at)
    }

    #[inline]
    fn timed_frame_at_or_after(&self, index: usize) -> Option<(usize, u32)> {
        let gap = self.frame(index)?.gap_ms();
        if gap != 0 {
            return Some((index, gap));
        }
        let frame = self.timed_extra.first_from(index.saturating_sub(1))?;
        Some((frame + 1, self.extra[frame].gap_ms()))
    }

    fn skip_elapsed_loops(&mut self, at: Instant, now: Instant) -> Option<Instant> {
        let elapsed = now.duration_since(at);
        let elapsed_ms = elapsed.as_millis();
        let loop_ms = u128::from(self.total_gap_ms);
        let skipped = elapsed_ms / loop_ms;
        if skipped == 0 {
            return Some(at);
        }
        let loops = u32::try_from(skipped).unwrap_or(u32::MAX);
        if let Some(limit) = self.playback.loop_limit
            && loops >= limit.get() - self.playback.completed_loops
        {
            let remaining = limit.get() - self.playback.completed_loops;
            let end =
                at.checked_add(Duration::from_millis(self.total_gap_ms).checked_mul(remaining)?)?;
            let last = self.timed_extra.last().map_or(0, |index| index + 1);
            let gap = self.frame(last).expect("A timed frame").gap_ms();
            self.current = last;
            self.playback.completed_loops = limit.get();
            self.playback.timing =
                Timing::Waiting(end.checked_sub(Duration::from_millis(u64::from(gap)))?);
            return None;
        }
        self.playback.completed_loops = self.playback.completed_loops.saturating_add(loops);
        let remainder = Duration::from_millis((elapsed_ms % loop_ms) as u64)
            + Duration::from_nanos(u64::from(elapsed.subsec_nanos() % 1_000_000));
        now.checked_sub(remainder)
    }
}
