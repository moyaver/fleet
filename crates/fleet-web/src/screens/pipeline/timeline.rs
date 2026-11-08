// SPDX-License-Identifier: AGPL-3.0-only
//! The REST timeline (`/attempts/{a}/timeline`) as the kit's swimlane data: a lane per device and
//! a bar per started flow, each bar carrying the provenance it is shown with (invariant I4).

use std::collections::HashMap;

use fleet_protocol::{event::FlowOutcome, live::Liveness, rest::TimelineView};
use fleet_ui::timeline::model::{BarState, FlowBar, LaneInfo, TimelineData};
use uuid::Uuid;

use crate::{clock::unix_ms, status::flow_provenance};

/// The lane of bars the server could not place on a device.
pub const UNASSIGNED_LANE: &str = "unassigned";

/// Lanes and bars for the swimlane. `liveness_of` gives an agent run's current liveness (from the
/// live store), so a bar still running on an agent that went quiet shows as stale, not live.
#[must_use]
pub fn timeline_data(
    view: &TimelineView,
    run_of_flow: impl Fn(Uuid) -> Option<Uuid>,
    liveness_of: impl Fn(Uuid) -> Option<Liveness>,
) -> TimelineData {
    let run_of: HashMap<Uuid, Uuid> = view.lanes.iter().map(|l| (l.id, l.agent_run_id)).collect();
    let mut lanes: Vec<LaneInfo> = view
        .lanes
        .iter()
        .map(|l| LaneInfo {
            id: l.id.to_string(),
            label: format!(
                "{} · {}",
                l.serial,
                l.runner_name.as_deref().unwrap_or(&l.job_key)
            ),
        })
        .collect();
    if view.bars.iter().any(|b| b.lane_id.is_none()) {
        lanes.push(LaneInfo {
            id: UNASSIGNED_LANE.to_owned(),
            label: "No device".to_owned(),
        });
    }
    let bars = view
        .bars
        .iter()
        .map(|b| {
            let start_ms = unix_ms(b.started_at);
            let liveness = b
                .lane_id
                .and_then(|lane| run_of.get(&lane).copied())
                .or_else(|| run_of_flow(b.flow_id))
                .and_then(&liveness_of);
            FlowBar {
                id: b.flow_id.to_string(),
                lane_id: b
                    .lane_id
                    .map_or_else(|| UNASSIGNED_LANE.to_owned(), |l| l.to_string()),
                label: b.name.clone(),
                start_ms,
                state: match b.state.as_str() {
                    "pending" | "running" => BarState::Running,
                    other => BarState::Ended {
                        end_ms: b
                            .ended_at
                            .map(unix_ms)
                            .or_else(|| b.duration_ms.map(|d| start_ms.saturating_add(d)))
                            .unwrap_or(start_ms),
                        outcome: outcome(other),
                    },
                },
                provenance: flow_provenance(
                    &b.finality,
                    &b.state,
                    b.outcome_source.as_deref(),
                    liveness,
                ),
            }
        })
        .collect();
    TimelineData::new(lanes, bars)
}

fn outcome(state: &str) -> FlowOutcome {
    match state {
        "passed" => FlowOutcome::Passed,
        "failed" => FlowOutcome::Failed,
        "skipped" => FlowOutcome::Skipped,
        "interrupted" => FlowOutcome::Interrupted,
        _ => FlowOutcome::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use fleet_ui::provenance::Provenance;
    use serde_json::json;

    use super::*;

    const RUN_A: &str = "00000000-0000-0000-0000-0000000000a1";

    fn view() -> TimelineView {
        serde_json::from_value(json!({
            "attempt_id": "00000000-0000-0000-0000-000000000001", "version": 5,
            "generated_at": "2026-01-01T00:01:00Z",
            "lanes": [{
                "id": "00000000-0000-0000-0000-0000000000d1", "serial": "emulator-5554",
                "state": "busy", "agent_run_id": RUN_A,
                "job_id": "00000000-0000-0000-0000-00000000000a", "job_key": "e2e",
                "runner_name": "runner-1",
                "first_seen_at": "2026-01-01T00:00:00Z", "last_seen_at": "2026-01-01T00:01:00Z"
            }],
            "bars": [
                bar("01", Some("d1"), "login", "passed", "verified", Some("2026-01-01T00:00:05Z"), Some(5000)),
                bar("02", Some("d1"), "checkout", "failed", "unverified_final", None, Some(2000)),
                bar("03", Some("d1"), "wait", "running", "provisional", None, None),
                bar("04", None, "orphan", "interrupted", "inferred", None, None),
            ],
            "not_started": 2
        }))
        .unwrap()
    }

    fn bar(
        n: &str,
        lane: Option<&str>,
        name: &str,
        state: &str,
        finality: &str,
        ended: Option<&str>,
        duration: Option<i64>,
    ) -> serde_json::Value {
        json!({
            "flow_id": format!("00000000-0000-0000-0000-0000000000{n}"),
            "lane_id": lane.map(|l| format!("00000000-0000-0000-0000-0000000000{l}")),
            "name": name, "shard_number": 1, "ordinal": 0, "state": state,
            "outcome_source": null, "finality": finality, "has_conflict": false,
            "started_at": "2026-01-01T00:00:00Z", "ended_at": ended, "duration_ms": duration
        })
    }

    fn bars(liveness: Option<Liveness>) -> Vec<FlowBar> {
        timeline_data(&view(), |_| None, |_| liveness)
            .bars()
            .to_vec()
    }

    fn by_name<'a>(bars: &'a [FlowBar], name: &str) -> &'a FlowBar {
        bars.iter().find(|b| b.label == name).unwrap()
    }

    #[test]
    fn a_lane_is_a_device_labelled_with_its_runner() {
        let data = timeline_data(&view(), |_| None, |_| None);
        assert_eq!(data.lanes()[0].label, "emulator-5554 · runner-1");
    }

    #[test]
    fn ended_bars_end_at_their_end_time_or_start_plus_duration_and_keep_their_outcome() {
        let bars = bars(Some(Liveness::Online));
        let start = 1_767_225_600_000;
        assert_eq!(
            by_name(&bars, "login").state,
            BarState::Ended {
                end_ms: start + 5000,
                outcome: FlowOutcome::Passed
            }
        );
        assert_eq!(
            by_name(&bars, "checkout").state,
            BarState::Ended {
                end_ms: start + 2000,
                outcome: FlowOutcome::Failed
            },
            "no end time: start plus Maestro's duration"
        );
        assert_eq!(by_name(&bars, "wait").state, BarState::Running);
    }

    #[test]
    fn provenance_follows_finality_and_a_quiet_agent_makes_running_bars_stale() {
        let live = bars(Some(Liveness::Online));
        assert_eq!(by_name(&live, "login").provenance, Provenance::Final);
        assert_eq!(
            by_name(&live, "checkout").provenance,
            Provenance::Provisional
        );
        assert_eq!(by_name(&live, "wait").provenance, Provenance::Provisional);
        assert_eq!(by_name(&live, "orphan").provenance, Provenance::Inferred);

        let quiet = bars(Some(Liveness::Stale));
        assert_eq!(by_name(&quiet, "wait").provenance, Provenance::Stale);
        assert_eq!(
            by_name(&quiet, "login").provenance,
            Provenance::Final,
            "settled results stay settled"
        );
    }

    #[test]
    fn an_unassigned_running_bar_follows_its_agents_liveness() {
        let mut v = view();
        v.bars[3].state = "running".into();
        v.bars[3].finality = "provisional".into();
        let data = timeline_data(
            &v,
            |_| Some(Uuid::parse_str(RUN_A).unwrap()),
            |_| Some(Liveness::Stale),
        );
        assert_eq!(by_name(data.bars(), "orphan").provenance, Provenance::Stale);
    }

    #[test]
    fn a_bar_without_a_device_gets_a_lane_of_its_own_instead_of_vanishing() {
        let data = timeline_data(&view(), |_| None, |_| None);
        let orphan = by_name(data.bars(), "orphan");
        assert_eq!(orphan.lane_id, UNASSIGNED_LANE);
        assert!(
            data.lanes()
                .iter()
                .any(|l| l.id == UNASSIGNED_LANE && l.label == "No device")
        );
    }
}
