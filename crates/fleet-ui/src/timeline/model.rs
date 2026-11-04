// SPDX-License-Identifier: AGPL-3.0-only
//! The timeline's data: devices (lanes) and the flows that ran on them (bars).

use fleet_protocol::event::FlowOutcome;

use crate::provenance::Provenance;

/// A lane: one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneInfo {
    /// Stable identifier, the key bars refer to.
    pub id: String,
    /// What the lane is called on screen.
    pub label: String,
}

/// Where a flow stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarState {
    /// Started, not finished: the bar stays open-ended and grows to "now".
    Running,
    /// Finished.
    Ended {
        /// When it ended, unix ms.
        end_ms: i64,
        /// How it ended, as resolved by the claim lattice.
        outcome: FlowOutcome,
    },
}

/// One flow execution on one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowBar {
    /// Stable identifier (the execution id).
    pub id: String,
    /// The lane it ran on.
    pub lane_id: String,
    /// The flow's name.
    pub label: String,
    /// When it started, unix ms.
    pub start_ms: i64,
    /// Running or ended.
    pub state: BarState,
    /// How settled the displayed outcome is.
    pub provenance: Provenance,
}

impl FlowBar {
    /// When the bar ends: its end, or `now_ms` while it runs. Never before its start.
    #[must_use]
    pub fn end_ms(&self, now_ms: i64) -> i64 {
        match self.state {
            BarState::Running => now_ms.max(self.start_ms),
            BarState::Ended { end_ms, .. } => end_ms.max(self.start_ms),
        }
    }
}

/// Lanes and bars, ordered for drawing and for keyboard navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineData {
    lanes: Vec<LaneInfo>,
    bars: Vec<FlowBar>,
}

impl TimelineData {
    /// Orders bars by lane, then start time. A bar on an unknown lane gets a lane of its own
    /// (labelled with the lane id) rather than being dropped.
    #[must_use]
    pub fn new(mut lanes: Vec<LaneInfo>, mut bars: Vec<FlowBar>) -> Self {
        for bar in &bars {
            if !lanes.iter().any(|l| l.id == bar.lane_id) {
                lanes.push(LaneInfo {
                    id: bar.lane_id.clone(),
                    label: bar.lane_id.clone(),
                });
            }
        }
        let lane_of = |bar: &FlowBar| lanes.iter().position(|l| l.id == bar.lane_id);
        bars.sort_by_key(|b| (lane_of(b), b.start_ms));
        Self { lanes, bars }
    }

    /// The lanes, top to bottom.
    #[must_use]
    pub fn lanes(&self) -> &[LaneInfo] {
        &self.lanes
    }

    /// The bars, ordered by lane then start time.
    #[must_use]
    pub fn bars(&self) -> &[FlowBar] {
        &self.bars
    }

    /// The index of a bar's lane.
    #[must_use]
    pub fn lane_index(&self, bar: &FlowBar) -> Option<usize> {
        self.lanes.iter().position(|l| l.id == bar.lane_id)
    }

    /// The time span of everything, with running bars reaching `now_ms`. `None` without bars.
    #[must_use]
    pub fn bounds(&self, now_ms: i64) -> Option<(i64, i64)> {
        let start = self.bars.iter().map(|b| b.start_ms).min()?;
        let end = self.bars.iter().map(|b| b.end_ms(now_ms)).max()?;
        Some((start, end))
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    pub(crate) fn lane(id: &str) -> LaneInfo {
        LaneInfo {
            id: id.to_owned(),
            label: format!("Device {id}"),
        }
    }

    pub(crate) fn ended(
        id: &str,
        lane: &str,
        start: i64,
        end: i64,
        outcome: FlowOutcome,
    ) -> FlowBar {
        FlowBar {
            id: id.to_owned(),
            lane_id: lane.to_owned(),
            label: format!("{id}.yaml"),
            start_ms: start,
            state: BarState::Ended {
                end_ms: end,
                outcome,
            },
            provenance: Provenance::Final,
        }
    }

    pub(crate) fn running(id: &str, lane: &str, start: i64) -> FlowBar {
        FlowBar {
            state: BarState::Running,
            provenance: Provenance::Observed,
            ..ended(id, lane, start, start, FlowOutcome::Unknown)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn bars_are_ordered_by_lane_then_start() {
        let d = TimelineData::new(
            vec![lane("a"), lane("b")],
            vec![
                ended("b2", "b", 5000, 6000, FlowOutcome::Passed),
                ended("a2", "a", 2000, 3000, FlowOutcome::Passed),
                ended("b1", "b", 1000, 2000, FlowOutcome::Failed),
                ended("a1", "a", 0, 1000, FlowOutcome::Passed),
            ],
        );
        let ids: Vec<&str> = d.bars().iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["a1", "a2", "b1", "b2"]);
    }

    #[test]
    fn a_bar_on_an_unknown_lane_is_kept_on_a_lane_of_its_own() {
        let d = TimelineData::new(
            vec![lane("a")],
            vec![ended("x", "ghost", 0, 1, FlowOutcome::Passed)],
        );
        assert_eq!(d.lanes().len(), 2);
        assert_eq!(d.lanes()[1].id, "ghost");
        assert_eq!(d.lanes()[1].label, "ghost");
        assert_eq!(d.bars().len(), 1);
    }

    #[test]
    fn running_bars_reach_now_and_ended_bars_never_end_before_they_start() {
        let d = TimelineData::new(
            vec![lane("a")],
            vec![
                ended("e", "a", 1000, 900, FlowOutcome::Passed), // clock skew
                running("r", "a", 4000),
            ],
        );
        assert_eq!(d.bars()[0].end_ms(9000), 1000);
        assert_eq!(d.bars()[1].end_ms(9000), 9000);
        // A running bar whose start is after "now" (skew) is not negative-length either.
        assert_eq!(d.bars()[1].end_ms(3000), 4000);
        assert_eq!(d.bounds(9000), Some((1000, 9000)));
        assert_eq!(
            TimelineData::new(vec![lane("a")], vec![]).bounds(9000),
            None
        );
    }
}
