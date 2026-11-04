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
pub mod invocation;
pub mod junit;
pub mod log;
pub mod stdout;

use fleet_protocol::event::ExitClass;

/// The Maestro version profile these parsers implement, for provenance (`maestro-log@2.12`).
pub const PROFILE: &str = "2.12";

/// Classifies how Maestro ended (research 08 §7).
#[must_use]
pub fn exit_class(code: Option<i32>, signal: Option<i32>, report_present: bool) -> ExitClass {
    match (code, signal) {
        (_, Some(_)) | (Some(130 | 143 | 137), _) => ExitClass::Interrupted,
        (Some(0), _) => ExitClass::Passed,
        (Some(1), _) if report_present => ExitClass::TestFailures,
        (Some(1), _) => ExitClass::MaestroError,
        (Some(2), _) => ExitClass::UsageError,
        _ => ExitClass::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_classes() {
        assert_eq!(exit_class(Some(0), None, true), ExitClass::Passed);
        assert_eq!(exit_class(Some(1), None, true), ExitClass::TestFailures);
        assert_eq!(exit_class(Some(1), None, false), ExitClass::MaestroError);
        assert_eq!(exit_class(Some(2), None, false), ExitClass::UsageError);
        assert_eq!(exit_class(Some(143), None, false), ExitClass::Interrupted);
        assert_eq!(exit_class(None, Some(9), false), ExitClass::Interrupted);
    }
}
