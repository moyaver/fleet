// SPDX-License-Identifier: AGPL-3.0-only
//! The device timeline of screen C: the swimlane's data from `GET …/attempts/{id}/timeline`,
//! cut down to the devices the page is about.
//!
//! Its time axis is the one the metric charts use, so a cursor over either shows the same moment
//! on both: that is how a metric spike is matched with the flow that was running.

use fleet_protocol::rest::{DeviceSessionView, TimelineView};
use fleet_ui::timeline::model::TimelineData;
use uuid::Uuid;

use crate::screens::pipeline::timeline as attempt_timeline;

/// What to load for a page's timeline: the attempts whose timelines hold the page's devices,
/// and those devices' lanes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineScope {
    /// Pipeline attempts to fetch a timeline for, each once.
    pub attempts: Vec<Uuid>,
    /// Device sessions to keep.
    pub lanes: Vec<Uuid>,
}

impl TimelineScope {
    /// The scope of these device sessions.
    #[must_use]
    pub fn of_sessions(sessions: &[DeviceSessionView]) -> Self {
        let mut scope = Self {
            attempts: Vec::new(),
            lanes: Vec::new(),
        };
        for s in sessions {
            if !scope.attempts.contains(&s.pipeline_attempt_id) {
                scope.attempts.push(s.pipeline_attempt_id);
            }
            if !scope.lanes.contains(&s.device.id) {
                scope.lanes.push(s.device.id);
            }
        }
        scope
    }
}

/// The swimlane of the devices `lanes` out of the given timelines (one per pipeline attempt),
/// drawn the way screen B draws it. A device in several responses is one lane; a flow no event
/// placed on a device is not on any device's timeline and is left out.
///
/// Liveness is not known here (these pages follow no live socket), so a bar is never marked
/// stale on the strength of an agent that went quiet.
#[must_use]
pub fn timeline_data(views: &[TimelineView], lanes: &[Uuid]) -> TimelineData {
    let mut out_lanes = Vec::new();
    let mut out_bars = Vec::new();
    for view in views {
        let mut mine = view.clone();
        mine.lanes.retain(|l| lanes.contains(&l.id));
        mine.bars
            .retain(|b| b.lane_id.is_some_and(|l| lanes.contains(&l)));
        let data = attempt_timeline::timeline_data(&mine, |_| None);
        for lane in data.lanes() {
            if !out_lanes
                .iter()
                .any(|x: &fleet_ui::timeline::model::LaneInfo| x.id == lane.id)
            {
                out_lanes.push(lane.clone());
            }
        }
        for bar in data.bars() {
            if !out_bars
                .iter()
                .any(|x: &fleet_ui::timeline::model::FlowBar| x.id == bar.id)
            {
                out_bars.push(bar.clone());
            }
        }
    }
    TimelineData::new(out_lanes, out_bars)
}

#[cfg(test)]
mod tests {
    use fleet_protocol::event::FlowOutcome;
    use fleet_ui::{provenance::Provenance, timeline::model::BarState};

    use super::*;

    const D1: &str = "00000000-0000-0000-0000-0000000000d1";
    const D2: &str = "00000000-0000-0000-0000-0000000000d2";

    fn id(s: &str) -> Uuid {
        s.parse().unwrap()
    }

    /// Two devices: d1 ran a passed flow and one still running; d2 ran a failed one. One flow
    /// has no device at all.
    const TIMELINE: &str = r#"{
      "attempt_id": "00000000-0000-0000-0000-0000000000c1", "version": 9,
      "generated_at": "2026-01-01T11:00:00Z",
      "lanes": [
        {"id": "00000000-0000-0000-0000-0000000000d1", "serial": "emulator-5554",
         "state": "busy", "agent_run_id": "00000000-0000-0000-0000-000000000101",
         "job_id": "00000000-0000-0000-0000-0000000000b1", "job_key": "e2e-shard-1",
         "runner_name": "mac-mini-3",
         "first_seen_at": "2026-01-01T10:00:00Z", "last_seen_at": "2026-01-01T10:59:00Z"},
        {"id": "00000000-0000-0000-0000-0000000000d2", "serial": "emulator-5556",
         "state": "busy", "agent_run_id": "00000000-0000-0000-0000-000000000102",
         "job_id": "00000000-0000-0000-0000-0000000000b2", "job_key": "e2e-shard-2",
         "runner_name": "mac-mini-4",
         "first_seen_at": "2026-01-01T10:00:00Z", "last_seen_at": "2026-01-01T10:59:00Z"}
      ],
      "bars": [
        {"flow_id": "00000000-0000-0000-0000-0000000000f1",
         "lane_id": "00000000-0000-0000-0000-0000000000d1", "name": "onboarding",
         "shard_number": 1, "ordinal": 1, "state": "passed", "outcome_source": "junit_final",
         "finality": "verified", "has_conflict": false,
         "started_at": "2026-01-01T10:10:00Z", "ended_at": "2026-01-01T10:10:08Z",
         "duration_ms": 8000},
        {"flow_id": "00000000-0000-0000-0000-0000000000f3",
         "lane_id": "00000000-0000-0000-0000-0000000000d1", "name": "checkout",
         "shard_number": 1, "ordinal": 2, "state": "running", "outcome_source": null,
         "finality": "provisional", "has_conflict": false,
         "started_at": "2026-01-01T10:50:00Z", "ended_at": null, "duration_ms": null},
        {"flow_id": "00000000-0000-0000-0000-0000000000f4",
         "lane_id": "00000000-0000-0000-0000-0000000000d2", "name": "search",
         "shard_number": 2, "ordinal": 1, "state": "failed", "outcome_source": "live_console",
         "finality": "provisional", "has_conflict": false,
         "started_at": "2026-01-01T10:20:00Z", "ended_at": "2026-01-01T10:20:30Z",
         "duration_ms": 30000},
        {"flow_id": "00000000-0000-0000-0000-0000000000f5", "lane_id": null, "name": "orphan",
         "shard_number": null, "ordinal": null, "state": "unknown", "outcome_source": null,
         "finality": "provisional", "has_conflict": false,
         "started_at": "2026-01-01T10:30:00Z", "ended_at": null, "duration_ms": null}
      ],
      "not_started": 0
    }"#;

    fn view() -> TimelineView {
        serde_json::from_str(TIMELINE).unwrap()
    }

    #[test]
    fn a_device_page_keeps_only_its_own_lane_and_the_flows_on_it() {
        let data = timeline_data(&[view()], &[id(D1)]);
        let lanes: Vec<_> = data
            .lanes()
            .iter()
            .map(|l| (l.id.as_str(), l.label.as_str()))
            .collect();
        assert_eq!(lanes, [(D1, "emulator-5554 · mac-mini-3")]);
        let bars: Vec<_> = data.bars().iter().map(|b| b.label.as_str()).collect();
        assert_eq!(bars, ["onboarding", "checkout"]);
    }

    #[test]
    fn a_worker_page_keeps_every_device_it_hosted_and_drops_flows_with_no_device() {
        let data = timeline_data(&[view()], &[id(D1), id(D2)]);
        assert_eq!(data.lanes().len(), 2);
        let bars: Vec<_> = data.bars().iter().map(|b| b.label.as_str()).collect();
        assert!(!bars.contains(&"orphan"));
        assert_eq!(bars.len(), 3);
    }

    #[test]
    fn bars_carry_outcome_end_and_provenance_from_the_server() {
        let data = timeline_data(&[view()], &[id(D1), id(D2)]);
        let bar = |name: &str| data.bars().iter().find(|b| b.label == name).unwrap();
        // 2026-01-01T10:10:00Z / :10:08Z
        let passed = bar("onboarding");
        assert_eq!(passed.start_ms, 1_767_262_200_000);
        assert_eq!(
            passed.state,
            BarState::Ended {
                end_ms: 1_767_262_208_000,
                outcome: FlowOutcome::Passed
            }
        );
        assert_eq!(passed.provenance, Provenance::Final);
        // A live-console failure stays provisional on the bar.
        let failed = bar("search");
        assert!(matches!(
            failed.state,
            BarState::Ended {
                outcome: FlowOutcome::Failed,
                ..
            }
        ));
        assert_eq!(failed.provenance, Provenance::Provisional);
        // Running: open-ended, drawn as screen B draws it.
        let running = bar("checkout");
        assert_eq!(running.state, BarState::Running);
        assert_eq!(running.provenance, Provenance::Provisional);
        assert_eq!(running.id, "00000000-0000-0000-0000-0000000000f3");
    }

    #[test]
    fn flows_from_several_attempts_share_one_timeline() {
        let mut other = view();
        other.attempt_id = id("00000000-0000-0000-0000-0000000000c2");
        other.lanes.retain(|l| l.id == id(D2));
        other.bars.retain(|b| b.lane_id == Some(id(D2)));
        let data = timeline_data(&[view(), other], &[id(D1), id(D2)]);
        // d2 appears in both responses: one lane, its flows once each.
        assert_eq!(data.lanes().len(), 2);
        assert_eq!(
            data.bars().iter().filter(|b| b.label == "search").count(),
            1
        );
    }

    #[test]
    fn the_scope_names_each_attempt_once_and_every_device_session() {
        let session = |device: &str, attempt: &str| -> fleet_protocol::rest::DeviceSessionView {
            serde_json::from_value(serde_json::json!({
              "device": {"id": device, "agent_run_id": "00000000-0000-0000-0000-000000000101",
                         "serial": "emulator-5554", "state": "ready", "rev": 1},
              "worker_id": null, "runner_name": null, "pipeline_attempt_id": attempt,
              "job_id": "00000000-0000-0000-0000-0000000000b1", "job_key": "k",
              "first_seen_at": "2026-01-01T10:00:00Z", "last_seen_at": "2026-01-01T10:00:00Z",
              "profile": null
            }))
            .unwrap()
        };
        let c1 = "00000000-0000-0000-0000-0000000000c1";
        let c2 = "00000000-0000-0000-0000-0000000000c2";
        let scope =
            TimelineScope::of_sessions(&[session(D1, c1), session(D2, c1), session(D1, c2)]);
        assert_eq!(scope.attempts, [id(c1), id(c2)]);
        assert_eq!(scope.lanes, [id(D1), id(D2)]);
    }
}
