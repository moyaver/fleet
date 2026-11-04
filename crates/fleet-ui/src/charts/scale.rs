// SPDX-License-Identifier: AGPL-3.0-only
//! Linear and time scales, "nice" tick selection and time-axis granularity.

use crate::charts::format::format_timestamp;

/// A linear map from a data domain to a pixel range. A degenerate domain maps to the middle of
/// the range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearScale {
    domain: (f64, f64),
    range: (f64, f64),
}

impl LinearScale {
    /// A scale from `domain` to `range`.
    #[must_use]
    pub fn new(domain: (f64, f64), range: (f64, f64)) -> Self {
        Self { domain, range }
    }

    /// The pixel for a data value (not clamped).
    #[must_use]
    pub fn map(&self, value: f64) -> f64 {
        let (d0, d1) = self.domain;
        let (r0, r1) = self.range;
        if (d1 - d0).abs() < f64::EPSILON {
            return f64::midpoint(r0, r1);
        }
        r0 + (value - d0) / (d1 - d0) * (r1 - r0)
    }

    /// The data value under a pixel (not clamped).
    #[must_use]
    pub fn invert(&self, px: f64) -> f64 {
        let (d0, d1) = self.domain;
        let (r0, r1) = self.range;
        if (r1 - r0).abs() < f64::EPSILON {
            return d0;
        }
        d0 + (px - r0) / (r1 - r0) * (d1 - d0)
    }
}

/// A linear map from unix milliseconds to pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeScale {
    domain: (i64, i64),
    range: (f64, f64),
}

impl TimeScale {
    /// A scale from the time `domain` (unix ms) to the pixel `range`.
    #[must_use]
    pub fn new(domain: (i64, i64), range: (f64, f64)) -> Self {
        Self { domain, range }
    }

    /// The visible time range.
    #[must_use]
    pub fn domain(&self) -> (i64, i64) {
        self.domain
    }

    /// The pixel for a time (not clamped).
    #[must_use]
    pub fn map(&self, at_ms: i64) -> f64 {
        let (d0, d1) = self.domain;
        LinearScale::new((ms_f64(d0), ms_f64(d1)), self.range).map(ms_f64(at_ms))
    }

    /// The time under a pixel, rounded to the millisecond (not clamped).
    #[must_use]
    pub fn invert(&self, px: f64) -> i64 {
        let (d0, d1) = self.domain;
        f64_ms(LinearScale::new((ms_f64(d0), ms_f64(d1)), self.range).invert(px))
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "unix milliseconds stay far below 2^53 until the year 287396"
)]
pub(crate) fn ms_f64(ms: i64) -> f64 {
    ms as f64
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "a rounded pixel-to-time result is clamped into i64 first"
)]
pub(crate) fn f64_ms(ms: f64) -> i64 {
    ms.round().clamp(-9.0e15, 9.0e15) as i64
}

/// Round tick values covering a data range.
#[derive(Debug, Clone, PartialEq)]
pub struct NiceTicks {
    /// Ascending, equally spaced tick values.
    pub ticks: Vec<f64>,
    /// The first tick: a lower bound of the data.
    pub min: f64,
    /// The last tick: an upper bound of the data.
    pub max: f64,
}

/// Round ticks (1, 2 or 5 × 10ⁿ apart) covering `min..=max`, aiming for about `target` ticks.
#[must_use]
pub fn nice_ticks(min: f64, max: f64, target: usize) -> NiceTicks {
    let (mut min, mut max) = if min.is_finite() && max.is_finite() {
        (min.min(max), min.max(max))
    } else {
        (0.0, 1.0)
    };
    if (max - min).abs() < f64::EPSILON {
        if min == 0.0 {
            max = 1.0;
        } else {
            let pad = min.abs() * 0.1;
            (min, max) = (min - pad, max + pad);
        }
    }
    let target = f64::from(u32::try_from(target.max(2)).unwrap_or(u32::MAX));
    let range = nice_number(max - min, false);
    let step = nice_number(range / (target - 1.0), true);
    let first = (min / step + 1e-9).floor();
    let last = (max / step - 1e-9).ceil();
    let mut ticks = Vec::new();
    let mut k = first;
    while k <= last && ticks.len() < 1000 {
        ticks.push(k * step);
        k += 1.0;
    }
    NiceTicks {
        ticks,
        min: first * step,
        max: last * step,
    }
}

/// Heckbert's "nice number": 1, 2, 5 or 10 times a power of ten near `x`.
fn nice_number(x: f64, round: bool) -> f64 {
    let exponent = x.log10().floor();
    let base = 10_f64.powf(exponent);
    let fraction = x / base;
    let nice = match (round, fraction) {
        (true, f) if f < 1.5 => 1.0,
        (true, f) if f < 3.0 => 2.0,
        (true, f) if f < 7.0 => 5.0,
        (false, f) if f <= 1.0 => 1.0,
        (false, f) if f <= 2.0 => 2.0,
        (false, f) if f <= 5.0 => 5.0,
        _ => 10.0,
    };
    nice * base
}

/// One mark on a time axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeTick {
    /// Unix time, milliseconds.
    pub at_ms: i64,
    /// The text under the mark.
    pub label: String,
}

/// Ticks on round boundaries (seconds up to days, in the viewer's UTC offset) with at most about
/// `target` intervals between `from_ms` and `to_ms`. There is always at least one tick.
#[must_use]
pub fn time_ticks(
    from_ms: i64,
    to_ms: i64,
    target: usize,
    utc_offset_minutes: i32,
) -> Vec<TimeTick> {
    let offset = i64::from(utc_offset_minutes) * 60_000;
    let span = to_ms.saturating_sub(from_ms).max(0);
    let target = i64::try_from(target.max(1)).unwrap_or(i64::MAX);
    let step = TIME_STEPS_MS
        .iter()
        .copied()
        .find(|&step| span / step <= target)
        .unwrap_or(2_592_000_000);
    let first = (from_ms + offset).div_euclid(step) * step - offset;
    let first = if first < from_ms { first + step } else { first };
    let mut ticks = Vec::new();
    let mut at = if span == 0 { from_ms } else { first };
    while at <= to_ms.max(from_ms) && ticks.len() < 1000 {
        ticks.push(TimeTick {
            at_ms: at,
            label: tick_label(at, step, utc_offset_minutes),
        });
        at += step;
    }
    if ticks.is_empty() {
        // No round boundary inside a short span at a coarse step: still label the start.
        ticks.push(TimeTick {
            at_ms: from_ms,
            label: tick_label(from_ms, step, utc_offset_minutes),
        });
    }
    ticks
}

/// Tick spacings: 1, 2, 5, 10, 15, 30 seconds; 1, 2, 5, 10, 15, 30 minutes; 1, 2, 3, 6, 12
/// hours; 1, 2, 7, 30 days.
const TIME_STEPS_MS: [i64; 21] = [
    1_000,
    2_000,
    5_000,
    10_000,
    15_000,
    30_000,
    60_000,
    120_000,
    300_000,
    600_000,
    900_000,
    1_800_000,
    3_600_000,
    7_200_000,
    10_800_000,
    21_600_000,
    43_200_000,
    86_400_000,
    172_800_000,
    604_800_000,
    2_592_000_000,
];

/// `HH:MM:SS` below a minute step, `HH:MM` below a day, `MM-DD` from a day up.
fn tick_label(at_ms: i64, step_ms: i64, utc_offset_minutes: i32) -> String {
    let full = format_timestamp(at_ms, utc_offset_minutes);
    let (date, clock) = full.split_once(' ').unwrap_or((&full, ""));
    if step_ms >= 86_400_000 {
        return date.split_once('-').map_or(date, |(_, md)| md).to_owned();
    }
    if step_ms >= 60_000 {
        return clock
            .rsplit_once(':')
            .map_or(clock, |(hm, _)| hm)
            .to_owned();
    }
    clock.to_owned()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn ticks_always_cover_the_data_and_are_evenly_spaced(
            a in -1.0e9_f64..1.0e9,
            b in -1.0e9_f64..1.0e9,
            target in 2_usize..12,
        ) {
            let t = nice_ticks(a, b, target);
            let (lo, hi) = (a.min(b), a.max(b));
            prop_assert!(t.min <= lo + 1e-6 && t.max >= hi - 1e-6);
            prop_assert!(t.ticks.len() >= 2 && t.ticks.len() <= 2 * target + 2);
            let step = t.ticks[1] - t.ticks[0];
            prop_assert!(t.ticks.windows(2).all(|w| ((w[1] - w[0]) - step).abs() <= step.abs() * 1e-6));
        }

        #[test]
        fn time_ticks_stay_inside_the_span_and_ascend(
            from in 0_i64..4_000_000_000_000,
            span in 1_i64..4_000_000_000,
            target in 1_usize..12,
            offset in -720_i32..840,
        ) {
            let ticks = time_ticks(from, from + span, target, offset);
            prop_assert!(!ticks.is_empty());
            prop_assert!(ticks.iter().all(|t| t.at_ms >= from && t.at_ms <= from + span));
            prop_assert!(ticks.windows(2).all(|w| w[0].at_ms < w[1].at_ms));
        }
    }

    fn approx(a: &[f64], b: &[f64]) {
        assert_eq!(a.len(), b.len(), "{a:?} vs {b:?}");
        for (x, y) in a.iter().zip(b) {
            assert!((x - y).abs() < 1e-9, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn linear_scale_maps_both_ways() {
        let s = LinearScale::new((0.0, 100.0), (200.0, 0.0)); // y grows upwards
        assert!((s.map(0.0) - 200.0).abs() < 1e-9);
        assert!((s.map(25.0) - 150.0).abs() < 1e-9);
        assert!((s.invert(50.0) - 75.0).abs() < 1e-9);
        let flat = LinearScale::new((5.0, 5.0), (0.0, 10.0));
        assert!((flat.map(5.0) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn time_scale_maps_milliseconds_to_pixels_and_back() {
        let s = TimeScale::new((1_000, 41_000), (50.0, 590.0));
        assert!((s.map(21_000) - 320.0).abs() < 1e-9);
        assert_eq!(s.invert(320.0), 21_000);
        assert_eq!(s.domain(), (1_000, 41_000));
    }

    #[test]
    fn ticks_land_on_round_numbers_that_cover_the_data() {
        let t = nice_ticks(0.0, 97.0, 5);
        approx(&t.ticks, &[0.0, 20.0, 40.0, 60.0, 80.0, 100.0]);
        assert!((t.min - 0.0).abs() < 1e-9 && (t.max - 100.0).abs() < 1e-9);
        approx(
            &nice_ticks(-3.0, 38.0, 5).ticks,
            &[-10.0, 0.0, 10.0, 20.0, 30.0, 40.0],
        );
        approx(
            &nice_ticks(0.0, 1.0, 5).ticks,
            &[0.0, 0.2, 0.4, 0.6, 0.8, 1.0],
        );
    }

    #[test]
    fn a_flat_series_still_gets_a_usable_axis() {
        approx(
            &nice_ticks(0.0, 0.0, 5).ticks,
            &[0.0, 0.2, 0.4, 0.6, 0.8, 1.0],
        );
        let t = nice_ticks(50.0, 50.0, 5);
        assert!(t.min < 50.0 && t.max > 50.0);
    }

    #[test]
    fn time_ticks_pick_round_boundaries() {
        // 22:13:20 UTC for one minute: ten-second steps.
        let from = 1_700_000_000_000;
        let labels = |t: Vec<TimeTick>| t.into_iter().map(|t| t.label).collect::<Vec<_>>();
        assert_eq!(
            labels(time_ticks(from, from + 60_000, 6, 0)),
            [
                "22:13:20", "22:13:30", "22:13:40", "22:13:50", "22:14:00", "22:14:10", "22:14:20"
            ]
        );
        // Ten minutes: two-minute steps, minutes only.
        assert_eq!(
            labels(time_ticks(from, from + 600_000, 6, 0)),
            ["22:14", "22:16", "22:18", "22:20", "22:22"]
        );
        // Ten days: two-day steps, dates only.
        assert_eq!(
            labels(time_ticks(from, from + 864_000_000, 5, 0)),
            ["11-15", "11-17", "11-19", "11-21", "11-23"]
        );
    }

    #[test]
    fn time_ticks_follow_the_viewers_offset() {
        // +30 minutes: 22:43:20 local; the ticks are on local five-minute marks.
        let from = 1_700_000_000_000;
        let t = time_ticks(from, from + 1_200_000, 4, 30);
        assert_eq!(t.first().map(|t| t.label.as_str()), Some("22:45"));
        assert_eq!(t.first().map(|t| t.at_ms), Some(from + 100_000));
    }

    #[test]
    fn an_empty_span_has_one_tick() {
        assert_eq!(time_ticks(5_000, 5_000, 6, 0).len(), 1);
    }
}
