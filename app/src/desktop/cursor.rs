use std::time::{Duration, Instant};

use lune_terminal::PixelPosition;

const MOTION_DURATION: Duration = Duration::from_millis(80);
const LINE_FEED_DELAY: Duration = Duration::from_millis(8);

#[derive(Clone, Copy, Default)]
pub(super) struct CursorMotion {
    target: Option<PixelPosition>,
    transition: Option<Transition>,
}

#[derive(Clone, Copy)]
struct Transition {
    origin: [f32; 2],
    start: Instant,
    waiting_for_line_feed: bool,
}

impl CursorMotion {
    pub(super) fn moving(self) -> bool {
        self.transition.is_some()
    }

    pub(super) fn advance(
        &mut self,
        target: Option<PixelPosition>,
        now: Instant,
    ) -> Option<PixelPosition> {
        let previous = self.target;
        self.target = target;
        let Some((previous, target)) = previous.zip(target) else {
            self.transition = None;
            return None;
        };
        if previous != target {
            match self.transition {
                Some(transition)
                    if transition.waiting_for_line_feed
                        && target.x == 0
                        && previous.y != target.y =>
                {
                    self.transition = Some(Transition {
                        waiting_for_line_feed: false,
                        ..transition
                    });
                }
                old => {
                    let origin = old.map_or_else(
                        || to_float(previous),
                        |transition| transition.sample(previous, now),
                    );
                    let waiting_for_line_feed =
                        previous.x > 0 && target.x == 0 && previous.y == target.y;
                    self.transition = Some(Transition {
                        origin,
                        start: if waiting_for_line_feed {
                            now + LINE_FEED_DELAY
                        } else {
                            now
                        },
                        waiting_for_line_feed,
                    });
                }
            }
        }

        let transition = self.transition.as_mut()?;
        if now >= transition.start + MOTION_DURATION {
            self.transition = None;
            return None;
        }
        if now > transition.start {
            transition.waiting_for_line_feed = false;
        }
        let [x, y] = transition.sample(target, now);
        Some(PixelPosition {
            x: x.round() as u32,
            y: y.round() as u32,
        })
    }
}

impl Transition {
    fn sample(self, target: PixelPosition, now: Instant) -> [f32; 2] {
        let progress =
            now.saturating_duration_since(self.start).as_secs_f32() / MOTION_DURATION.as_secs_f32();
        let remaining = (1.0 - progress).max(0.0);
        let eased = 1.0 - remaining * remaining * remaining;
        let [x, y] = to_float(target);
        [
            self.origin[0] + (x - self.origin[0]) * eased,
            self.origin[1] + (y - self.origin[1]) * eased,
        ]
    }
}

fn to_float(position: PixelPosition) -> [f32; 2] {
    [position.x as f32, position.y as f32]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retargeting_preserves_the_sampled_position_and_stops_after_arrival() {
        let start = Instant::now();
        let half = start + MOTION_DURATION / 2;
        let mut motion = CursorMotion::default();
        let point = |x| Some(PixelPosition { x, y: 20 });
        assert_eq!(motion.advance(point(20), start), None);
        assert_eq!(motion.advance(point(100), start), point(20));
        assert_eq!(motion.advance(point(100), half), point(90));
        assert_eq!(motion.advance(point(180), half), point(90));
        assert_eq!(motion.advance(point(180), half + MOTION_DURATION), None);
        assert!(!motion.moving());
        assert_eq!(motion.advance(None, half + MOTION_DURATION), None);
    }

    #[test]
    fn line_feed_before_the_next_sample_completes_a_single_diagonal() {
        let mut motion = CursorMotion::default();
        let start = Instant::now();
        let from = Some(PixelPosition { x: 80, y: 40 });
        let carriage_return = Some(PixelPosition { x: 0, y: 40 });
        let next_line = Some(PixelPosition { x: 0, y: 80 });
        motion.advance(from, start);
        assert_eq!(motion.advance(carriage_return, start), from);
        assert_eq!(
            motion.advance(next_line, start + LINE_FEED_DELAY + MOTION_DURATION / 2),
            Some(PixelPosition { x: 10, y: 75 })
        );
        assert_eq!(
            motion.advance(next_line, start + LINE_FEED_DELAY + MOTION_DURATION),
            None
        );
    }
}
