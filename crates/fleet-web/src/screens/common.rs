// SPDX-License-Identifier: AGPL-3.0-only
//! Wording and mappings shared by the pages.

use fleet_protocol::journal::SourceKind;
use fleet_ui::{
    charts::format::{format_duration, format_timestamp},
    provenance::{Provenance, STALE_AFTER_MS},
};

/// How fresh a sample taken `age_ms` ago reads on a live page (invariant I4, issue #149): observed
/// while it is young and the page is receiving updates, stale once it is old or while the page's
/// socket is not confirming updates, since a value the page cannot refresh may have changed.
#[must_use]
pub fn sample_freshness(age_ms: u64, updating: bool) -> Provenance {
    if updating {
        Provenance::Observed.displayed(age_ms, STALE_AFTER_MS)
    } else {
        Provenance::Stale
    }
}

/// What a page needs to turn data into text: where it is, when it is, and the viewer's clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ctx {
    /// The project slug of the route, for links to project-scoped pages.
    pub project: String,
    /// The current time, unix ms.
    pub now_ms: i64,
    /// The viewer's offset from UTC in minutes, for time labels.
    pub utc_offset_minutes: i32,
}

impl Ctx {
    /// `2026-01-01 10:00:00` in the viewer's time zone.
    #[must_use]
    pub fn time(&self, unix_ms: i64) -> String {
        format_timestamp(unix_ms, self.utc_offset_minutes)
    }

    /// `just now`, `4.2 s ago`, `3 min ago`: how long before now `unix_ms` was.
    #[must_use]
    pub fn ago(&self, unix_ms: i64) -> String {
        let age = self.now_ms.saturating_sub(unix_ms);
        if age < 1000 {
            "just now".to_owned()
        } else {
            format!("{} ago", format_duration(age))
        }
    }

    /// How old `unix_ms` is, in milliseconds (never negative).
    #[must_use]
    pub fn age_ms(&self, unix_ms: i64) -> u64 {
        u64::try_from(self.now_ms.saturating_sub(unix_ms)).unwrap_or(0)
    }
}

/// A labelled fact. A fact the server doesn't have is `None` and shows as "Not reported", never
/// as a blank or a made-up value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    /// What it is.
    pub label: &'static str,
    /// Its value, when known.
    pub value: Option<String>,
}

impl Fact {
    /// A fact with a known value.
    #[must_use]
    pub fn known(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            value: Some(value.into()),
        }
    }

    /// A fact from an optional value.
    #[must_use]
    pub fn maybe(label: &'static str, value: Option<String>) -> Self {
        Self { label, value }
    }

    /// The text to show.
    #[must_use]
    pub fn shown(&self) -> &str {
        self.value.as_deref().unwrap_or("Not reported")
    }
}

/// What a finality means, for the tooltip next to the badge.
#[must_use]
pub fn finality_note(finality: &str) -> &'static str {
    match finality {
        "verified" => "Confirmed by the final test report or per-flow artifacts",
        "provisional" => "Not final: read from the live run",
        "unverified_final" => "Final, but no test report confirmed it",
        "inferred" => "Inferred by Fleet; it was not observed",
        _ => "Finality unknown",
    }
}

/// The evidence source named by the REST API (`junit_final`, …); an unknown name is
/// [`SourceKind::Unrecognized`], which never counts as evidence of success.
#[must_use]
pub fn parse_source(name: &str) -> SourceKind {
    match name {
        "declared" => SourceKind::Declared,
        "agent_observed" => SourceKind::AgentObserved,
        "process_exit" => SourceKind::ProcessExit,
        "live_console" => SourceKind::LiveConsole,
        "debug_log" => SourceKind::DebugLog,
        "artifact_file" => SourceKind::ArtifactFile,
        "junit_final" => SourceKind::JunitFinal,
        "server_inferred" => SourceKind::ServerInferred,
        _ => SourceKind::Unrecognized,
    }
}

/// What to call an evidence source on screen.
#[must_use]
pub const fn source_label(source: SourceKind) -> &'static str {
    match source {
        SourceKind::Declared => "Declared up front",
        SourceKind::AgentObserved => "Agent observation",
        SourceKind::ProcessExit => "Maestro exit status",
        SourceKind::LiveConsole => "Live console",
        SourceKind::DebugLog => "Debug log",
        SourceKind::ArtifactFile => "Per-flow artifacts",
        SourceKind::JunitFinal => "Final JUnit report",
        SourceKind::ServerInferred => "Server inference",
        SourceKind::Unrecognized => "Unknown source",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finality_is_explained_in_words() {
        assert_eq!(
            finality_note("unverified_final"),
            "Final, but no test report confirmed it"
        );
        assert_eq!(
            finality_note("provisional"),
            "Not final: read from the live run"
        );
    }

    #[test]
    fn source_names_follow_the_journal_vocabulary() {
        assert_eq!(parse_source("junit_final"), SourceKind::JunitFinal);
        assert_eq!(parse_source("live_console"), SourceKind::LiveConsole);
        assert_eq!(parse_source("from-the-future"), SourceKind::Unrecognized);
        assert_eq!(source_label(SourceKind::JunitFinal), "Final JUnit report");
        assert_eq!(source_label(SourceKind::Unrecognized), "Unknown source");
    }
}
