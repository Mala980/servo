/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Chromium-style animated ("smooth") scrolling for mouse wheel input.
//!
//! Discrete wheel notches (`LineDelta` events) are accumulated and then
//! delivered to the web content as a series of small pixel deltas spread out
//! over time, which produces the smooth scroll animation users know from
//! Chromium. High-resolution input like touchpad `PixelDelta` events is
//! forwarded unchanged, since it is already smooth.

use std::time::{Duration, Instant};

/// The time constant of the exponential ease-out, i.e. the time after which
/// the remaining distance shrinks to about 37%. Chromium uses a similar
/// value for its wheel scroll animation.
const TIME_CONSTANT: Duration = Duration::from_millis(55);

/// The maximum time between two ticks that will be animated at once. If the
/// compositor stalls for longer than this (or the user stops scrolling and
/// comes back much later), the animation simply jumps instead of playing
/// back the stall.
const MAX_TICK_DURATION: Duration = Duration::from_millis(100);

/// Once the remaining distance falls below this threshold (in pixels), the
/// animation is finished and the remainder is delivered in one step.
const DONE_THRESHOLD: f64 = 0.5;

/// Animates discrete mouse wheel scrolling.
#[derive(Default)]
pub(crate) struct SmoothScrollAnimator {
    /// The distance left to scroll, in pixels, as `(x, y)` wheel deltas.
    remaining: (f64, f64),
    /// The time of the last animation tick, if one is in flight.
    last_tick: Option<Instant>,
}

impl SmoothScrollAnimator {
    /// Adds a discrete wheel delta to the pending animation. Deltas use the
    /// same axes and signs as the `WheelDelta` delivered to web content.
    pub(crate) fn push(&mut self, x: f64, y: f64) {
        self.remaining = (self.remaining.0 + x, self.remaining.1 + y);
        // A tick timestamp of "now" makes the first tick deliver a natural
        // first step instead of treating the whole queue as overdue.
        if self.last_tick.is_none() {
            self.last_tick = Some(Instant::now());
        }
    }

    /// Whether an animation is currently in flight.
    pub(crate) fn is_active(&self) -> bool {
        (self.remaining.0.abs() > DONE_THRESHOLD || self.remaining.1.abs() > DONE_THRESHOLD) &&
            self.last_tick.is_some()
    }

    /// Advances the animation by one frame and returns the next wheel delta
    /// chunk to deliver to web content, or `None` when the animation is done.
    pub(crate) fn tick(&mut self, now: Instant) -> Option<(f64, f64)> {
        if !self.is_active() {
            self.last_tick = None;
            return None;
        }

        let elapsed = now
            .duration_since(self.last_tick.unwrap_or(now))
            .min(MAX_TICK_DURATION);
        self.last_tick = Some(now);

        // Exponential ease-out: each frame delivers a fraction of the
        // remaining distance based on the elapsed time.
        let fraction = 1.0 - (-elapsed.as_secs_f64() / TIME_CONSTANT.as_secs_f64()).exp();

        let (remaining_x, remaining_y) = self.remaining;
        let (delta_x, delta_y) = (remaining_x * fraction, remaining_y * fraction);
        self.remaining = (remaining_x - delta_x, remaining_y - delta_y);

        // When the remaining distance becomes negligible, deliver it in full
        // and finish the animation.
        if self.remaining.0.abs() <= DONE_THRESHOLD && self.remaining.1.abs() <= DONE_THRESHOLD {
            let final_delta = (delta_x + self.remaining.0, delta_y + self.remaining.1);
            self.remaining = (0.0, 0.0);
            self.last_tick = None;
            return Some(final_delta);
        }

        Some((delta_x, delta_y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::LINE_HEIGHT;

    fn total_delta(
        animator: &mut SmoothScrollAnimator,
        start: Instant,
        ticks: usize,
    ) -> (f64, f64) {
        let mut total = (0.0, 0.0);
        let mut time = start;
        for _ in 0..ticks {
            time += Duration::from_millis(16);
            if let Some((x, y)) = animator.tick(time) {
                total = (total.0 + x, total.1 + y);
            }
        }
        total
    }

    #[test]
    fn test_animation_is_inactive_by_default() {
        let mut animator = SmoothScrollAnimator::default();
        assert!(!animator.is_active());
        assert_eq!(animator.tick(Instant::now()), None);
    }

    #[test]
    fn test_animation_delivers_full_distance() {
        let start = Instant::now();
        let mut animator = SmoothScrollAnimator::default();
        let distance = 3.0 * f64::from(LINE_HEIGHT);
        animator.push(0.0, distance);

        let (total_x, total_y) = total_delta(&mut animator, start, 100);
        assert!(total_x.abs() < 1.0);
        // The delivered distance must match the requested distance within
        // the done threshold, since the last step delivers the remainder.
        assert!(
            (total_y - distance).abs() <= DONE_THRESHOLD * 2.0,
            "total: {total_y}, expected: {distance}"
        );
    }

    #[test]
    fn test_animation_finishes() {
        let start = Instant::now();
        let mut animator = SmoothScrollAnimator::default();
        animator.push(0.0, f64::from(LINE_HEIGHT));
        let _ = total_delta(&mut animator, start, 100);
        assert!(!animator.is_active(), "animation should finish");
        assert_eq!(animator.tick(start + Duration::from_secs(10)), None);
    }

    #[test]
    fn test_first_tick_moves_a_sensible_amount() {
        let start = Instant::now();
        let mut animator = SmoothScrollAnimator::default();
        animator.push(0.0, f64::from(LINE_HEIGHT));
        // A 16ms first frame should move roughly a quarter of the distance
        // for a 55ms time constant: 1 - exp(-16/55) is about 0.25.
        let (x, y) = animator.tick(start + Duration::from_millis(16)).unwrap();
        assert_eq!(x, 0.0);
        assert!(
            y > f64::from(LINE_HEIGHT) * 0.15 && y < f64::from(LINE_HEIGHT) * 0.4,
            "unexpected first tick distance: {y}"
        );
    }

    #[test]
    fn test_stall_does_not_dump_huge_delta() {
        let start = Instant::now();
        let mut animator = SmoothScrollAnimator::default();
        animator.push(0.0, f64::from(LINE_HEIGHT));
        // Simulate a long stall (compositor frozen for 5 seconds); the tick
        // must be clamped to `MAX_TICK_DURATION`.
        let (x, y) = animator.tick(start + Duration::from_secs(5)).unwrap();
        assert_eq!(x, 0.0);
        let max_fraction =
            1.0 - (-MAX_TICK_DURATION.as_secs_f64() / TIME_CONSTANT.as_secs_f64()).exp();
        assert!(
            y <= f64::from(LINE_HEIGHT) * max_fraction + 0.001,
            "unexpected stall tick distance: {y}"
        );
    }

    #[test]
    fn test_new_push_continues_animation() {
        let start = Instant::now();
        let mut animator = SmoothScrollAnimator::default();
        animator.push(0.0, f64::from(LINE_HEIGHT));
        let _ = animator.tick(start + Duration::from_millis(16)).unwrap();
        // Pushing more distance mid-animation extends it instead of
        // restarting abruptly.
        animator.push(0.0, f64::from(LINE_HEIGHT));
        assert!(animator.is_active());
        let (total_x, total_y) = total_delta(&mut animator, start + Duration::from_millis(32), 100);
        assert_eq!(total_x, 0.0);
        assert!(
            (total_y - 2.0 * f64::from(LINE_HEIGHT)).abs() <= DONE_THRESHOLD * 2.0
        );
    }
}
