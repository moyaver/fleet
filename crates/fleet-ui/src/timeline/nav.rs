// SPDX-License-Identifier: AGPL-3.0-only
//! Keyboard focus order over the bars: left and right walk time within a lane and on into the
//! next lane, up and down jump to the nearest-in-time bar of the neighbouring lane.
//!
//! Every function takes the id of the focused bar (`None` before anything has focus) and returns
//! the bar that should have focus next. At the ends, focus stays where it is.

use crate::timeline::model::{FlowBar, TimelineData};

/// The first bar in reading order.
#[must_use]
pub fn first(data: &TimelineData) -> Option<&FlowBar> {
    data.bars().first()
}

/// The last bar in reading order.
#[must_use]
pub fn last(data: &TimelineData) -> Option<&FlowBar> {
    data.bars().last()
}

/// The bar after the focused one (lane by lane, earliest first).
#[must_use]
pub fn next<'a>(data: &'a TimelineData, current: Option<&str>) -> Option<&'a FlowBar> {
    match position(data, current) {
        None => first(data),
        Some(i) => data.bars().get(i + 1).or_else(|| data.bars().get(i)),
    }
}

/// The bar before the focused one.
#[must_use]
pub fn previous<'a>(data: &'a TimelineData, current: Option<&str>) -> Option<&'a FlowBar> {
    match position(data, current) {
        None => last(data),
        Some(i) => i
            .checked_sub(1)
            .and_then(|j| data.bars().get(j))
            .or_else(|| data.bars().get(i)),
    }
}

/// The bar in the next lane down that is closest in time to the focused one, skipping lanes
/// without bars.
#[must_use]
pub fn lane_down<'a>(
    data: &'a TimelineData,
    current: Option<&str>,
    now_ms: i64,
) -> Option<&'a FlowBar> {
    let from = position(data, current).and_then(|i| data.bars().get(i));
    let lanes = from
        .and_then(|bar| data.lane_index(bar))
        .map(|lane| (lane + 1..data.lanes().len()).collect::<Vec<_>>());
    jump(data, from, lanes.unwrap_or_default(), now_ms)
}

/// Like [`lane_down`], towards the top.
#[must_use]
pub fn lane_up<'a>(
    data: &'a TimelineData,
    current: Option<&str>,
    now_ms: i64,
) -> Option<&'a FlowBar> {
    let from = position(data, current).and_then(|i| data.bars().get(i));
    let lanes = from
        .and_then(|bar| data.lane_index(bar))
        .map(|lane| (0..lane).rev().collect::<Vec<_>>());
    jump(data, from, lanes.unwrap_or_default(), now_ms)
}

fn position(data: &TimelineData, current: Option<&str>) -> Option<usize> {
    let id = current?;
    data.bars().iter().position(|b| b.id == id)
}

/// The bar of the first of `lanes` that has any, nearest in time to `from`; `from` itself when
/// none does, and the first bar when nothing has focus yet.
fn jump<'a>(
    data: &'a TimelineData,
    from: Option<&'a FlowBar>,
    lanes: Vec<usize>,
    now_ms: i64,
) -> Option<&'a FlowBar> {
    let Some(from) = from else {
        return first(data);
    };
    let target = midpoint(from, now_ms);
    for lane in lanes {
        let nearest = data
            .bars()
            .iter()
            .filter(|b| data.lane_index(b) == Some(lane))
            .min_by_key(|b| (midpoint(b, now_ms) - target).abs());
        if nearest.is_some() {
            return nearest;
        }
    }
    Some(from)
}

fn midpoint(bar: &FlowBar, now_ms: i64) -> i64 {
    bar.start_ms / 2 + bar.end_ms(now_ms) / 2
}

#[cfg(test)]
mod tests {
    use fleet_protocol::event::FlowOutcome::Passed;

    use super::*;
    use crate::timeline::model::fixtures::*;

    /// Lane a: a1, a2. Lane b: empty. Lane c: c1 (overlaps a2), c2 (far right).
    fn data() -> TimelineData {
        TimelineData::new(
            vec![lane("a"), lane("b"), lane("c")],
            vec![
                ended("c2", "c", 5000, 6000, Passed),
                ended("a2", "a", 2000, 3000, Passed),
                ended("c1", "c", 1900, 2500, Passed),
                ended("a1", "a", 0, 1000, Passed),
            ],
        )
    }

    fn id(b: Option<&FlowBar>) -> Option<&str> {
        b.map(|b| b.id.as_str())
    }

    #[test]
    fn next_and_previous_walk_lane_by_lane_and_stop_at_the_ends() {
        let d = data();
        assert_eq!(id(next(&d, None)), Some("a1"));
        assert_eq!(id(next(&d, Some("a1"))), Some("a2"));
        assert_eq!(id(next(&d, Some("a2"))), Some("c1"));
        assert_eq!(id(next(&d, Some("c2"))), Some("c2"));
        assert_eq!(id(previous(&d, Some("c1"))), Some("a2"));
        assert_eq!(id(previous(&d, Some("a1"))), Some("a1"));
        assert_eq!(id(previous(&d, None)), Some("c2"));
        assert_eq!(id(first(&d)), Some("a1"));
        assert_eq!(id(last(&d)), Some("c2"));
    }

    #[test]
    fn up_and_down_jump_to_the_nearest_in_time_bar_skipping_empty_lanes() {
        let d = data();
        // a2 is centred on 2500; c1 on 2200 and c2 on 5500.
        assert_eq!(id(lane_down(&d, Some("a2"), 0)), Some("c1"));
        // c2 is centred on 5500; a2 on 2500 and a1 on 500.
        assert_eq!(id(lane_up(&d, Some("c2"), 0)), Some("a2"));
        // Nowhere to go: stay.
        assert_eq!(id(lane_up(&d, Some("a1"), 0)), Some("a1"));
        assert_eq!(id(lane_down(&d, Some("c1"), 0)), Some("c1"));
        assert_eq!(id(lane_down(&d, None, 0)), Some("a1"));
    }

    #[test]
    fn a_running_bar_counts_up_to_now_when_comparing_times() {
        let d = TimelineData::new(
            vec![lane("a"), lane("b")],
            vec![
                running("r", "a", 1000), // 1000..9000, centred on 5000
                ended("near", "b", 4000, 5000, Passed),
                ended("far", "b", 0, 1000, Passed),
            ],
        );
        assert_eq!(id(lane_down(&d, Some("r"), 9000)), Some("near"));
    }

    #[test]
    fn an_unknown_id_behaves_like_no_focus() {
        let d = data();
        assert_eq!(id(next(&d, Some("gone"))), Some("a1"));
        assert_eq!(id(TimelineData::new(vec![], vec![]).bars().first()), None);
        assert_eq!(id(next(&TimelineData::new(vec![], vec![]), None)), None);
    }
}
