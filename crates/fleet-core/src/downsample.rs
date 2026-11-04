// SPDX-License-Identifier: AGPL-3.0-only
//! Largest-Triangle-Three-Buckets downsampling for charts (PLAN §5.4, §13).
//!
//! The function returns *indices* into the input rather than new points: a chart can only ever
//! draw samples that exist, and the caller keeps whatever else it knows about them (units,
//! provenance). Nothing is averaged, interpolated or invented.

/// Picks at most `threshold` of `points` (`(x, y)`, with `x` ascending) that best preserve the
/// visual shape of the line, as ascending indices into `points`.
///
/// - The first and last points are always kept (when `threshold >= 2`).
/// - `threshold >= points.len()` returns every index; `threshold == 0` returns none.
/// - Points with a NaN coordinate are never preferred over a finite one in the same bucket.
///
/// # Examples
///
/// ```
/// use fleet_core::downsample::lttb;
///
/// let points = [(0.0, 0.0), (1.0, 10.0), (2.0, 0.0), (3.0, 0.0), (4.0, 0.0)];
/// assert_eq!(lttb(&points, 3), vec![0, 1, 4]);
/// ```
#[must_use]
pub fn lttb(points: &[(f64, f64)], threshold: usize) -> Vec<usize> {
    let n = points.len();
    match threshold {
        0 => return Vec::new(),
        _ if threshold >= n => return (0..n).collect(),
        1 => return vec![0],
        2 => return vec![0, n - 1],
        _ => {}
    }
    let buckets = threshold - 2;
    // Bucket `i` of the n - 2 interior points is `start(i)..start(i + 1)`; `start(buckets)` is
    // the last point's index. Integer arithmetic keeps every bucket non-empty (n > threshold).
    let start = |i: usize| 1 + i * (n - 2) / buckets;
    let mut picked = Vec::with_capacity(threshold);
    picked.push(0);
    let mut anchor = points.first().copied().unwrap_or_default();
    for i in 0..buckets {
        let next = points
            .get(start(i + 1)..start(i + 2).min(n))
            .unwrap_or_default();
        let next_avg = centroid(next);
        let lo = start(i);
        let bucket = points.get(lo..start(i + 1)).unwrap_or_default();
        let mut best = (lo, -1.0_f64);
        for (offset, &p) in bucket.iter().enumerate() {
            let area = triangle_area(anchor, p, next_avg);
            if area > best.1 {
                best = (lo + offset, area);
            }
        }
        anchor = points.get(best.0).copied().unwrap_or(anchor);
        picked.push(best.0);
    }
    picked.push(n - 1);
    picked
}

#[expect(
    clippy::cast_precision_loss,
    reason = "bucket sizes are chart point counts, far below 2^52"
)]
fn centroid(points: &[(f64, f64)]) -> (f64, f64) {
    let len = points.len().max(1) as f64;
    let (sx, sy) = points
        .iter()
        .fold((0.0, 0.0), |(sx, sy), &(x, y)| (sx + x, sy + y));
    (sx / len, sy / len)
}

/// The area of the triangle `abc`.
fn triangle_area(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
    ((a.0 - c.0) * (b.1 - a.1) - (a.0 - b.0) * (c.1 - a.1)).abs() * 0.5
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn one_bucket_keeps_the_spike() {
        // Middle bucket is indices 1..=3; with both neighbours at y = 0 the triangle is largest
        // for the point furthest from the line y = 0.
        let pts = [(0.0, 0.0), (1.0, 10.0), (2.0, 0.0), (3.0, 0.0), (4.0, 0.0)];
        assert_eq!(lttb(&pts, 3), vec![0, 1, 4]);
    }

    #[test]
    fn two_buckets_follow_the_running_anchor() {
        // Worked by hand. Buckets are {1, 2} and {3, 4}. Anchor (0, 0), next-bucket centroid
        // (3.5, 5): areas 6.25 for index 1 and 3.25 for index 2, so index 1 wins. Anchor (1, 5),
        // last point (5, 0): areas 3 for index 3 and 15.5 for index 4, so index 4 wins.
        let pts = [
            (0.0, 0.0),
            (1.0, 5.0),
            (2.0, 1.0),
            (3.0, 1.0),
            (4.0, 9.0),
            (5.0, 0.0),
        ];
        assert_eq!(lttb(&pts, 4), vec![0, 1, 4, 5]);
    }

    #[test]
    fn degenerate_thresholds() {
        let pts = [(0.0, 1.0), (1.0, 2.0), (2.0, 3.0)];
        assert_eq!(lttb(&pts, 0), Vec::<usize>::new());
        assert_eq!(lttb(&pts, 1), vec![0]);
        assert_eq!(lttb(&pts, 2), vec![0, 2]);
        assert_eq!(lttb(&pts, 3), vec![0, 1, 2]);
        assert_eq!(lttb(&pts, 99), vec![0, 1, 2]);
        assert_eq!(lttb(&[], 5), Vec::<usize>::new());
        assert_eq!(lttb(&[(0.0, 0.0)], 2), vec![0]);
    }

    proptest! {
        #[test]
        fn picks_are_a_subsequence_that_keeps_the_ends(
            ys in prop::collection::vec(-1.0e6_f64..1.0e6, 0..400),
            threshold in 0_usize..60,
        ) {
            let pts: Vec<(f64, f64)> = (0_u32..).zip(&ys).map(|(i, &y)| (f64::from(i), y)).collect();
            let picked = lttb(&pts, threshold);
            // Never invents points: strictly ascending, in-bounds indices.
            prop_assert!(picked.windows(2).all(|w| w[0] < w[1]));
            prop_assert!(picked.iter().all(|&i| i < pts.len()));
            // Exactly `threshold` points once there are more than that.
            prop_assert_eq!(picked.len(), threshold.min(pts.len()));
            if threshold >= 2 && !pts.is_empty() {
                prop_assert_eq!(picked.first(), Some(&0));
                prop_assert_eq!(picked.last(), Some(&(pts.len() - 1)));
            }
        }

        #[test]
        fn a_lone_spike_always_survives(
            n in 20_u32..300,
            spike_at in 1_usize..19,
            threshold in 3_usize..19,
        ) {
            let mut pts: Vec<(f64, f64)> = (0..n).map(|i| (f64::from(i), 0.0)).collect();
            pts[spike_at].1 = 1000.0;
            prop_assert!(lttb(&pts, threshold).contains(&spike_at));
        }
    }
}
