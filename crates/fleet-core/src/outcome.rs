// SPDX-License-Identifier: AGPL-3.0-only
//! Suite and job outcomes (research 05 §6.3, §6.4; invariant I3, spec acceptance criterion 15).
//!
//! Pure functions over what is known about a suite or a job. The rule they share: nothing reads
//! as successful unless Maestro's own exit status said so and no evidence contradicts it. A job
//! that was cancelled, abandoned, interrupted or failed can never come out `completed`.

use fleet_protocol::{
    event::{ExitClass, FlowOutcome},
    journal::SourceKind,
};

use crate::claims::FlowState;

/// A suite's (one Maestro invocation's) outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuiteOutcome {
    /// Maestro exited 0 and no flow failed or was interrupted.
    Passed,
    /// A flow failed, or Maestro exited non-zero for any reason other than a signal.
    Failed,
    /// Maestro was killed or cut short by a signal.
    Interrupted,
    /// Nothing reliable is known (for example no exit was observed).
    Unknown,
}

/// What is known about one suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuiteFacts<'a> {
    /// Maestro's exit code, when it exited normally.
    pub exit_code: Option<i32>,
    /// How the exit reads, when Maestro exited at all.
    pub exit_class: Option<ExitClass>,
    /// Whether the final report was parsed completely.
    pub report_complete: bool,
    /// The resolved state of each of its flow executions.
    pub flows: &'a [FlowState],
}

/// Decides a suite's outcome and the source it rests on. `None` while Maestro hasn't exited.
#[must_use]
pub fn suite_outcome(facts: &SuiteFacts<'_>) -> Option<(SuiteOutcome, SourceKind)> {
    let class = facts.exit_class?;
    let report_source = if facts.report_complete {
        SourceKind::JunitFinal
    } else {
        SourceKind::ProcessExit
    };
    let any = |o: FlowOutcome| facts.flows.contains(&FlowState::Ended(o));
    let outcome = match class {
        ExitClass::Interrupted => (SuiteOutcome::Interrupted, SourceKind::ProcessExit),
        _ if any(FlowOutcome::Failed) => (SuiteOutcome::Failed, report_source),
        _ if any(FlowOutcome::Interrupted) => (SuiteOutcome::Interrupted, report_source),
        ExitClass::Passed if facts.exit_code == Some(0) => (SuiteOutcome::Passed, report_source),
        ExitClass::Passed => (SuiteOutcome::Unknown, SourceKind::ProcessExit),
        ExitClass::TestFailures
        | ExitClass::MaestroError
        | ExitClass::UsageError
        | ExitClass::Other => (SuiteOutcome::Failed, SourceKind::ProcessExit),
    };
    Some(outcome)
}

/// A CI job's Maestro result (not the CI job's own result: later steps are invisible to Fleet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    /// Every suite passed and every agent run ended normally.
    Completed,
    /// A suite failed.
    Failed,
    /// CI cancelled the job (the agent saw SIGINT or SIGTERM).
    Cancelled,
    /// Cut short: a suite was interrupted, or an agent vanished while Maestro ran.
    Interrupted,
    /// Nothing reliable is known.
    Unknown,
}

/// Where one agent run of the job stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Connected or briefly away; the run may still send evidence.
    Open,
    /// Maestro exited; the agent is reconciling and flushing.
    Finalizing,
    /// `run.ended` was committed.
    Ended,
    /// The agent never came back within the resume grace period.
    Abandoned,
}

/// What is known about one agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunFacts {
    /// Its state.
    pub state: RunState,
    /// The agent forwarded a cancellation signal (SIGINT or SIGTERM).
    pub cancelled: bool,
    /// A suite of this run was still open at the last contact.
    pub suite_open: bool,
}

/// A job's lifecycle (research 05 §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobLifecycle {
    /// An agent connected; no suite yet.
    Starting,
    /// A suite started.
    Running,
    /// Every run is done or finalizing, and at least one is finalizing.
    Finalizing,
    /// Every run ended or was abandoned. A new run reopens the job.
    Closed,
}

/// Decides a job's lifecycle, and once it is closed its outcome and the source that outcome
/// rests on.
#[must_use]
pub fn job_state(
    runs: &[RunFacts],
    suites: &[Option<SuiteOutcome>],
) -> (JobLifecycle, Option<(JobOutcome, SourceKind)>) {
    let done = |r: &RunFacts| matches!(r.state, RunState::Ended | RunState::Abandoned);
    let lifecycle = if !runs.is_empty() && runs.iter().all(done) {
        JobLifecycle::Closed
    } else if !runs.is_empty()
        && runs
            .iter()
            .all(|r| done(r) || r.state == RunState::Finalizing)
    {
        JobLifecycle::Finalizing
    } else if suites.is_empty() {
        JobLifecycle::Starting
    } else {
        JobLifecycle::Running
    };
    if lifecycle != JobLifecycle::Closed {
        return (lifecycle, None);
    }
    (lifecycle, Some(job_outcome(runs, suites)))
}

fn job_outcome(runs: &[RunFacts], suites: &[Option<SuiteOutcome>]) -> (JobOutcome, SourceKind) {
    if runs
        .iter()
        .any(|r| r.cancelled && r.state == RunState::Ended)
    {
        return (JobOutcome::Cancelled, SourceKind::AgentObserved);
    }
    let abandoned = || runs.iter().filter(|r| r.state == RunState::Abandoned);
    if abandoned().any(|r| r.suite_open) {
        return (JobOutcome::Interrupted, SourceKind::ServerInferred);
    }
    if abandoned().next().is_some() {
        return (JobOutcome::Unknown, SourceKind::ServerInferred);
    }
    if suites.contains(&Some(SuiteOutcome::Failed)) {
        return (JobOutcome::Failed, SourceKind::ProcessExit);
    }
    if suites.contains(&Some(SuiteOutcome::Interrupted)) {
        return (JobOutcome::Interrupted, SourceKind::ProcessExit);
    }
    if !suites.is_empty() && suites.iter().all(|s| *s == Some(SuiteOutcome::Passed)) {
        return (JobOutcome::Completed, SourceKind::ProcessExit);
    }
    (JobOutcome::Unknown, SourceKind::ServerInferred)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn facts(code: Option<i32>, class: ExitClass, flows: &[FlowState]) -> SuiteFacts<'_> {
        SuiteFacts {
            exit_code: code,
            exit_class: Some(class),
            report_complete: true,
            flows,
        }
    }

    #[test]
    fn a_failed_flow_fails_the_suite_even_with_exit_zero() {
        let flows = [
            FlowState::Ended(FlowOutcome::Passed),
            FlowState::Ended(FlowOutcome::Failed),
        ];
        assert_eq!(
            suite_outcome(&facts(Some(0), ExitClass::Passed, &flows)).map(|o| o.0),
            Some(SuiteOutcome::Failed)
        );
    }

    #[test]
    fn no_exit_means_no_outcome_and_signals_interrupt() {
        let f = SuiteFacts {
            exit_code: None,
            exit_class: None,
            report_complete: false,
            flows: &[],
        };
        assert_eq!(suite_outcome(&f), None);
        assert_eq!(
            suite_outcome(&facts(Some(143), ExitClass::Interrupted, &[])),
            Some((SuiteOutcome::Interrupted, SourceKind::ProcessExit))
        );
    }

    #[test]
    fn a_job_closes_only_when_every_run_is_done() {
        let open = RunFacts {
            state: RunState::Open,
            cancelled: false,
            suite_open: true,
        };
        let ended = RunFacts {
            state: RunState::Ended,
            ..open
        };
        assert_eq!(
            job_state(&[open, ended], &[Some(SuiteOutcome::Passed)]).0,
            JobLifecycle::Running
        );
        assert_eq!(
            job_state(&[ended, ended], &[Some(SuiteOutcome::Passed)]),
            (
                JobLifecycle::Closed,
                Some((JobOutcome::Completed, SourceKind::ProcessExit))
            )
        );
        assert_eq!(
            job_state(&[ended], &[]).1.map(|o| o.0),
            Some(JobOutcome::Unknown),
            "no suite is not success"
        );
    }

    fn any_flow() -> impl Strategy<Value = FlowState> {
        prop::sample::select(vec![
            FlowState::Pending,
            FlowState::Running,
            FlowState::Ended(FlowOutcome::Passed),
            FlowState::Ended(FlowOutcome::Failed),
            FlowState::Ended(FlowOutcome::Skipped),
            FlowState::Ended(FlowOutcome::Interrupted),
            FlowState::Ended(FlowOutcome::Unknown),
        ])
    }

    fn any_class() -> impl Strategy<Value = ExitClass> {
        prop::sample::select(vec![
            ExitClass::Passed,
            ExitClass::TestFailures,
            ExitClass::MaestroError,
            ExitClass::UsageError,
            ExitClass::Interrupted,
            ExitClass::Other,
        ])
    }

    fn any_run() -> impl Strategy<Value = RunFacts> {
        (
            prop::sample::select(vec![
                RunState::Open,
                RunState::Finalizing,
                RunState::Ended,
                RunState::Abandoned,
            ]),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(state, cancelled, suite_open)| RunFacts {
                state,
                cancelled,
                suite_open,
            })
    }

    fn any_suite() -> impl Strategy<Value = Option<SuiteOutcome>> {
        prop::sample::select(vec![
            None,
            Some(SuiteOutcome::Passed),
            Some(SuiteOutcome::Failed),
            Some(SuiteOutcome::Interrupted),
            Some(SuiteOutcome::Unknown),
        ])
    }

    proptest! {
        /// Criterion 15 at suite level: passed needs exit 0 and no failed or interrupted flow.
        #[test]
        fn a_suite_passes_only_on_exit_zero_without_contrary_evidence(
            code in prop::option::of(-1i32..200), class in any_class(),
            flows in prop::collection::vec(any_flow(), 0..8), complete in any::<bool>()
        ) {
            let f = SuiteFacts { exit_code: code, exit_class: Some(class), report_complete: complete, flows: &flows };
            if let Some((SuiteOutcome::Passed, source)) = suite_outcome(&f) {
                prop_assert_eq!(code, Some(0));
                prop_assert_eq!(class, ExitClass::Passed);
                prop_assert!(!flows.contains(&FlowState::Ended(FlowOutcome::Failed)));
                prop_assert!(!flows.contains(&FlowState::Ended(FlowOutcome::Interrupted)));
                prop_assert_ne!(source, SourceKind::ServerInferred);
            }
        }

        /// Criterion 15 at job level: completed needs every run ended uncancelled and every suite
        /// passed; failed, cancelled and interrupted runs never read as completed.
        #[test]
        fn a_job_completes_only_when_everything_passed(
            runs in prop::collection::vec(any_run(), 0..5),
            suites in prop::collection::vec(any_suite(), 0..5)
        ) {
            let (lifecycle, outcome) = job_state(&runs, &suites);
            prop_assert_eq!(lifecycle == JobLifecycle::Closed, outcome.is_some());
            if let Some((JobOutcome::Completed, _)) = outcome {
                prop_assert!(!suites.is_empty());
                prop_assert!(suites.iter().all(|s| *s == Some(SuiteOutcome::Passed)));
                prop_assert!(runs.iter().all(|r| r.state == RunState::Ended && !r.cancelled));
            }
            if suites.contains(&Some(SuiteOutcome::Failed)) || runs.iter().any(|r| r.state == RunState::Abandoned) {
                prop_assert_ne!(outcome.map(|o| o.0), Some(JobOutcome::Completed));
            }
        }

        /// Order of runs and suites never changes the answer.
        #[test]
        fn job_state_ignores_order(
            runs in prop::collection::vec(any_run(), 0..5),
            suites in prop::collection::vec(any_suite(), 0..5)
        ) {
            let (mut r2, mut s2) = (runs.clone(), suites.clone());
            r2.reverse();
            s2.reverse();
            prop_assert_eq!(job_state(&runs, &suites).0, job_state(&r2, &s2).0);
            prop_assert_eq!(job_state(&runs, &suites).1.map(|o| o.0), job_state(&r2, &s2).1.map(|o| o.0));
        }
    }
}
