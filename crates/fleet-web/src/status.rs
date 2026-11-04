// SPDX-License-Identifier: AGPL-3.0-only
//! Wording, colour roles and provenance for the states the server reports (invariant I4).
//!
//! The server sends states as words (`running`, `junit_final`, …). Every mapping here is total:
//! a word this build doesn't know gets a neutral, honest label, never a success.

use fleet_protocol::{live::Liveness, rest::JobCounts};
use fleet_ui::{provenance::Provenance, tone::Tone};

/// What a status pill shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pill {
    /// The words, always shown: colour only reinforces them.
    pub label: &'static str,
    /// The colour role.
    pub tone: Tone,
}

impl Pill {
    const fn new(label: &'static str, tone: Tone) -> Self {
        Self { label, tone }
    }
}

/// A flow execution's state.
#[must_use]
pub fn flow_state_pill(state: &str) -> Pill {
    match state {
        "pending" => Pill::new("Pending", Tone::Neutral),
        "running" => Pill::new("Running", Tone::Info),
        "passed" => Pill::new("Passed", Tone::Success),
        "failed" => Pill::new("Failed", Tone::Danger),
        "skipped" => Pill::new("Skipped", Tone::Muted),
        "interrupted" => Pill::new("Interrupted", Tone::Warning),
        _ => Pill::new("Unknown", Tone::Neutral),
    }
}

/// A job's outcome once closed, or its lifecycle while open.
#[must_use]
pub fn job_pill(lifecycle: &str, outcome: Option<&str>) -> Pill {
    match (lifecycle, outcome) {
        ("closed", Some(outcome)) => match outcome {
            // "Completed" says Maestro ran to the end, not that the flows passed (I3): neutral.
            "completed" => Pill::new("Completed", Tone::Neutral),
            "failed" => Pill::new("Failed", Tone::Danger),
            "cancelled" => Pill::new("Cancelled", Tone::Muted),
            "interrupted" => Pill::new("Interrupted", Tone::Warning),
            _ => Pill::new("Unknown", Tone::Neutral),
        },
        ("closed", None) => Pill::new("Closed", Tone::Neutral),
        _ => open_pill(lifecycle),
    }
}

/// A job or suite that has not closed.
fn open_pill(lifecycle: &str) -> Pill {
    match lifecycle {
        "starting" => Pill::new("Starting", Tone::Info),
        "running" => Pill::new("Running", Tone::Info),
        "finalizing" => Pill::new("Finalizing", Tone::Info),
        "reporting" => Pill::new("Reporting", Tone::Info),
        _ => Pill::new("Unknown", Tone::Neutral),
    }
}

/// A suite's outcome once closed, or its lifecycle while open.
#[must_use]
pub fn suite_pill(lifecycle: &str, outcome: Option<&str>) -> Pill {
    match (lifecycle, outcome) {
        ("closed", Some("passed")) => Pill::new("Passed", Tone::Success),
        ("closed", Some("failed")) => Pill::new("Failed", Tone::Danger),
        ("closed", Some("interrupted")) => Pill::new("Interrupted", Tone::Warning),
        ("closed", Some(_)) => Pill::new("Unknown", Tone::Neutral),
        ("closed", None) => Pill::new("Closed", Tone::Neutral),
        _ => open_pill(lifecycle),
    }
}

/// An agent run's liveness; `None` when the server did not say.
#[must_use]
pub fn liveness_pill(liveness: Option<Liveness>) -> Pill {
    match liveness {
        Some(Liveness::Online) => Pill::new("Online", Tone::Success),
        Some(Liveness::Stale) => Pill::new("Stale", Tone::Warning),
        Some(Liveness::Disconnected) => Pill::new("Disconnected", Tone::Danger),
        Some(Liveness::Completed) => Pill::new("Completed", Tone::Neutral),
        None => Pill::new("Unknown", Tone::Muted),
    }
}

/// An attempt's overall state from its jobs.
#[must_use]
pub fn attempt_pill(jobs: &JobCounts) -> Pill {
    if jobs.starting + jobs.running + jobs.finalizing > 0 {
        Pill::new("Running", Tone::Info)
    } else if jobs.pending > 0 {
        Pill::new("Pending", Tone::Neutral)
    } else if jobs.closed > 0 {
        Pill::new("Finished", Tone::Neutral)
    } else {
        Pill::new("No jobs yet", Tone::Muted)
    }
}

/// The provenance of a flow's displayed outcome. `liveness` is the agent run's: a flow that is
/// still running but whose agent has gone quiet or away shows as stale, not as live.
#[must_use]
pub fn flow_provenance(finality: &str, state: &str, liveness: Option<Liveness>) -> Provenance {
    match (state, finality) {
        ("pending", _) => Provenance::Missing,
        ("running", _) if matches!(liveness, Some(Liveness::Stale | Liveness::Disconnected)) => {
            Provenance::Stale
        }
        (_, "verified") => Provenance::Final,
        (_, "inferred") => Provenance::Inferred,
        // Provisional, unverified-final (no report confirmed it) and anything this build doesn't
        // know: the weakest claim that still shows a value.
        _ => Provenance::Provisional,
    }
}

/// The provenance of a job's or suite's outcome, from the evidence source it rests on.
#[must_use]
pub fn source_provenance(source: Option<&str>) -> Provenance {
    match source {
        None => Provenance::Missing,
        Some("junit_final" | "artifact_file") => Provenance::Final,
        Some("process_exit" | "agent_observed") => Provenance::Observed,
        Some("server_inferred") => Provenance::Inferred,
        Some(_) => Provenance::Provisional,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pill(label: &'static str, tone: Tone) -> Pill {
        Pill { label, tone }
    }

    #[test]
    fn flow_states_are_spelled_out_and_an_unknown_state_is_neutral() {
        let cases = [
            ("pending", pill("Pending", Tone::Neutral)),
            ("running", pill("Running", Tone::Info)),
            ("passed", pill("Passed", Tone::Success)),
            ("failed", pill("Failed", Tone::Danger)),
            ("skipped", pill("Skipped", Tone::Muted)),
            ("interrupted", pill("Interrupted", Tone::Warning)),
            ("unknown", pill("Unknown", Tone::Neutral)),
            ("from-the-future", pill("Unknown", Tone::Neutral)),
        ];
        for (state, expected) in cases {
            assert_eq!(flow_state_pill(state), expected, "{state}");
        }
    }

    #[test]
    fn a_job_shows_its_outcome_once_closed_and_its_lifecycle_before() {
        assert_eq!(job_pill("running", None), pill("Running", Tone::Info));
        assert_eq!(job_pill("finalizing", None), pill("Finalizing", Tone::Info));
        assert_eq!(job_pill("starting", None), pill("Starting", Tone::Info));
        assert_eq!(
            job_pill("closed", Some("completed")),
            pill("Completed", Tone::Neutral)
        );
        assert_eq!(
            job_pill("closed", Some("failed")),
            pill("Failed", Tone::Danger)
        );
        assert_eq!(
            job_pill("closed", Some("cancelled")),
            pill("Cancelled", Tone::Muted)
        );
        assert_eq!(
            job_pill("closed", Some("interrupted")),
            pill("Interrupted", Tone::Warning)
        );
        assert_eq!(
            job_pill("closed", Some("unknown")),
            pill("Unknown", Tone::Neutral)
        );
        assert_eq!(job_pill("closed", None), pill("Closed", Tone::Neutral));
        assert_eq!(
            job_pill("closed", Some("odd")),
            pill("Unknown", Tone::Neutral)
        );
    }

    #[test]
    fn a_suite_shows_passed_or_failed_once_closed() {
        assert_eq!(suite_pill("running", None), pill("Running", Tone::Info));
        assert_eq!(suite_pill("reporting", None), pill("Reporting", Tone::Info));
        assert_eq!(
            suite_pill("closed", Some("passed")),
            pill("Passed", Tone::Success)
        );
        assert_eq!(
            suite_pill("closed", Some("failed")),
            pill("Failed", Tone::Danger)
        );
        assert_eq!(
            suite_pill("closed", Some("interrupted")),
            pill("Interrupted", Tone::Warning)
        );
        assert_eq!(suite_pill("closed", None), pill("Closed", Tone::Neutral));
    }

    #[test]
    fn liveness_is_spelled_out_and_unknown_when_the_server_did_not_say() {
        let cases = [
            (Some(Liveness::Online), pill("Online", Tone::Success)),
            (Some(Liveness::Stale), pill("Stale", Tone::Warning)),
            (
                Some(Liveness::Disconnected),
                pill("Disconnected", Tone::Danger),
            ),
            (Some(Liveness::Completed), pill("Completed", Tone::Neutral)),
            (None, pill("Unknown", Tone::Muted)),
        ];
        for (liveness, expected) in cases {
            assert_eq!(liveness_pill(liveness), expected);
        }
    }

    #[test]
    fn an_attempt_is_running_while_any_job_is_open() {
        let jobs = |pending, starting, running, finalizing, closed| JobCounts {
            pending,
            starting,
            running,
            finalizing,
            closed,
        };
        assert_eq!(
            attempt_pill(&jobs(0, 0, 0, 0, 0)),
            pill("No jobs yet", Tone::Muted)
        );
        assert_eq!(
            attempt_pill(&jobs(1, 0, 0, 0, 0)),
            pill("Pending", Tone::Neutral)
        );
        assert_eq!(
            attempt_pill(&jobs(1, 0, 1, 0, 2)),
            pill("Running", Tone::Info)
        );
        assert_eq!(
            attempt_pill(&jobs(0, 1, 0, 0, 0)),
            pill("Running", Tone::Info)
        );
        assert_eq!(
            attempt_pill(&jobs(0, 0, 0, 1, 3)),
            pill("Running", Tone::Info)
        );
        assert_eq!(
            attempt_pill(&jobs(0, 0, 0, 0, 3)),
            pill("Finished", Tone::Neutral)
        );
    }

    #[test]
    fn a_flow_is_only_badged_verified_when_the_report_settled_it() {
        use Provenance::{Final, Inferred, Missing, Provisional, Stale};
        let online = Some(Liveness::Online);
        let cases = [
            ("verified", "passed", online, Final),
            ("verified", "failed", online, Final),
            ("unverified_final", "passed", online, Provisional),
            ("provisional", "passed", online, Provisional),
            ("provisional", "running", online, Provisional),
            ("inferred", "unknown", online, Inferred),
            (
                "inferred",
                "interrupted",
                Some(Liveness::Disconnected),
                Inferred,
            ),
            ("provisional", "pending", online, Missing),
            ("provisional", "running", Some(Liveness::Stale), Stale),
            (
                "provisional",
                "running",
                Some(Liveness::Disconnected),
                Stale,
            ),
            ("provisional", "running", None, Provisional),
            ("verified", "passed", Some(Liveness::Disconnected), Final),
            ("from-the-future", "passed", online, Provisional),
        ];
        for (finality, state, liveness, expected) in cases {
            assert_eq!(
                flow_provenance(finality, state, liveness),
                expected,
                "{finality} {state} {liveness:?}"
            );
        }
    }

    #[test]
    fn an_outcome_source_maps_to_the_provenance_it_can_support() {
        use Provenance::{Final, Inferred, Missing, Observed, Provisional};
        let cases = [
            (Some("junit_final"), Final),
            (Some("artifact_file"), Final),
            (Some("process_exit"), Observed),
            (Some("agent_observed"), Observed),
            (Some("live_console"), Provisional),
            (Some("debug_log"), Provisional),
            (Some("server_inferred"), Inferred),
            (Some("declared"), Provisional),
            (Some("from-the-future"), Provisional),
            (None, Missing),
        ];
        for (source, expected) in cases {
            assert_eq!(source_provenance(source), expected, "{source:?}");
        }
    }
}
