use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use super::Animation;

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
    Waiting,
}

#[derive(Debug, Default)]
pub(super) struct Playback {
    mode: PlaybackMode,
    timing: Timing,
    loop_limit: Option<NonZeroU32>,
    completed_loops: u32,
}

impl Playback {
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
    pub(crate) fn playing(&self) -> bool {
        self.playback.mode != PlaybackMode::Stopped
    }

    pub(crate) fn advance(&mut self, now: Instant) -> (Option<Instant>, bool) {
        let previous = self.current;
        let next = self.advance_to(now);
        (next, self.current != previous)
    }

    fn advance_to(&mut self, now: Instant) -> Option<Instant> {
        if !self.playing() || self.playback.loops_finished() {
            return None;
        }
        let shown = match self.playback.timing {
            Timing::Start => {
                self.playback.timing = Timing::Showing(now);
                Some(now)
            }
            Timing::Showing(shown) => Some(shown),
            Timing::Waiting => None,
        };
        if self.extra.is_empty() {
            return None;
        }
        if self.total_gap_ms == 0 {
            self.playback.timing = Timing::Waiting;
            return None;
        }

        let gap = self
            .frame(self.current)
            .expect("Current animation frame")
            .gap_ms;
        let mut next_at = match shown {
            Some(shown) => shown.checked_add(Duration::from_millis(u64::from(gap)))?,
            None => now,
        };
        let mut index = self.current + 1;
        while next_at <= now {
            if index > self.extra.len() {
                if self.playback.mode == PlaybackMode::Loading {
                    self.playback.timing = Timing::Waiting;
                    return None;
                }
                self.playback.completed_loops = self.playback.completed_loops.saturating_add(1);
                if self.playback.loops_finished() {
                    self.playback.timing = Timing::Waiting;
                    return None;
                }
                next_at = self.skip_elapsed_loops(next_at, now)?;
                index = 0;
            }

            let gap = self.frame(index).expect("Animation frame").gap_ms;
            if gap != 0 {
                self.current = index;
                self.playback.timing = Timing::Showing(next_at);
                next_at = next_at.checked_add(Duration::from_millis(u64::from(gap)))?;
            }
            index += 1;
        }
        Some(next_at)
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
        self.playback.completed_loops = self.playback.completed_loops.saturating_add(loops);
        if self.playback.loops_finished() {
            self.current = self
                .extra
                .iter()
                .rposition(|frame| frame.gap_ms != 0)
                .map_or(0, |index| index + 1);
            self.playback.completed_loops =
                self.playback.loop_limit.expect("Finite animation").get();
            self.playback.timing = Timing::Waiting;
            return None;
        }
        let remainder = Duration::from_millis((elapsed_ms % loop_ms) as u64)
            + Duration::from_nanos(u64::from(elapsed.subsec_nanos() % 1_000_000));
        now.checked_sub(remainder)
    }
}
