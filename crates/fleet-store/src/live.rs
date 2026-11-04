// SPDX-License-Identifier: AGPL-3.0-only
//! Read models for REST snapshots and live deltas (research 03 §5.3; invariant I4).
//!
//! A snapshot is one pipeline attempt read in a single `REPEATABLE READ` transaction together
//! with the attempt's live `version`, so a client that applies deltas with a higher version
//! never misses or double-applies a change. Every outcome travels with its source and, for
//! flows, its finality: nothing is shown without a provenance.

use std::collections::BTreeMap;

use serde::Serialize;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{Result, Store, ingest::EntityRef};

/// A project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ProjectView {
    /// ID.
    pub id: Uuid,
    /// URL-safe identifier.
    pub slug: String,
    /// Display name.
    pub name: String,
}

/// A pipeline attempt (one CI run attempt) and its live version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
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
    pub first_seen_at: OffsetDateTime,
}

/// A CI job. The outcome is the Maestro result, with its source; `None` until closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
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
    pub last_frame_at: OffsetDateTime,
    /// It came back after it had been abandoned.
    pub late_resume: bool,
    /// The Fleet plan it runs a shard of.
    pub plan_id: Option<String>,
    /// That shard.
    pub plan_shard_id: Option<i32>,
    /// Maestro's exit code.
    pub maestro_exit_code: Option<i32>,
    /// Revision.
    pub rev: i64,
    /// `online`, `stale`, `disconnected`, `completed`: set by the server from live contact
    /// (heartbeats never touch the database).
    pub liveness: Option<String>,
}

/// One Maestro invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
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
}

/// A Fleet plan (ADR-0013).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PlanView {
    /// Plan ID.
    pub plan_id: String,
    /// Flows planned across shards.
    pub planned_total: i32,
    /// The shards.
    pub shards: Vec<PlanShardView>,
}

/// Everything about one attempt, at `attempt.version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
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

/// Rows of one attempt, or the rows with these IDs.
#[derive(Debug, Clone, Copy)]
enum Scope<'a> {
    Attempt(Uuid),
    Ids(&'a [Uuid]),
}

impl Scope<'_> {
    fn parts(&self) -> (Option<Uuid>, Option<Vec<Uuid>>) {
        match self {
            Self::Attempt(a) => (Some(*a), None),
            Self::Ids(ids) => (None, Some(ids.to_vec())),
        }
    }
}

impl Store {
    /// Every project.
    pub async fn projects(&self) -> Result<Vec<ProjectView>> {
        Ok(sqlx::query_as!(
            ProjectView,
            "SELECT id, slug, name FROM projects WHERE archived_at IS NULL ORDER BY slug"
        )
        .fetch_all(self.pool())
        .await?)
    }

    /// A project's most recent attempts, newest first.
    pub async fn attempts(&self, project_id: Uuid, limit: i64) -> Result<Vec<AttemptView>> {
        attempts(self.pool(), Some(project_id), None, limit).await
    }

    /// One attempt with everything in it, consistent with `attempt.version`. `None` when the
    /// attempt doesn't exist or belongs to another project.
    pub async fn snapshot(&self, project_id: Uuid, attempt_id: Uuid) -> Result<Option<Snapshot>> {
        let mut tx = self.pool().begin().await?;
        sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let Some(attempt) = attempts(&mut *tx, Some(project_id), Some(attempt_id), 1)
            .await?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let scope = Scope::Attempt(attempt_id);
        let snapshot = Snapshot {
            jobs: jobs(&mut tx, scope).await?,
            agent_runs: agent_runs(&mut tx, scope).await?,
            suites: suites(&mut tx, scope).await?,
            devices: devices(&mut tx, scope).await?,
            flows: flows(&mut tx, scope).await?,
            plans: plans(&mut tx, project_id, attempt_id).await?,
            attempt,
        };
        tx.commit().await?;
        Ok(Some(snapshot))
    }

    /// The current state of changed entities, for a live delta.
    pub async fn entities(&self, refs: &[EntityRef]) -> Result<Vec<EntityView>> {
        let mut by_kind: BTreeMap<u8, Vec<Uuid>> = BTreeMap::new();
        for r in refs {
            let (k, id) = match *r {
                EntityRef::Job(id) => (0, id),
                EntityRef::AgentRun(id) => (1, id),
                EntityRef::Suite(id) => (2, id),
                EntityRef::Flow(id) => (3, id),
                EntityRef::Device(id) => (4, id),
            };
            by_kind.entry(k).or_default().push(id);
        }
        let mut conn = self.pool().acquire().await?;
        let mut out = Vec::new();
        for (k, ids) in by_kind {
            let scope = Scope::Ids(&ids);
            match k {
                0 => out.extend(
                    jobs(&mut conn, scope)
                        .await?
                        .into_iter()
                        .map(EntityView::Job),
                ),
                1 => out.extend(
                    agent_runs(&mut conn, scope)
                        .await?
                        .into_iter()
                        .map(EntityView::AgentRun),
                ),
                2 => out.extend(
                    suites(&mut conn, scope)
                        .await?
                        .into_iter()
                        .map(EntityView::Suite),
                ),
                3 => out.extend(
                    flows(&mut conn, scope)
                        .await?
                        .into_iter()
                        .map(EntityView::Flow),
                ),
                _ => out.extend(
                    devices(&mut conn, scope)
                        .await?
                        .into_iter()
                        .map(EntityView::Device),
                ),
            }
        }
        Ok(out)
    }

    /// The live versions of attempts with open jobs: what the hub restores after a restart.
    pub async fn open_attempt_versions(&self) -> Result<Vec<(Uuid, Uuid, u64)>> {
        Ok(sqlx::query!(
            "SELECT DISTINCT a.id, a.project_id, a.version
               FROM pipeline_attempts a JOIN jobs j ON j.pipeline_attempt_id = a.id
              WHERE j.lifecycle <> 'closed'"
        )
        .fetch_all(self.pool())
        .await?
        .into_iter()
        .map(|r| (r.id, r.project_id, u64::try_from(r.version).unwrap_or(0)))
        .collect())
    }
}

async fn attempts<'e, E: sqlx::PgExecutor<'e>>(
    e: E,
    project: Option<Uuid>,
    attempt: Option<Uuid>,
    limit: i64,
) -> Result<Vec<AttemptView>> {
    Ok(sqlx::query_as!(
        AttemptView,
        "SELECT a.id, a.project_id, r.provider, r.provider_run_key AS run_key, r.repository,
                r.commit_sha, r.git_ref, a.attempt, a.version, a.first_seen_at,
                (SELECT sum(p.planned_total)::bigint FROM plans p
                  WHERE p.project_id = a.project_id AND p.pipeline_attempt_id = a.id) AS planned_total,
                (SELECT count(*) FROM jobs j WHERE j.pipeline_attempt_id = a.id) AS \"jobs!\",
                (SELECT count(*) FROM jobs j
                  WHERE j.pipeline_attempt_id = a.id AND j.lifecycle <> 'closed') AS \"open_jobs!\"
           FROM pipeline_attempts a JOIN pipeline_runs r ON r.id = a.pipeline_run_id
          WHERE ($1::uuid IS NULL OR a.project_id = $1) AND ($2::uuid IS NULL OR a.id = $2)
          ORDER BY a.first_seen_at DESC
          LIMIT $3",
        project,
        attempt,
        limit
    )
    .fetch_all(e)
    .await?)
}

async fn jobs(c: &mut sqlx::PgConnection, scope: Scope<'_>) -> Result<Vec<JobView>> {
    let (attempt, ids) = scope.parts();
    Ok(sqlx::query_as!(
        JobView,
        "SELECT id, pipeline_attempt_id, job_key, name, lifecycle, outcome,
                outcome_source::text AS outcome_source, reopened_count, rev
           FROM jobs
          WHERE ($1::uuid IS NULL OR pipeline_attempt_id = $1) AND ($2::uuid[] IS NULL OR id = ANY($2))
          ORDER BY started_at, job_key",
        attempt,
        ids.as_deref()
    )
    .fetch_all(c)
    .await?)
}

async fn agent_runs(c: &mut sqlx::PgConnection, scope: Scope<'_>) -> Result<Vec<AgentRunView>> {
    let (attempt, ids) = scope.parts();
    Ok(sqlx::query_as!(
        AgentRunView,
        "SELECT r.id, r.job_id, r.worker_id, w.runner_name, r.state,
                r.trust_level::text AS \"trust_level!\", r.committed_seq,
                (SELECT count(*) FROM ingest_gaps g WHERE g.agent_run_id = r.id) AS \"gaps!\",
                r.last_frame_at, r.late_resume, r.plan_id, r.plan_shard_id, r.maestro_exit_code,
                r.rev, NULL::text AS liveness
           FROM agent_runs r JOIN jobs j ON j.id = r.job_id
           LEFT JOIN workers w ON w.id = r.worker_id
          WHERE ($1::uuid IS NULL OR j.pipeline_attempt_id = $1) AND ($2::uuid[] IS NULL OR r.id = ANY($2))
          ORDER BY r.started_at",
        attempt,
        ids.as_deref()
    )
    .fetch_all(c)
    .await?)
}

async fn suites(c: &mut sqlx::PgConnection, scope: Scope<'_>) -> Result<Vec<SuiteView>> {
    let (attempt, ids) = scope.parts();
    Ok(sqlx::query_as!(
        SuiteView,
        "SELECT s.id, s.job_id, s.agent_run_id, s.shard_mode, s.shard_count, s.planned_count,
                s.plan_source::text AS plan_source, s.lifecycle, s.outcome,
                s.outcome_source::text AS outcome_source, s.report_status, s.exit_code, s.rev
           FROM suite_executions s JOIN jobs j ON j.id = s.job_id
          WHERE ($1::uuid IS NULL OR j.pipeline_attempt_id = $1) AND ($2::uuid[] IS NULL OR s.id = ANY($2))
          ORDER BY s.started_at NULLS LAST, s.id",
        attempt,
        ids.as_deref()
    )
    .fetch_all(c)
    .await?)
}

async fn devices(c: &mut sqlx::PgConnection, scope: Scope<'_>) -> Result<Vec<DeviceView>> {
    let (attempt, ids) = scope.parts();
    Ok(sqlx::query_as!(
        DeviceView,
        "SELECT d.id, d.agent_run_id, d.serial, d.state, d.rev
           FROM device_sessions d JOIN agent_runs r ON r.id = d.agent_run_id
           JOIN jobs j ON j.id = r.job_id
          WHERE ($1::uuid IS NULL OR j.pipeline_attempt_id = $1) AND ($2::uuid[] IS NULL OR d.id = ANY($2))
          ORDER BY d.first_seen_at, d.id",
        attempt,
        ids.as_deref()
    )
    .fetch_all(c)
    .await?)
}

async fn flows(c: &mut sqlx::PgConnection, scope: Scope<'_>) -> Result<Vec<FlowView>> {
    let (attempt, ids) = scope.parts();
    Ok(sqlx::query_as!(
        FlowView,
        "SELECT f.id, f.job_id, f.suite_execution_id, f.agent_run_id, f.device_session_id,
                f.shard_number, f.ordinal, f.flow_name AS name, f.flow_path AS path, f.state,
                f.outcome_source::text AS outcome_source, f.finality::text AS \"finality!\",
                f.has_conflict, f.failure_message, f.duration_ms, f.rev,
                (SELECT count(*) FROM step_executions st
                  WHERE st.run_day = r.run_day AND st.flow_execution_id = f.id) AS \"steps!\"
           FROM flow_executions f JOIN agent_runs r ON r.id = f.agent_run_id
          WHERE ($1::uuid IS NULL OR f.pipeline_attempt_id = $1) AND ($2::uuid[] IS NULL OR f.id = ANY($2))
          ORDER BY f.shard_number NULLS LAST, f.ordinal NULLS LAST, f.id",
        attempt,
        ids.as_deref()
    )
    .fetch_all(c)
    .await?)
}

async fn plans(c: &mut sqlx::PgConnection, project: Uuid, attempt: Uuid) -> Result<Vec<PlanView>> {
    let rows = sqlx::query!(
        "SELECT p.plan_id, p.planned_total, s.shard_id, s.flows, s.estimate_ms,
                coalesce(array_agg(DISTINCT r.id) FILTER (WHERE r.id IS NOT NULL), '{}') AS \"runs!\",
                (SELECT count(*) FROM flow_executions f JOIN agent_runs r2 ON r2.id = f.agent_run_id
                  WHERE r2.project_id = p.project_id AND r2.plan_id = p.plan_id
                    AND r2.plan_shard_id = s.shard_id) AS \"executed!\"
           FROM plans p
           JOIN plan_shards s ON s.project_id = p.project_id AND s.plan_id = p.plan_id
           LEFT JOIN agent_runs r ON r.project_id = p.project_id AND r.plan_id = p.plan_id
                                 AND r.plan_shard_id = s.shard_id
          WHERE p.project_id = $1
            AND (p.pipeline_attempt_id = $2
                 OR p.plan_id IN (SELECT r3.plan_id FROM agent_runs r3 JOIN jobs j ON j.id = r3.job_id
                                   WHERE j.pipeline_attempt_id = $2 AND r3.plan_id IS NOT NULL))
          GROUP BY p.project_id, p.plan_id, p.planned_total, s.shard_id, s.flows, s.estimate_ms
          ORDER BY p.plan_id, s.shard_id",
        project,
        attempt
    )
    .fetch_all(c)
    .await?;
    let mut out: Vec<PlanView> = Vec::new();
    for r in rows {
        let shard = PlanShardView {
            shard_id: r.shard_id,
            flows: r.flows,
            estimate_ms: r.estimate_ms,
            agent_runs: r.runs,
            executed: r.executed,
        };
        match out.last_mut() {
            Some(p) if p.plan_id == r.plan_id => p.shards.push(shard),
            _ => out.push(PlanView {
                plan_id: r.plan_id,
                planned_total: r.planned_total,
                shards: vec![shard],
            }),
        }
    }
    Ok(out)
}
