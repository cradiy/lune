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
            .gap_ms;
        let gap = Duration::from_millis(u64::from(gap));
        let (mut shown, mut next_at) = match timing {
            Timing::Showing(shown) => (shown, shown.checked_add(gap)?),
            Timing::Start => (now, now.checked_add(gap)?),
            Timing::Waiting(shown) => {
                // Keep the current frame's remaining gap and start new frames at this wake.
                let deadline = shown.checked_add(gap)?;
                if deadline > now {
                    return Some(deadline);
                }
                (shown, now)
            }
        };
        let mut index = self.current + 1;
        while next_at <= now {
            if index > self.extra.len() {
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
            }

            let gap = self.frame(index).expect("Animation frame").gap_ms;
            if gap != 0 {
                self.current = index;
                shown = next_at;
                next_at = next_at.checked_add(Duration::from_millis(u64::from(gap)))?;
            }
            index += 1;
        }
        self.playback.timing = Timing::Showing(shown);
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
        if let Some(limit) = self.playback.loop_limit
            && loops >= limit.get() - self.playback.completed_loops
        {
            let remaining = limit.get() - self.playback.completed_loops;
            let end =
                at.checked_add(Duration::from_millis(self.total_gap_ms).checked_mul(remaining)?)?;
            let (last, gap) = self
                .extra
                .iter()
                .enumerate()
                .rfind(|(_, frame)| frame.gap_ms != 0)
                .map_or((0, self.root.gap_ms), |(index, frame)| {
                    (index + 1, frame.gap_ms)
                });
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
