// SPDX-License-Identifier: AGPL-3.0-only
//! Wording, marks and provenance for the states the server reports (invariant I4).
//!
//! The server sends states as words (`running`, `junit_final`, …). Every mapping here is total:
//! a word this build doesn't know gets a neutral, honest label, never a success.
//!
//! The words are one Title Case vocabulary: Passed, Failed, Running, Not started, Interrupted,
//! Skipped, Unknown (the redesign's Foundations). A job that ran to its end is `Completed`, with a
//! neutral end mark, never a check (I3).

use fleet_protocol::{live::Liveness, rest::JobCounts};
use fleet_ui::{mark::Mark, provenance::Provenance};

/// What a status pill shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pill {
    /// The word, always shown: the icon and colour only reinforce it.
    pub label: &'static str,
    /// The icon and colour of the word.
    pub mark: Mark,
}

impl Pill {
    const fn new(label: &'static str, mark: Mark) -> Self {
        Self { label, mark }
    }
}

/// A flow execution's state.
#[must_use]
pub fn flow_state_pill(state: &str) -> Pill {
    match state {
        "pending" => Pill::new("Not started", Mark::NotStarted),
        "running" => Pill::new("Running", Mark::Running),
        "passed" => Pill::new("Passed", Mark::Passed),
        "failed" => Pill::new("Failed", Mark::Failed),
        "skipped" => Pill::new("Skipped", Mark::Skipped),
        "interrupted" => Pill::new("Interrupted", Mark::Interrupted),
        _ => Pill::new("Unknown", Mark::Unknown),
    }
}

/// A job's outcome once closed, or its lifecycle while open.
#[must_use]
pub fn job_pill(lifecycle: &str, outcome: Option<&str>) -> Pill {
    match (lifecycle, outcome) {
        ("closed", Some(outcome)) => match outcome {
            // "Completed" says Maestro ran to the end, not that the flows passed (I3): neutral.
            "completed" => Pill::new("Completed", Mark::End),
            "failed" => Pill::new("Failed", Mark::Failed),
            // Cut short by a person or the runner: the Interrupted vocabulary, not a new word.
            "cancelled" | "interrupted" => Pill::new("Interrupted", Mark::Interrupted),
            _ => Pill::new("Unknown", Mark::Unknown),
        },
        ("closed", None) => Pill::new("Completed", Mark::End),
        _ => open_pill(lifecycle),
    }
}

/// A job or suite that has not closed.
fn open_pill(lifecycle: &str) -> Pill {
    match lifecycle {
        "starting" | "running" | "finalizing" | "reporting" => Pill::new("Running", Mark::Running),
        _ => Pill::new("Unknown", Mark::Unknown),
    }
}

/// A suite's outcome once closed, or its lifecycle while open.
#[must_use]
pub fn suite_pill(lifecycle: &str, outcome: Option<&str>) -> Pill {
    match (lifecycle, outcome) {
        ("closed", Some("passed")) => Pill::new("Passed", Mark::Passed),
        ("closed", Some("failed")) => Pill::new("Failed", Mark::Failed),
        ("closed", Some("interrupted")) => Pill::new("Interrupted", Mark::Interrupted),
        ("closed", Some(_)) => Pill::new("Unknown", Mark::Unknown),
        ("closed", None) => Pill::new("Completed", Mark::End),
        _ => open_pill(lifecycle),
    }
}

/// An agent run's liveness; `None` when the server did not say.
#[must_use]
pub fn liveness_pill(liveness: Option<Liveness>) -> Pill {
    match liveness {
        Some(Liveness::Online) => Pill::new("Online", Mark::Passed),
        Some(Liveness::Stale) => Pill::new("Stale", Mark::Stale),
        Some(Liveness::Disconnected) => Pill::new("Disconnected", Mark::Failed),
        Some(Liveness::Completed) => Pill::new("Completed", Mark::End),
        None => Pill::new("Unknown", Mark::Unknown),
    }
}

/// An attempt's overall state from its jobs, while the pipeline verdict is still this one.
#[must_use]
pub fn attempt_pill(jobs: &JobCounts) -> Pill {
    if jobs.starting + jobs.running + jobs.finalizing > 0 {
        Pill::new("Running", Mark::Running)
    } else if jobs.pending > 0 {
        Pill::new("Not started", Mark::NotStarted)
    } else if jobs.closed > 0 {
        Pill::new("Completed", Mark::End)
    } else {
        Pill::new("Not started", Mark::NotStarted)
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

    fn pill(label: &'static str, mark: Mark) -> Pill {
        Pill { label, mark }
    }

    #[test]
    fn flow_states_are_title_case_and_an_unknown_state_is_unknown() {
        let cases = [
            ("pending", pill("Not started", Mark::NotStarted)),
            ("running", pill("Running", Mark::Running)),
            ("passed", pill("Passed", Mark::Passed)),
            ("failed", pill("Failed", Mark::Failed)),
            ("skipped", pill("Skipped", Mark::Skipped)),
            ("interrupted", pill("Interrupted", Mark::Interrupted)),
            ("unknown", pill("Unknown", Mark::Unknown)),
            ("from-the-future", pill("Unknown", Mark::Unknown)),
        ];
        for (state, expected) in cases {
            assert_eq!(flow_state_pill(state), expected, "{state}");
        }
    }

    #[test]
    fn a_job_shows_its_outcome_once_closed_and_its_lifecycle_before() {
        assert_eq!(job_pill("running", None), pill("Running", Mark::Running));
        assert_eq!(job_pill("finalizing", None), pill("Running", Mark::Running));
        assert_eq!(job_pill("starting", None), pill("Running", Mark::Running));
        assert_eq!(
            job_pill("closed", Some("completed")),
            pill("Completed", Mark::End)
        );
        assert_eq!(
            job_pill("closed", Some("failed")),
            pill("Failed", Mark::Failed)
        );
        assert_eq!(
            job_pill("closed", Some("cancelled")),
            pill("Interrupted", Mark::Interrupted)
        );
        assert_eq!(
            job_pill("closed", Some("interrupted")),
            pill("Interrupted", Mark::Interrupted)
        );
        assert_eq!(
            job_pill("closed", Some("unknown")),
            pill("Unknown", Mark::Unknown)
        );
        assert_eq!(job_pill("closed", None), pill("Completed", Mark::End));
        assert_eq!(
            job_pill("closed", Some("odd")),
            pill("Unknown", Mark::Unknown)
        );
    }

    #[test]
    fn a_completed_job_is_never_drawn_as_a_check() {
        // Invariant I3: "Completed" says the run reached its end, not that the flows passed.
        assert_ne!(job_pill("closed", Some("completed")).mark, Mark::Passed);
    }

    #[test]
    fn a_suite_shows_passed_or_failed_once_closed() {
        assert_eq!(suite_pill("running", None), pill("Running", Mark::Running));
        assert_eq!(
            suite_pill("reporting", None),
            pill("Running", Mark::Running)
        );
        assert_eq!(
            suite_pill("closed", Some("passed")),
            pill("Passed", Mark::Passed)
        );
        assert_eq!(
            suite_pill("closed", Some("failed")),
            pill("Failed", Mark::Failed)
        );
        assert_eq!(
            suite_pill("closed", Some("interrupted")),
            pill("Interrupted", Mark::Interrupted)
        );
        assert_eq!(suite_pill("closed", None), pill("Completed", Mark::End));
    }

    #[test]
    fn liveness_is_spelled_out_and_unknown_when_the_server_did_not_say() {
        let cases = [
            (Some(Liveness::Online), pill("Online", Mark::Passed)),
            (Some(Liveness::Stale), pill("Stale", Mark::Stale)),
            (
                Some(Liveness::Disconnected),
                pill("Disconnected", Mark::Failed),
            ),
            (Some(Liveness::Completed), pill("Completed", Mark::End)),
            (None, pill("Unknown", Mark::Unknown)),
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
            pill("Not started", Mark::NotStarted)
        );
        assert_eq!(
            attempt_pill(&jobs(1, 0, 0, 0, 0)),
            pill("Not started", Mark::NotStarted)
        );
        assert_eq!(
            attempt_pill(&jobs(1, 0, 1, 0, 2)),
            pill("Running", Mark::Running)
        );
        assert_eq!(
            attempt_pill(&jobs(0, 1, 0, 0, 0)),
            pill("Running", Mark::Running)
        );
        assert_eq!(
            attempt_pill(&jobs(0, 0, 0, 1, 3)),
            pill("Running", Mark::Running)
        );
        assert_eq!(
            attempt_pill(&jobs(0, 0, 0, 0, 3)),
            pill("Completed", Mark::End)
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

    #[test]
    fn every_status_word_is_in_the_one_vocabulary() {
        // The redesign's words (issue #149), with Completed, which a job or suite ends with, and
        // the agent's liveness words, which name a runner rather than a status.
        const VOCABULARY: [&str; 9] = [
            "Passed",
            "Failed",
            "Running",
            "Not started",
            "Interrupted",
            "Skipped",
            "Unknown",
            "Completed",
            "No failures",
        ];
        const LIVENESS: [&str; 3] = ["Online", "Stale", "Disconnected"];
        let mut words = Vec::new();
        for state in [
            "pending",
            "running",
            "passed",
            "failed",
            "skipped",
            "interrupted",
            "x",
        ] {
            words.push(flow_state_pill(state).label);
        }
        let outcomes = [
            None,
            Some("completed"),
            Some("failed"),
            Some("cancelled"),
            Some("interrupted"),
            Some("x"),
        ];
        for lifecycle in [
            "starting",
            "running",
            "finalizing",
            "reporting",
            "closed",
            "x",
        ] {
            for outcome in outcomes {
                words.push(job_pill(lifecycle, outcome).label);
                words.push(suite_pill(lifecycle, outcome).label);
            }
        }
        let liveness = [
            None,
            Some(Liveness::Online),
            Some(Liveness::Stale),
            Some(Liveness::Disconnected),
            Some(Liveness::Completed),
        ];
        for live in liveness {
            words.push(liveness_pill(live).label);
        }
        let jobs = |pending, starting, running, finalizing, closed| JobCounts {
            pending,
            starting,
            running,
            finalizing,
            closed,
        };
        for counts in [
            jobs(1, 0, 0, 0, 0),
            jobs(0, 0, 0, 0, 3),
            jobs(0, 0, 1, 0, 2),
        ] {
            words.push(attempt_pill(&counts).label);
        }
        for word in words {
            assert!(
                VOCABULARY.contains(&word) || LIVENESS.contains(&word),
                "{word} is not in the vocabulary"
            );
        }
    }
}
