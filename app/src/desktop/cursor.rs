use std::time::{Duration, Instant};

use lune_terminal::PixelPosition;

/// Angular frequency (rad/s) for the critically damped 2D cursor spring (~105 ms settle).
const SPRING_OMEGA: f32 = 56.0;
const LINE_FEED_DELAY: Duration = Duration::from_millis(8);
/// Grace window for preserving the last visible target across brief hide/show cycles
/// (such as DECTCEM cursor hiding during prompt redraws or blink-off frames).
const HIDE_GRACE: Duration = Duration::from_millis(250);
const SETTLE_POSITION_EPS: f32 = 0.25;
const SETTLE_VELOCITY_EPS: f32 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CursorTarget {
    /// Cursor is visible at this target pixel origin.
    Visible(PixelPosition),
    /// Cursor is temporarily hidden (e.g. blink-off or brief DECTCEM hide); preserve
    /// the last visible anchor within [`HIDE_GRACE`] so reappearance still glides smoothly.
    Hidden,
    /// Animation is disabled or context changed (unfocused, vi/search/hints/preedit/scrolled).
    Reset,
}

#[derive(Clone, Copy, Default)]
pub(super) struct CursorMotion {
    target: Option<PixelPosition>,
    hidden_since: Option<Instant>,
    spring: Option<SpringState>,
}

#[derive(Clone, Copy)]
struct SpringState {
    position: [f32; 2],
    velocity: [f32; 2],
    last_sample: Instant,
    waiting_for_line_feed: bool,
}

impl CursorMotion {
    pub(super) fn moving(self) -> bool {
        self.spring.is_some()
    }

    pub(super) fn advance(&mut self, input: CursorTarget, now: Instant) -> Option<[f32; 2]> {
        let target = match input {
            CursorTarget::Reset => {
                self.target = None;
                self.hidden_since = None;
                self.spring = None;
                return None;
            }
            CursorTarget::Hidden => {
                let since = *self.hidden_since.get_or_insert(now);
                if now.saturating_duration_since(since) > HIDE_GRACE {
                    self.target = None;
                    self.spring = None;
                } else if let Some(spring) = &mut self.spring
                    && let Some(prev_target) = self.target
                    && spring.advance_to(prev_target, now)
                {
                    self.spring = None;
                }
                return None;
            }
            CursorTarget::Visible(target) => {
                self.hidden_since = None;
                target
            }
        };

        let previous = self.target;
        self.target = Some(target);
        let Some(previous) = previous else {
            self.spring = None;
            return None;
        };

        if previous != target {
            match self.spring {
                Some(mut spring)
                    if spring.waiting_for_line_feed && target.x == 0 && previous.y != target.y =>
                {
                    spring.waiting_for_line_feed = false;
                    self.spring = Some(spring);
                }
                Some(mut spring) => {
                    // Advance to `now` against the previous target so position and velocity
                    // carry continuously into the new target without velocity stalls.
                    let _ = spring.advance_to(previous, now);
                    let waiting_for_line_feed =
                        previous.x > 0 && target.x == 0 && previous.y == target.y;
                    spring.waiting_for_line_feed = waiting_for_line_feed;
                    if waiting_for_line_feed {
                        spring.last_sample = now + LINE_FEED_DELAY;
                    }
                    self.spring = Some(spring);
                }
                None => {
                    let waiting_for_line_feed =
                        previous.x > 0 && target.x == 0 && previous.y == target.y;
                    self.spring = Some(SpringState {
                        position: to_float(previous),
                        velocity: [0.0, 0.0],
                        last_sample: if waiting_for_line_feed {
                            now + LINE_FEED_DELAY
                        } else {
                            now
                        },
                        waiting_for_line_feed,
                    });
                }
            }
        }

        let spring = self.spring.as_mut()?;
        if spring.advance_to(target, now) {
            self.spring = None;
            return None;
        }
        Some(spring.position)
    }
}

impl SpringState {
    /// Advances the analytical critically damped spring to `now`.
    /// Returns `true` when settled within subpixel tolerance of `target`.
    fn advance_to(&mut self, target: PixelPosition, now: Instant) -> bool {
        if now <= self.last_sample {
            return false;
        }
        self.waiting_for_line_feed = false;
        let dt = now
            .saturating_duration_since(self.last_sample)
            .as_secs_f32();
        self.last_sample = now;

        let [tx, ty] = to_float(target);
        let decay = (-SPRING_OMEGA * dt).exp();

        let dx = self.position[0] - tx;
        let vx = self.velocity[0];
        let cx = vx + SPRING_OMEGA * dx;
        let next_dx = (dx + cx * dt) * decay;
        let next_vx = (vx - SPRING_OMEGA * cx * dt) * decay;

        let dy = self.position[1] - ty;
        let vy = self.velocity[1];
        let cy = vy + SPRING_OMEGA * dy;
        let next_dy = (dy + cy * dt) * decay;
        let next_vy = (vy - SPRING_OMEGA * cy * dt) * decay;

        self.position = [tx + next_dx, ty + next_dy];
        self.velocity = [next_vx, next_vy];

        next_dx.abs() < SETTLE_POSITION_EPS
            && next_dy.abs() < SETTLE_POSITION_EPS
            && next_vx.abs() < SETTLE_VELOCITY_EPS
            && next_vy.abs() < SETTLE_VELOCITY_EPS
    }
}

fn to_float(position: PixelPosition) -> [f32; 2] {
    [position.x as f32, position.y as f32]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retargeting_preserves_position_and_velocity_and_settles_without_overshoot() {
        let start = Instant::now();
        let step = Duration::from_millis(24);
        let mut motion = CursorMotion::default();
        let point = |x| CursorTarget::Visible(PixelPosition { x, y: 20 });

        assert_eq!(motion.advance(point(20), start), None);
        let p0 = motion.advance(point(100), start).unwrap();
        assert!((p0[0] - 20.0).abs() < 0.01);

        let p1 = motion.advance(point(100), start + step).unwrap();
        assert!(p1[0] > 40.0 && p1[0] < 100.0);

        // Retargeting mid-flight preserves current position continuously.
        let p2 = motion.advance(point(180), start + step).unwrap();
        assert!((p2[0] - p1[0]).abs() < 0.01);

        // Spring advances monotonically toward 180 and settles cleanly.
        let p3 = motion.advance(point(180), start + step * 2).unwrap();
        assert!(p3[0] > p2[0] && p3[0] <= 180.0);
        assert_eq!(
            motion.advance(point(180), start + Duration::from_millis(350)),
            None
        );
        assert!(!motion.moving());
    }

    #[test]
    fn line_feed_before_the_next_sample_completes_a_single_diagonal() {
        let mut motion = CursorMotion::default();
        let start = Instant::now();
        let from = CursorTarget::Visible(PixelPosition { x: 80, y: 40 });
        let carriage_return = CursorTarget::Visible(PixelPosition { x: 0, y: 40 });
        let next_line = CursorTarget::Visible(PixelPosition { x: 0, y: 80 });
        motion.advance(from, start);
        let hold = motion.advance(carriage_return, start).unwrap();
        assert!((hold[0] - 80.0).abs() < 0.01 && (hold[1] - 40.0).abs() < 0.01);

        let mid = motion
            .advance(
                next_line,
                start + LINE_FEED_DELAY + Duration::from_millis(25),
            )
            .unwrap();
        assert!(mid[0] > 0.0 && mid[0] < 80.0);
        assert!(mid[1] > 40.0 && mid[1] < 80.0);
        assert_eq!(
            motion.advance(
                next_line,
                start + LINE_FEED_DELAY + Duration::from_millis(350)
            ),
            None
        );
    }

    #[test]
    fn brief_hide_preserves_anchor_for_smooth_reappearance() {
        let mut motion = CursorMotion::default();
        let start = Instant::now();
        let a = CursorTarget::Visible(PixelPosition { x: 20, y: 20 });
        let b = CursorTarget::Visible(PixelPosition { x: 100, y: 20 });
        assert_eq!(motion.advance(a, start), None);
        assert_eq!(
            motion.advance(CursorTarget::Hidden, start + Duration::from_millis(16)),
            None
        );
        let p = motion
            .advance(b, start + Duration::from_millis(32))
            .expect("brief hide should still animate to new position");
        assert!((p[0] - 20.0).abs() < 0.01);
    }
}
