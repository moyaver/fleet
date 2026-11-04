// SPDX-License-Identifier: AGPL-3.0-only
//! The visible time window: zoom around the cursor and pan, always inside the data's bounds.

use crate::charts::scale::{f64_ms, ms_f64};

/// The visible window `[start, end]` (unix ms) inside the `bounds` of all the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    bounds: (i64, i64),
    window: (i64, i64),
    min_span_ms: i64,
}

impl Viewport {
    /// A viewport showing all of `bounds`, never zoomed in past `min_span_ms`.
    #[must_use]
    pub fn new(bounds: (i64, i64), min_span_ms: i64) -> Self {
        let bounds = (bounds.0.min(bounds.1), bounds.0.max(bounds.1));
        Self {
            bounds,
            window: bounds,
            min_span_ms: min_span_ms.max(1),
        }
    }

    /// The visible window, `(start, end)` in unix ms.
    #[must_use]
    pub fn window(&self) -> (i64, i64) {
        self.window
    }

    /// Zooms in by `factor` (above 1) or out (below 1), keeping the time `anchor_ms` at the same
    /// place on screen.
    pub fn zoom(&mut self, factor: f64, anchor_ms: i64) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let (start, end) = self.window;
        let span = end - start;
        let new_span = self.clamp_span(f64_ms(ms_f64(span) / factor));
        let along = if span == 0 {
            0.5
        } else {
            (ms_f64(anchor_ms - start) / ms_f64(span)).clamp(0.0, 1.0)
        };
        self.place(anchor_ms - f64_ms(along * ms_f64(new_span)), new_span);
    }

    /// Moves the window by `delta_ms`, stopping at the bounds.
    pub fn pan(&mut self, delta_ms: i64) {
        let (start, end) = self.window;
        self.place(start.saturating_add(delta_ms), end - start);
    }

    /// Pans by a pointer drag of `dx_px` across a plot `plot_width_px` wide: dragging right shows
    /// earlier times.
    pub fn pan_px(&mut self, dx_px: f64, plot_width_px: f64) {
        if plot_width_px <= 0.0 || !dx_px.is_finite() {
            return;
        }
        let span = self.window.1 - self.window.0;
        self.pan(f64_ms(-dx_px / plot_width_px * ms_f64(span)));
    }

    /// Pans the least needed to show `[start_ms, end_ms]`; a span wider than the window shows its
    /// start.
    pub fn reveal(&mut self, start_ms: i64, end_ms: i64) {
        let (start, end) = self.window;
        let span = end - start;
        if end_ms - start_ms >= span || start_ms < start {
            self.place(start_ms, span);
        } else if end_ms > end {
            self.place(end_ms - span, span);
        }
    }

    /// Replaces the bounds (the data grew) and keeps the window inside them.
    pub fn set_bounds(&mut self, bounds: (i64, i64)) {
        let follows = self.window == self.bounds;
        self.bounds = (bounds.0.min(bounds.1), bounds.0.max(bounds.1));
        if follows {
            self.window = self.bounds;
            return;
        }
        let (start, end) = self.window;
        self.place(start, end - start);
    }

    fn clamp_span(&self, span: i64) -> i64 {
        let full = self.bounds.1 - self.bounds.0;
        span.clamp(self.min_span_ms.min(full), full)
    }

    /// Puts a window of `span` at `start`, shifted to fit inside the bounds.
    fn place(&mut self, start: i64, span: i64) {
        let span = self.clamp_span(span);
        let start = start.clamp(self.bounds.0, self.bounds.1 - span);
        self.window = (start, start + span);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn vp() -> Viewport {
        Viewport::new((0, 1000), 10)
    }

    #[test]
    fn zooming_in_keeps_the_time_under_the_cursor_in_place() {
        let mut v = vp();
        v.zoom(2.0, 250); // 25 % along: stays 25 % along a window half as wide
        assert_eq!(v.window(), (125, 625));
        let mut left = vp();
        left.zoom(2.0, 0);
        assert_eq!(left.window(), (0, 500));
        let mut right = vp();
        right.zoom(2.0, 1000);
        assert_eq!(right.window(), (500, 1000));
    }

    #[test]
    fn zoom_is_limited_by_the_minimum_span_and_the_bounds() {
        let mut v = vp();
        v.zoom(1.0e9, 500);
        assert_eq!(v.window(), (495, 505));
        v.zoom(1.0e-9, 500);
        assert_eq!(v.window(), (0, 1000));
    }

    #[test]
    fn panning_stops_at_the_bounds() {
        let mut v = vp();
        v.zoom(2.0, 250);
        v.pan(300);
        assert_eq!(v.window(), (425, 925));
        v.pan(10_000);
        assert_eq!(v.window(), (500, 1000));
        v.pan(-10_000);
        assert_eq!(v.window(), (0, 500));
    }

    #[test]
    fn dragging_right_shows_earlier_times() {
        let mut v = vp();
        v.zoom(2.0, 0); // (0, 500)
        v.pan_px(-100.0, 500.0); // drag left by a fifth of the plot: 100 ms later
        assert_eq!(v.window(), (100, 600));
        v.pan_px(50.0, 500.0);
        assert_eq!(v.window(), (50, 550));
    }

    #[test]
    fn reveal_moves_the_least_needed() {
        let mut v = vp();
        v.zoom(4.0, 0); // (0, 250)
        v.reveal(300, 400);
        assert_eq!(v.window(), (150, 400));
        v.reveal(10, 20);
        assert_eq!(v.window(), (10, 260));
        v.reveal(100, 150); // already visible
        assert_eq!(v.window(), (10, 260));
        v.reveal(500, 900); // wider than the window: its start
        assert_eq!(v.window(), (500, 750));
    }

    #[test]
    fn growing_bounds_keep_the_window_and_shrinking_ones_clamp_it() {
        let mut v = vp();
        v.zoom(2.0, 0);
        v.set_bounds((0, 2000));
        assert_eq!(v.window(), (0, 500));
        v.pan(1500);
        v.set_bounds((0, 1000));
        assert_eq!(v.window(), (500, 1000));
    }

    #[test]
    fn an_unzoomed_viewport_follows_growing_bounds() {
        // A live run: nobody zoomed, so the window keeps showing everything.
        let mut v = vp();
        v.set_bounds((0, 2000));
        assert_eq!(v.window(), (0, 2000));
        v.set_bounds((-500, 2500));
        assert_eq!(v.window(), (-500, 2500));
        // Once zoomed, growth no longer moves it.
        v.zoom(2.0, 0);
        let zoomed = v.window();
        v.set_bounds((-500, 3000));
        assert_eq!(v.window(), zoomed);
    }

    proptest! {
        #[test]
        fn the_window_always_stays_inside_the_bounds(
            ops in prop::collection::vec((0_u8..3, -5.0_f64..5.0, -2000_i64..3000), 0..30),
        ) {
            let mut v = vp();
            for (op, f, t) in ops {
                match op {
                    0 => v.zoom(f.exp(), t),
                    1 => v.pan(t),
                    _ => v.pan_px(f * 100.0, 400.0),
                }
                let (s, e) = v.window();
                prop_assert!(s >= 0 && e <= 1000);
                prop_assert!(e - s >= 10 && e - s <= 1000);
            }
        }
    }
}
