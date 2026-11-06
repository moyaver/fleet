// SPDX-License-Identifier: AGPL-3.0-only
//! Read models and frames of the browser's live socket (PLAN §7.3; invariant I4).
//!
//! A snapshot is one pipeline attempt read in a single `REPEATABLE READ` transaction together
//! with the attempt's live `version`, so a client that applies deltas with a higher version
//! never misses or double-applies a change. Every outcome travels with its source and, for
//! flows, its finality: nothing is shown without a provenance.
//!
//! [`LiveFrame`] is what `/api/v1/live` sends and [`LiveRequest`] what it accepts. The
//! dashboard's live store consumes the same types, so both ends cannot drift.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ProjectView {
    /// ID.
    pub id: Uuid,
    /// URL-safe identifier.
    pub slug: String,
    /// Display name.
    pub name: String,
}

/// A pipeline attempt (one CI run attempt) and its live version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AttemptView {
    /// ID.
    pub id: Uuid,
    /// Project.
    pub project_id: Uuid,
    /// CI provider (`github_actions`, `generic`).
    pub provider: String,
    /// The CI run's natural key (`<repository>:<run_id>`).
    pub run_key: String,
    /// `owner/name`.
    pub repository: Option<String>,
    /// Commit.
    pub commit_sha: Option<String>,
    /// Git ref.
    pub git_ref: Option<String>,
    /// Re-run attempt, from 1.
    pub attempt: i32,
    /// Live version: deltas carry the version they produce.
    pub version: i64,
    /// Flows planned by Fleet plans linked to this attempt; `None` when no plan is known.
    pub planned_total: Option<i64>,
    /// Jobs seen.
    pub jobs: i64,
    /// Jobs not closed yet.
    pub open_jobs: i64,
    /// First seen.
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "schema", schemars(schema_with = "crate::schema::rfc3339"))]
    pub first_seen_at: OffsetDateTime,
}

/// A CI job. The outcome is the Maestro result, with its source; `None` until closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct JobView {
    /// ID.
    pub id: Uuid,
    /// Attempt.
    pub pipeline_attempt_id: Uuid,
    /// Stable key within the attempt.
    pub job_key: String,
    /// Display name.
    pub name: Option<String>,
    /// `starting`, `running`, `finalizing`, `closed`.
    pub lifecycle: String,
    /// `completed`, `failed`, `cancelled`, `interrupted`, `unknown`.
    pub outcome: Option<String>,
    /// The evidence the outcome rests on.
    pub outcome_source: Option<String>,
    /// Times a new agent run reopened it.
    pub reopened_count: i32,
    /// Revision, bumped on every change.
    pub rev: i64,
}

/// One agent process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AgentRunView {
    /// ID.
    pub id: Uuid,
    /// Job.
    pub job_id: Uuid,
    /// Worker.
    pub worker_id: Option<Uuid>,
    /// Runner name.
    pub runner_name: Option<String>,
    /// `active`, `detached`, `finalizing`, `ended`, `abandoned`.
    pub state: String,
    /// `trusted`, `pr`, `untrusted`.
    pub trust_level: String,
    /// Highest committed `seq`.
    pub committed_seq: i64,
    /// Explicitly recorded gaps (I5): journal and telemetry ranges known to be missing.
    pub gaps: i64,
    /// Last persisted contact.
    #[serde(with = "time::serde::rfc3339")]
    #[cfg_attr(feature = "schema", schemars(schema_with = "crate::schema::rfc3339"))]
    pub last_frame_at: OffsetDateTime,
    /// It came back after it had been abandoned.
    pub late_resume: bool,
    /// The Fleet plan it runs a shard of.
    pub plan_id: Option<String>,
    /// That shard.
    pub plan_shard_id: Option<i32>,
    /// Maestro's exit code.
    pub maestro_exit_code: Option<i32>,
    /// The Maestro version the run's session log named, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maestro_version: Option<String>,
    /// Whether the agent's parsers were built for that version. Absent until it is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maestro_profile_supported: Option<bool>,
    /// Revision.
    pub rev: i64,
    /// Set by the server from live contact (heartbeats never touch the database).
    pub liveness: Option<Liveness>,
}

/// One Maestro invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SuiteView {
    /// ID.
    pub id: Uuid,
    /// Job.
    pub job_id: Uuid,
    /// Agent run.
    pub agent_run_id: Uuid,
    /// `none`, `split`, `all`.
    pub shard_mode: String,
    /// Shards requested.
    pub shard_count: i32,
    /// Flows planned; `None` = unknown (progress is indeterminate, never guessed).
    pub planned_count: Option<i32>,
    /// Where the plan came from.
    pub plan_source: Option<String>,
    /// `starting`, `running`, `reporting`, `closed`.
    pub lifecycle: String,
    /// `passed`, `failed`, `interrupted`, `unknown`.
    pub outcome: Option<String>,
    /// The evidence the outcome rests on.
    pub outcome_source: Option<String>,
    /// `complete`, `partial`, `absent`, `invalid`.
    pub report_status: Option<String>,
    /// Maestro's exit code.
    pub exit_code: Option<i32>,
    /// Revision.
    pub rev: i64,
}

/// A device as seen by one agent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeviceView {
    /// ID.
    pub id: Uuid,
    /// Agent run.
    pub agent_run_id: Uuid,
    /// adb serial.
    pub serial: String,
    /// `discovered`, `booting`, `ready`, `busy`, `offline`, `released`, `lost`.
    pub state: String,
    /// Revision.
    pub rev: i64,
}

/// One flow execution: state, provenance and finality together (I4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FlowView {
    /// ID.
    pub id: Uuid,
    /// Job.
    pub job_id: Uuid,
    /// Suite.
    pub suite_execution_id: Uuid,
    /// Agent run.
    pub agent_run_id: Uuid,
    /// Device.
    pub device_session_id: Option<Uuid>,
    /// 1-based shard.
    pub shard_number: Option<i32>,
    /// Order within the shard.
    pub ordinal: Option<i32>,
    /// Flow name.
    pub name: String,
    /// Flow file.
    pub path: Option<String>,
    /// `pending`, `running`, `passed`, `failed`, `skipped`, `interrupted`, `unknown`.
    pub state: String,
    /// The winning claim's source.
    pub outcome_source: Option<String>,
    /// `provisional`, `verified`, `unverified_final`, `inferred`.
    pub finality: String,
    /// Two parsed sources disagreed (a Maestro compatibility alarm).
    pub has_conflict: bool,
    /// Why it failed, redacted.
    pub failure_message: Option<String>,
    /// Maestro's or the report's duration.
    pub duration_ms: Option<i64>,
    /// Steps seen.
    pub steps: i64,
    /// Revision.
    pub rev: i64,
}

/// One shard of a Fleet plan: planned against actual.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PlanShardView {
    /// Shard number.
    pub shard_id: i32,
    /// Flow files assigned.
    pub flows: Vec<String>,
    /// The planner's estimate.
    pub estimate_ms: Option<i64>,
    /// Agent runs that ran it.
    pub agent_runs: Vec<Uuid>,
    /// Flow executions those runs reported.
    pub executed: i64,
    /// Wall-clock span of the shard's flows: first start to last end. `None` until every flow
    /// that started has ended, and when none started.
    #[serde(default)]
    pub actual_ms: Option<i64>,
}

/// A Fleet plan (ADR-0013).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PlanView {
    /// Plan ID.
    pub plan_id: String,
    /// Flows planned across shards.
    pub planned_total: i32,
    /// The largest shard's estimate had the flows been split by count alone, in ms; `None` for
    /// plans published before the planner recorded it.
    #[serde(default)]
    pub equal_count_estimate_ms: Option<i64>,
    /// The shards.
    pub shards: Vec<PlanShardView>,
}

/// Everything about one attempt, at `attempt.version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Snapshot {
    /// The attempt and its version.
    pub attempt: AttemptView,
    /// Jobs.
    pub jobs: Vec<JobView>,
    /// Agent runs.
    pub agent_runs: Vec<AgentRunView>,
    /// Suites.
    pub suites: Vec<SuiteView>,
    /// Devices.
    pub devices: Vec<DeviceView>,
    /// Flow executions.
    pub flows: Vec<FlowView>,
    /// Linked Fleet plans.
    pub plans: Vec<PlanView>,
}

/// One changed entity, as a delta carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "kind", content = "entity", rename_all = "snake_case")]
pub enum EntityView {
    /// A job.
    Job(JobView),
    /// An agent run.
    AgentRun(AgentRunView),
    /// A suite.
    Suite(SuiteView),
    /// A flow execution.
    Flow(FlowView),
    /// A device.
    Device(DeviceView),
}

/// A run's display liveness. Heartbeats never touch the database, so the server derives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    /// Frames are arriving.
    Online,
    /// Nothing heard for longer than the stale threshold; the run may still be alive.
    Stale,
    /// The agent's socket is closed and the run has not ended.
    Disconnected,
    /// The agent said goodbye.
    Completed,
}

/// Why the server asks a client to fetch snapshots again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ResyncReason {
    /// The client's socket fell behind the project's channel.
    Lagged,
}

/// What the browser socket receives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LiveFrame {
    /// Subscribed to a project; deltas follow.
    Subscribed {
        /// The project.
        project_id: Uuid,
    },
    /// An attempt's full state at `snapshot.attempt.version`.
    Snapshot {
        /// The snapshot.
        snapshot: Box<Snapshot>,
    },
    /// Entities that changed at `version`. Apply only `version == last + 1`; on a gap, resync.
    Delta {
        /// Project.
        project_id: Uuid,
        /// Attempt.
        attempt_id: Uuid,
        /// The attempt's version after this change.
        version: u64,
        /// The changed entities, as they are now.
        entities: Vec<EntityView>,
    },
    /// A run's liveness crossed a threshold (display only, not versioned).
    Liveness {
        /// Attempt.
        attempt_id: Uuid,
        /// Agent run.
        agent_run_id: Uuid,
        /// The new liveness.
        liveness: Liveness,
    },
    /// This subscriber fell behind: fetch snapshots again.
    Resync {
        /// Why.
        reason: ResyncReason,
    },
    /// A subscription request was refused.
    Error {
        /// Why.
        message: String,
    },
}

/// What the browser socket accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LiveRequest {
    /// Subscribe to a project, and to one attempt's snapshot.
    Subscribe {
        /// The project's slug.
        project: String,
        /// The attempt to snapshot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<Uuid>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delta_frame_parses_from_the_documented_wire_form() {
        let wire = r#"{"op":"delta","project_id":"00000000-0000-0000-0000-000000000001",
            "attempt_id":"00000000-0000-0000-0000-000000000002","version":7,
            "entities":[{"kind":"device","entity":{"id":"00000000-0000-0000-0000-000000000003",
            "agent_run_id":"00000000-0000-0000-0000-000000000004","serial":"emulator-5554",
            "state":"ready","rev":2}}]}"#;
        let LiveFrame::Delta {
            version, entities, ..
        } = serde_json::from_str(wire).unwrap()
        else {
            panic!("expected a delta");
        };
        assert_eq!(version, 7);
        assert!(matches!(&entities[..], [EntityView::Device(d)] if d.serial == "emulator-5554"));
    }

    #[test]
    fn liveness_and_resync_use_snake_case_words() {
        let wire = r#"{"op":"liveness","attempt_id":"00000000-0000-0000-0000-000000000002",
            "agent_run_id":"00000000-0000-0000-0000-000000000004","liveness":"disconnected"}"#;
        assert!(matches!(
            serde_json::from_str(wire).unwrap(),
            LiveFrame::Liveness {
                liveness: Liveness::Disconnected,
                ..
            }
        ));
        assert_eq!(
            serde_json::to_string(&LiveFrame::Resync {
                reason: ResyncReason::Lagged
            })
            .unwrap(),
            r#"{"op":"resync","reason":"lagged"}"#
        );
    }
}
