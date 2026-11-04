// SPDX-License-Identifier: AGPL-3.0-only
//! The timeline as a table: the same flows for people who can't use the canvas.

use crate::charts::format::{format_duration, format_timestamp};
use crate::timeline::model::{BarState, TimelineData};
use crate::timeline::style::bar_style;

/// One flow as a table row, every cell already text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    /// The flow execution id.
    pub bar_id: String,
    /// The device's label.
    pub device: String,
    /// The flow's name.
    pub flow: String,
    /// When it started.
    pub start: String,
    /// When it ended, or `running`.
    pub end: String,
    /// How long it took, or `… so far` while it runs.
    pub duration: String,
    /// `passed`, `failed`, `running`, …
    pub outcome: String,
    /// `final`, `provisional`, …
    pub provenance: String,
    /// Still running.
    pub running: bool,
}

impl TableRow {
    /// One sentence for assistive technology, also used to announce keyboard focus.
    #[must_use]
    pub fn summary(&self) -> String {
        let how_long = if self.running {
            self.duration.clone()
        } else {
            format!("took {}", self.duration)
        };
        format!(
            "{} on {}: {} ({}), started {}, {how_long}",
            self.flow, self.device, self.outcome, self.provenance, self.start
        )
    }
}

/// A row per bar, ordered like the lanes and bars (device, then start time).
#[must_use]
pub fn table_rows(data: &TimelineData, now_ms: i64, utc_offset_minutes: i32) -> Vec<TableRow> {
    let time = |ms| format_timestamp(ms, utc_offset_minutes);
    data.bars()
        .iter()
        .map(|bar| {
            let style = bar_style(bar.state, bar.provenance);
            let running = matches!(bar.state, BarState::Running);
            let elapsed = format_duration(bar.end_ms(now_ms) - bar.start_ms);
            TableRow {
                bar_id: bar.id.clone(),
                device: data
                    .lane_index(bar)
                    .and_then(|i| data.lanes().get(i))
                    .map_or_else(|| bar.lane_id.clone(), |l| l.label.clone()),
                flow: bar.label.clone(),
                start: time(bar.start_ms),
                end: if running {
                    "running".to_owned()
                } else {
                    time(bar.end_ms(now_ms))
                },
                duration: if running {
                    format!("{elapsed} so far")
                } else {
                    elapsed
                },
                outcome: style.outcome.to_owned(),
                provenance: bar.provenance.badge().label.to_lowercase(),
                running,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use fleet_protocol::event::FlowOutcome;

    use super::*;
    use crate::provenance::Provenance;
    use crate::timeline::model::fixtures::*;

    const T0: i64 = 1_700_000_000_000; // 2023-11-14 22:13:20 UTC

    fn data() -> TimelineData {
        let mut failed = ended("login", "a", T0, T0 + 4_200, FlowOutcome::Failed);
        failed.provenance = Provenance::Provisional;
        TimelineData::new(
            vec![lane("a")],
            vec![failed, running("checkout", "a", T0 + 10_000)],
        )
    }

    #[test]
    fn an_ended_flow_reads_as_a_row_with_outcome_and_provenance() {
        let rows = table_rows(&data(), T0 + 15_000, 0);
        assert_eq!(
            rows[0],
            TableRow {
                bar_id: "login".to_owned(),
                device: "Device a".to_owned(),
                flow: "login.yaml".to_owned(),
                start: "2023-11-14 22:13:20".to_owned(),
                end: "2023-11-14 22:13:24".to_owned(),
                duration: "4.2 s".to_owned(),
                outcome: "failed".to_owned(),
                provenance: "provisional".to_owned(),
                running: false,
            }
        );
        assert_eq!(
            rows[0].summary(),
            "login.yaml on Device a: failed (provisional), started 2023-11-14 22:13:20, took 4.2 s"
        );
    }

    #[test]
    fn a_running_flow_has_no_end_and_a_duration_so_far() {
        let rows = table_rows(&data(), T0 + 15_000, 0);
        let r = &rows[1];
        assert_eq!(
            (r.end.as_str(), r.duration.as_str()),
            ("running", "5 s so far")
        );
        assert_eq!(
            (r.outcome.as_str(), r.provenance.as_str()),
            ("running", "observed")
        );
        assert_eq!(
            r.summary(),
            "checkout.yaml on Device a: running (observed), started 2023-11-14 22:13:30, 5 s so far"
        );
    }

    #[test]
    fn times_follow_the_viewers_offset() {
        let rows = table_rows(&data(), T0, 120);
        assert_eq!(rows[0].start, "2023-11-15 00:13:20");
    }
}
