// SPDX-License-Identifier: AGPL-3.0-only
//! Test progress (spec §16 AC6): accurate, or explicitly marked uncertain, never guessed.
//!
//! A total exists only when a Fleet plan (or a suite's own plan) says how many flows there are.
//! Without one the fraction is `None` and the text says so; nothing is extrapolated from the
//! flows seen so far.

use fleet_protocol::{live::FlowView, rest::FlowCounts};

/// How many flows there are in all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Planned {
    /// A plan lists exactly this many (at least one).
    Known(u64),
    /// No plan is known: the total is unknown.
    Unknown,
}

/// Flow counts by state, and the planned total if there is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Finished and passed.
    pub passed: u64,
    /// Finished and failed.
    pub failed: u64,
    /// Not run on purpose.
    pub skipped: u64,
    /// Cut short.
    pub interrupted: u64,
    /// Finished with nothing reliable known (or a state this build doesn't know).
    pub unknown: u64,
    /// Started, not finished.
    pub running: u64,
    /// Seen and waiting to start.
    pub pending: u64,
    /// The plan.
    pub planned: Planned,
}

impl Progress {
    /// From the server's counts and the attempt's planned total. A total of zero or less is not a
    /// plan.
    #[must_use]
    pub fn from_counts(counts: &FlowCounts, planned: Option<i64>) -> Self {
        let n = |v: i64| u64::try_from(v).unwrap_or(0);
        Self {
            passed: n(counts.passed),
            failed: n(counts.failed),
            skipped: n(counts.skipped),
            interrupted: n(counts.interrupted),
            unknown: n(counts.unknown),
            running: n(counts.running),
            pending: n(counts.pending),
            planned: match planned.map(n) {
                Some(total) if total > 0 => Planned::Known(total),
                _ => Planned::Unknown,
            },
        }
    }

    /// From flow executions as the live store holds them. A state this build doesn't know counts
    /// as `unknown`, never as passed.
    #[must_use]
    pub fn from_flows<'a>(
        flows: impl IntoIterator<Item = &'a FlowView>,
        planned: Option<i64>,
    ) -> Self {
        let mut counts = FlowCounts::default();
        for flow in flows {
            match flow.state.as_str() {
                "pending" => counts.pending += 1,
                "running" => counts.running += 1,
                "passed" => counts.passed += 1,
                "failed" => counts.failed += 1,
                "skipped" => counts.skipped += 1,
                "interrupted" => counts.interrupted += 1,
                _ => counts.unknown += 1,
            }
        }
        Self::from_counts(&counts, planned)
    }

    /// Flows that reached an end state.
    #[must_use]
    pub fn finished(&self) -> u64 {
        self.passed + self.failed + self.skipped + self.interrupted + self.unknown
    }

    /// Every flow Fleet knows of, when a plan gives the total: the plan, or more if more were seen.
    #[must_use]
    pub fn total(&self) -> Option<u64> {
        match self.planned {
            Planned::Known(plan) => Some(plan.max(self.finished() + self.running + self.pending)),
            Planned::Unknown => None,
        }
    }

    /// Whether the total is unknown.
    #[must_use]
    pub fn is_uncertain(&self) -> bool {
        self.total().is_none()
    }

    /// Finished over total, 0 to 1; `None` while the total is unknown.
    #[must_use]
    pub fn fraction(&self) -> Option<f64> {
        #[expect(
            clippy::cast_precision_loss,
            reason = "flow counts are far below 2^52, so the conversion is exact"
        )]
        self.total()
            .map(|total| self.finished() as f64 / total as f64)
    }

    /// The sentence shown next to the bar.
    #[must_use]
    pub fn text(&self) -> String {
        let finished = self.finished();
        let running = if self.running > 0 {
            format!(" · {} running", self.running)
        } else {
            String::new()
        };
        if let Some(total) = self.total() {
            return format!("{finished} of {total} flows finished{running}");
        }
        let flows = match finished {
            0 => "No flows finished yet".to_owned(),
            1 => "1 flow finished".to_owned(),
            n => format!("{n} flows finished"),
        };
        format!("{flows}{running} · progress uncertain (no plan)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_protocol::frame::RunOrigin;

    fn counts(passed: i64, failed: i64, running: i64, pending: i64) -> FlowCounts {
        FlowCounts {
            passed,
            failed,
            running,
            pending,
            ..FlowCounts::default()
        }
    }

    #[test]
    fn a_planned_total_gives_a_fraction_and_a_sentence() {
        let p = Progress::from_counts(&counts(2, 1, 1, 0), Some(6));
        assert_eq!(p.finished(), 3);
        assert_eq!(p.total(), Some(6));
        assert_eq!(p.fraction(), Some(0.5));
        assert!(!p.is_uncertain());
        assert_eq!(p.text(), "3 of 6 flows finished · 1 running");
    }

    #[test]
    fn without_a_plan_the_progress_is_uncertain_and_never_extrapolated() {
        let p = Progress::from_counts(&counts(2, 1, 1, 4), None);
        assert!(p.is_uncertain());
        assert_eq!(p.fraction(), None);
        assert_eq!(
            p.text(),
            "3 flows finished · 1 running · progress uncertain (no plan)"
        );
        assert_eq!(
            Progress::from_counts(&FlowCounts::default(), None).text(),
            "No flows finished yet · progress uncertain (no plan)"
        );
        assert_eq!(
            Progress::from_counts(&counts(1, 0, 0, 0), None).text(),
            "1 flow finished · progress uncertain (no plan)"
        );
    }

    #[test]
    fn a_plan_of_nothing_is_not_a_plan() {
        for planned in [Some(0), Some(-3)] {
            assert!(Progress::from_counts(&counts(1, 0, 0, 0), planned).is_uncertain());
        }
    }

    #[test]
    fn more_flows_than_planned_raise_the_total_instead_of_overflowing_the_bar() {
        let p = Progress::from_counts(&counts(3, 0, 0, 0), Some(2));
        assert_eq!(p.total(), Some(3));
        assert_eq!(p.fraction(), Some(1.0));
    }

    #[test]
    fn flows_are_counted_by_state_and_an_unknown_state_is_never_a_pass() {
        let flow = |state: &str| FlowView {
            origin: RunOrigin::Live,
            id: uuid::Uuid::nil(),
            job_id: uuid::Uuid::nil(),
            suite_execution_id: uuid::Uuid::nil(),
            agent_run_id: uuid::Uuid::nil(),
            device_session_id: None,
            shard_number: None,
            ordinal: None,
            name: "f".into(),
            path: None,
            state: state.into(),
            outcome_source: None,
            finality: "provisional".into(),
            has_conflict: false,
            failure_message: None,
            duration_ms: None,
            steps: 0,
            rev: 1,
        };
        let flows = [
            "passed", "passed", "failed", "running", "pending", "mystery",
        ]
        .map(flow);
        let p = Progress::from_flows(&flows, Some(10));
        assert_eq!((p.passed, p.failed, p.running, p.pending), (2, 1, 1, 1));
        assert_eq!(p.unknown, 1);
        assert_eq!(p.finished(), 4);
    }
}
