// SPDX-License-Identifier: AGPL-3.0-only
//! Maestro knowledge (PLAN §6.3–§6.4): parsers for what Maestro writes, the invocation the
//! agent prepares, and exit classification. Built from captured fixtures only (PLAN §0.6,
//! `fixtures/maestro/`); sans-I/O.
//!
//! Version profile: Maestro 2.12 (fixtures 2.12.1; 2.11 data agrees, research 08 §1).
#![warn(
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::expect_used,
    clippy::panic
)]

pub mod bundle;
pub mod crash;
pub mod invocation;
pub mod junit;
pub mod log;
pub mod logcat;
pub mod redact;
pub mod reproduce;
pub mod stdout;
pub mod workspace;

use fleet_protocol::event::ExitClass;

/// The Maestro version profile these parsers implement, for provenance (`maestro-log@2.12`).
pub const PROFILE: &str = "2.12";

/// Whether `version`, as `maestro --version` prints it, is in [`PROFILE`]: the same `major.minor`
/// whatever the patch level.
#[must_use]
pub fn supports(version: &str) -> bool {
    version == PROFILE
        || version
            .strip_prefix(PROFILE)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Whether a JUnit report was expected from the run, and whether it showed up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportState {
    /// The report was written.
    Present,
    /// A report was expected (the agent added it, or the user asked for JUnit) but isn't there.
    Absent,
    /// No report was requested: Maestro ran as the user wrote it (for example one flow, or `-c`).
    NotRequested,
}

/// Classifies how Maestro ended (research 08 §7).
///
/// Exit 1 is `TestFailures` when the report is present or a flow was seen to fail by direct
/// observation (`flow_failed_observed`: stdout, bundle or log, not the report). Without either, it
/// is `RunnerError` if a report was expected and is missing, else `Other`: with no report asked
/// for, Fleet can't tell a failing test from an infrastructure error.
#[must_use]
pub fn exit_class(
    code: Option<i32>,
    signal: Option<i32>,
    report: ReportState,
    flow_failed_observed: bool,
) -> ExitClass {
    match (code, signal) {
        (_, Some(_)) | (Some(130 | 143 | 137), _) => ExitClass::Interrupted,
        (Some(0), _) => ExitClass::Passed,
        (Some(1), _) if report == ReportState::Present || flow_failed_observed => {
            ExitClass::TestFailures
        }
        (Some(1), _) if report == ReportState::Absent => ExitClass::RunnerError,
        (Some(2), _) => ExitClass::UsageError,
        _ => ExitClass::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_is_supported_by_its_major_minor_profile_only() {
        assert!(supports("2.12.1"));
        assert!(supports("2.12.0-rc1"));
        assert!(supports("2.12"));
        assert!(!supports("2.120.0"));
        assert!(!supports("2.13.0"));
        assert!(!supports("2.1"));
        assert!(!supports(""));
    }

    #[test]
    fn exit_classes() {
        use ReportState::{Absent, NotRequested, Present};
        assert_eq!(exit_class(Some(0), None, Present, false), ExitClass::Passed);
        assert_eq!(
            exit_class(Some(0), None, NotRequested, false),
            ExitClass::Passed
        );
        // Exit 1: the report or an observed failure says test failures.
        assert_eq!(
            exit_class(Some(1), None, Present, false),
            ExitClass::TestFailures
        );
        assert_eq!(
            exit_class(Some(1), None, Present, true),
            ExitClass::TestFailures
        );
        assert_eq!(
            exit_class(Some(1), None, Absent, true),
            ExitClass::TestFailures
        );
        assert_eq!(
            exit_class(Some(1), None, NotRequested, true),
            ExitClass::TestFailures
        );
        // No failure seen: a missing expected report is an error, no report by choice is unknown.
        assert_eq!(
            exit_class(Some(1), None, Absent, false),
            ExitClass::RunnerError
        );
        assert_eq!(
            exit_class(Some(1), None, NotRequested, false),
            ExitClass::Other
        );
        assert_eq!(
            exit_class(Some(2), None, Absent, false),
            ExitClass::UsageError
        );
        assert_eq!(
            exit_class(Some(143), None, Absent, false),
            ExitClass::Interrupted
        );
        assert_eq!(
            exit_class(None, Some(9), Absent, true),
            ExitClass::Interrupted
        );
    }
}
