// SPDX-License-Identifier: AGPL-3.0-only
//! REST read models of screens E (history) and F (compare) (spec §8; PLAN §10.3; ADR-0009).
//!
//! History is thin SQL over the facts the ingest path already stores: each execution carries
//! the outcome with its source and finality (invariant I4), its failure fingerprint and its
//! condition fingerprint. A flow's flakiness is the verdict of `fleet_core::flaky` and names the
//! runs behind it. Compare explains a difference between two attempts only from what the two
//! sides' evidence says: a changed condition, a different failure, a flow present on one side
//! only, a weaker provenance. Nothing here is a guess.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    frame::RunOrigin,
    live::AttemptView,
    rest::{FlowCounts, InfrastructureView, ReproCommandView},
};

/// Whether a failure fingerprint is known (tracked by an issue) or resolved (issue #84). A
/// fingerprint with no state is untriaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum TriageState {
    /// Tracked by an issue, named by `issue_url`.
    Known,
    /// Fixed; `issue_url` may name the change that fixed it.
    Resolved,
}

/// What a maintainer recorded about one failure fingerprint. Every fingerprint has one, from the
/// first time it was seen, so `first_seen_at` is always known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FailureTriageView {
    /// `v1:` and 16 hex digits.
    pub fingerprint: String,
    /// `known` or `resolved`; `None` while untriaged.
    pub state: Option<TriageState>,
    /// The issue (or change) the state points at.
    pub issue_url: Option<String>,
    /// When the fingerprint was first seen in this project.
    #[serde(with = "time::serde::rfc3339")]
    pub first_seen_at: OffsetDateTime,
    /// When the state last changed; `None` while untriaged.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
}

/// The body of `PUT .../failures/{fingerprint}/triage`. Clearing the triage is a `DELETE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FailureTriageRequest {
    /// The new state.
    pub state: TriageState,
    /// The issue URL (`http` or `https`). Required for `known`.
    #[serde(default)]
    pub issue_url: Option<String>,
}

/// How flaky a flow is, by the rules of ADR-0009 (`fleet_core::flaky::Flakiness`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Flakiness {
    /// Nothing in the history says it is.
    NotFlaky,
    /// Its recent default-branch results flip often.
    Suspected,
    /// It passed and failed on one commit under one condition.
    Confirmed,
}

/// The runs behind a flakiness verdict, so the page can link to them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlakyEvidenceView {
    /// One commit and condition with both outcomes.
    PassedAndFailed {
        /// The commit.
        commit: String,
        /// The condition fingerprint, lowercase hex.
        condition: String,
        /// The passing flow executions, oldest first.
        passed: Vec<Uuid>,
        /// The failing flow executions, oldest first.
        failed: Vec<Uuid>,
    },
    /// A high flip rate over the latest default-branch runs of one condition.
    Flips {
        /// The condition fingerprint, lowercase hex.
        condition: String,
        /// The counted flow executions, oldest first.
        runs: Vec<Uuid>,
        /// How many consecutive runs differ in outcome.
        flips: u32,
    },
}

/// One flow's flakiness verdict and its evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FlowFlakinessView {
    /// The flow's identity: its repository path when known, else its name.
    pub flow: String,
    /// The verdict.
    pub flakiness: Flakiness,
    /// What supports it; empty for [`Flakiness::NotFlaky`].
    pub evidence: Vec<FlakyEvidenceView>,
    /// How many of its failed runs were excluded because a lost device caused them, so they
    /// never count (ADR-0009). Older servers don't send it: it reads as 0.
    #[serde(default)]
    pub excluded: i64,
}

/// One finished flow execution in the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct HistoryEntryView {
    /// The flow execution (screen D's address).
    pub flow_id: Uuid,
    /// The flow's identity: its repository path when known, else its name.
    pub flow: String,
    /// The flow's name as Maestro reported it.
    pub name: String,
    /// The pipeline attempt it ran in.
    pub attempt_id: Uuid,
    /// The repository.
    pub repository: Option<String>,
    /// The commit that was tested, when the CI said.
    pub commit_sha: Option<String>,
    /// The git ref, when the CI said.
    pub git_ref: Option<String>,
    /// `passed`, `failed`, `skipped`, `interrupted`, `unknown`.
    pub state: String,
    /// The winning claim's source; `None` while no claim exists.
    pub outcome_source: Option<String>,
    /// `provisional`, `verified`, `unverified_final`, `inferred`.
    pub finality: String,
    /// When it ended, or started when it never reported an end.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// Maestro's or the report's duration.
    pub duration_ms: Option<i64>,
    /// The origin of the agent run that ran the flow: a published run is imported, not live.
    #[serde(default)]
    pub origin: RunOrigin,
    /// The failure message, redacted.
    pub failure_message: Option<String>,
    /// `v1:` and 16 hex digits; set on a final failure.
    pub failure_fingerprint: Option<String>,
    /// The condition fingerprint, lowercase hex; set on a final pass or failure.
    pub condition_fingerprint: Option<String>,
    /// Set when the failure is the device's (the device was lost, issue #145). The row still
    /// reads as failed, and the flow's flakiness leaves it out.
    #[serde(default)]
    pub infrastructure: Option<InfrastructureView>,
    /// What was recorded about the failure fingerprint, on a final failure.
    #[serde(default)]
    pub triage: Option<FailureTriageView>,
    /// How to rerun this flow (issue #82). Absent from older servers: then there is no command.
    #[serde(default)]
    pub reproduction: ReproCommandView,
}

/// A page of the history, newest first, with the flakiness of the flows on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct HistoryPage {
    /// Start of the window, inclusive: the requested one, or the default.
    #[serde(with = "time::serde::rfc3339")]
    pub from: OffsetDateTime,
    /// End of the window, exclusive.
    #[serde(with = "time::serde::rfc3339")]
    pub to: OffsetDateTime,
    /// The executions.
    pub items: Vec<HistoryEntryView>,
    /// Pass this as `cursor` for the next page; `None` on the last one.
    pub next_cursor: Option<String>,
    /// One verdict per flow on this page, in flow order.
    pub flows: Vec<FlowFlakinessView>,
}

/// How a flow's executions in a window ended. Only settled results (`verified` or
/// `unverified_final`) count as passed or failed (invariants I3 and I4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OutcomeCounts {
    /// Settled passes.
    pub passed: i64,
    /// Settled failures.
    pub failed: i64,
    /// Everything else that ran: provisional or inferred results, skipped, interrupted, unknown.
    pub other: i64,
}

/// How long a flow takes when it passes. No sample is `None`, never zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DurationSummary {
    /// Settled passes that reported a duration.
    pub samples: i64,
    /// The median, in milliseconds.
    pub p50_ms: Option<i64>,
    /// The 90th percentile, in milliseconds; interpolated between neighbouring samples.
    pub p90_ms: Option<i64>,
}

/// One point of the duration chart: a settled execution that reported a duration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DurationPoint {
    /// The flow execution.
    pub flow_id: Uuid,
    /// When it ended.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// How long it took, in milliseconds.
    pub duration_ms: i64,
    /// `passed` or `failed`.
    pub state: String,
}

/// Failures that are the same failure: one fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FailureGroupView {
    /// `v1:` and 16 hex digits.
    pub fingerprint: String,
    /// The step the failure happened in (its description), or empty when no step failed.
    pub step_kind: String,
    /// The failure message with its volatile parts (serials, timings, ids) replaced.
    pub message: String,
    /// How many executions failed this way in the window.
    pub count: i64,
    /// The first of them.
    #[serde(with = "time::serde::rfc3339")]
    pub first_at: OffsetDateTime,
    /// The latest of them.
    #[serde(with = "time::serde::rfc3339")]
    pub last_at: OffsetDateTime,
    /// Up to five of them, newest first.
    pub flow_ids: Vec<Uuid>,
    /// What was recorded about this fingerprint; `None` only from an older server.
    #[serde(default)]
    pub triage: Option<FailureTriageView>,
}

/// Screen E's per-flow panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FlowStatsView {
    /// The flow's identity: its repository path when known, else its name.
    pub flow: String,
    /// Start of the window, inclusive.
    #[serde(with = "time::serde::rfc3339")]
    pub from: OffsetDateTime,
    /// End of the window, exclusive.
    #[serde(with = "time::serde::rfc3339")]
    pub to: OffsetDateTime,
    /// How its executions ended.
    pub counts: OutcomeCounts,
    /// Its flakiness at the end of the window, with the runs behind the verdict.
    pub flakiness: FlowFlakinessView,
    /// How many commits it both passed and failed on under one condition.
    pub flaky_commits: i64,
    /// How long it takes when it passes.
    pub duration: DurationSummary,
    /// The latest 200 settled executions with a duration, oldest first.
    pub durations: Vec<DurationPoint>,
    /// The ten most recent kinds of failure, latest first.
    pub recent_failures: Vec<FailureGroupView>,
}

/// Which side of a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The first attempt, `a`.
    A,
    /// The second attempt, `b`.
    B,
}

/// A flow as one side of a comparison ran it: its latest execution in that attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CompareFlowView {
    /// The flow execution (screen D's address).
    pub flow_id: Uuid,
    /// The flow's name as Maestro reported it.
    pub name: String,
    /// `pending`, `running`, `passed`, `failed`, `skipped`, `interrupted`, `unknown`.
    pub state: String,
    /// The winning claim's source; `None` while no claim exists.
    pub outcome_source: Option<String>,
    /// `provisional`, `verified`, `unverified_final`, `inferred`.
    pub finality: String,
    /// Maestro's or the report's duration.
    pub duration_ms: Option<i64>,
    /// How many times the attempt executed the flow; the latest is shown.
    pub executions: i64,
    /// The condition fingerprint, lowercase hex; set on a final pass or failure.
    pub condition_fingerprint: Option<String>,
    /// `v1:` and 16 hex digits; set on a final failure.
    pub failure_fingerprint: Option<String>,
    /// The failure message, redacted.
    pub failure_message: Option<String>,
}

/// One condition dimension that differs between two sides (`unknown` where a side didn't report
/// it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DimChangeView {
    /// The dimension: `arch`, `os`, `device_model`, `device_api_level`, `host_memory`, …
    pub dim: String,
    /// Its value on side A.
    pub a: String,
    /// Its value on side B.
    pub b: String,
}

/// How settled an outcome is and where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EvidenceView {
    /// The winning claim's source; `None` while no claim exists.
    pub outcome_source: Option<String>,
    /// `provisional`, `verified`, `unverified_final`, `inferred`.
    pub finality: String,
}

/// Why two sides differ, drawn from what their evidence says and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Explanation {
    /// One attempt never executed the flow.
    FlowMissing {
        /// The side that has no execution.
        side: Side,
    },
    /// The two executions ran under different conditions.
    ConditionChanged {
        /// Side A's condition fingerprint.
        a: String,
        /// Side B's condition fingerprint.
        b: String,
        /// The dimensions that differ.
        changes: Vec<DimChangeView>,
    },
    /// Both failed, but not in the same way.
    FailureChanged {
        /// Side A's failure fingerprint.
        a: String,
        /// Side B's failure fingerprint.
        b: String,
    },
    /// The two outcomes rest on different evidence: one may be provisional or inferred.
    ProvenanceDiffers {
        /// Side A's evidence.
        a: EvidenceView,
        /// Side B's evidence.
        b: EvidenceView,
    },
}

/// What happened to a flow between A and B.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// The same state on both sides.
    Unchanged,
    /// Passed in A, failed in B.
    NewlyFailing,
    /// Failed in A, passed in B.
    Recovered,
    /// Any other difference of state (for example passed in A, interrupted in B).
    Changed,
    /// Executed in A, never in B.
    OnlyInA,
    /// Executed in B, never in A.
    OnlyInB,
}

/// One flow, A against B.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CompareRowView {
    /// The flow's identity: its repository path when known, else its name.
    pub flow: String,
    /// What happened to it.
    pub change: ChangeKind,
    /// Its latest execution in A; `None` when A never ran it.
    pub a: Option<CompareFlowView>,
    /// Its latest execution in B; `None` when B never ran it.
    pub b: Option<CompareFlowView>,
    /// Why the sides differ, from evidence only; empty when nothing the evidence says differs.
    pub explanations: Vec<Explanation>,
}

/// One compared attempt and how its flows stand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CompareSideView {
    /// The attempt.
    pub attempt: AttemptView,
    /// Its distinct flows, by the state of their latest execution.
    pub flows: FlowCounts,
}

/// Screen F: two attempts compared flow by flow, sorted by flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CompareView {
    /// The first attempt.
    pub a: CompareSideView,
    /// The second attempt.
    pub b: CompareSideView,
    /// Every flow either side executed.
    pub rows: Vec<CompareRowView>,
    /// A side has more than 5 000 flows: the rows are incomplete.
    pub truncated: bool,
}
