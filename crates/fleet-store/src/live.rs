// SPDX-License-Identifier: AGPL-3.0-only
//! Queries behind REST snapshots and live deltas; the views are `fleet_protocol::live` (research 03 §5.3; invariant I4).
//!
//! A snapshot is one pipeline attempt read in a single `REPEATABLE READ` transaction together
//! with the attempt's live `version`, so a client that applies deltas with a higher version
//! never misses or double-applies a change. Every outcome travels with its source and, for
//! flows, its finality: nothing is shown without a provenance.
//!
//! Between instances (research 03 §5.5): every committed version bump also runs `pg_notify` in
//! its transaction, so the notice is delivered only if the commit happens. Each instance
//! listens on the `fleet_live` channel ([`Store::notices`]) and republishes the others' changes into its own
//! hub. A notice carries ids and the version, never the state: the receiver loads the entities
//! from Postgres, as a resync does.

use std::collections::BTreeMap;

use fleet_protocol::{
    frame::RunOrigin,
    live::{
        AgentRunView, AttemptView, DeviceView, EntityView, FlowView, JobView, PlanShardView,
        PlanView, ProjectView, Snapshot, SuiteView,
    },
};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, postgres::PgListener};
use uuid::Uuid;

use crate::{
    Error, Result, Store,
    ingest::{Change, EntityRef},
};

/// The origin a run's `origin` column names. The column's check constraint admits only the names
/// [`RunOrigin::parse`] knows, so anything else is stored data this build does not understand.
pub(crate) fn origin_of(stored: &str) -> Result<RunOrigin> {
    RunOrigin::parse(stored)
        .ok_or_else(|| Error::Corrupt(format!("run origin {stored:?} is not known")))
}

/// The Postgres channel that carries committed live changes between instances.
const CHANNEL: &str = "fleet_live";
/// Postgres refuses notifications of 8000 bytes or more; a longer entity list is sent as none.
const MAX_PAYLOAD: usize = 7_000;

/// What an instance tells the others about one committed change.
#[derive(Debug, Serialize, Deserialize)]
struct Notice {
    origin: Uuid,
    project: Uuid,
    attempt: Uuid,
    version: u64,
    /// `None` when the list would not fit: receivers resync their subscribers instead.
    entities: Option<Vec<EntityRef>>,
}

/// Announces a change committed by `tx`. The notice is delivered only when `tx` commits.
pub(crate) async fn notify(
    tx: &mut PgConnection,
    origin: Uuid,
    change: &Change,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(CHANNEL)
        .bind(payload(origin, change))
        .execute(tx)
        .await?;
    Ok(())
}

fn payload(origin: Uuid, change: &Change) -> String {
    let notice = |entities: Option<Vec<EntityRef>>| Notice {
        origin,
        project: change.project_id,
        attempt: change.pipeline_attempt_id,
        version: change.version,
        entities,
    };
    let full = serde_json::to_string(&notice(Some(change.entities.clone())))
        .expect("a notice of ids should serialize");
    if full.len() < MAX_PAYLOAD {
        return full;
    }
    serde_json::to_string(&notice(None)).expect("a notice of ids should serialize")
}

/// The change a notice describes, unless this instance sent it or it is malformed.
fn decode(payload: &str, origin: Uuid) -> Option<Change> {
    let notice: Notice = match serde_json::from_str(payload) {
        Ok(notice) => notice,
        Err(err) => {
            tracing::warn!(error = %err, "ignoring a malformed live notice");
            return None;
        }
    };
    (notice.origin != origin).then(|| Change {
        project_id: notice.project,
        pipeline_attempt_id: notice.attempt,
        version: notice.version,
        entities: notice.entities.unwrap_or_default(),
    })
}

/// One thing [`Notices::recv`] reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveNotice {
    /// A change another instance committed.
    Change(Change),
    /// The connection was lost and re-established: notices sent in the gap are gone, so
    /// subscribers must resync.
    Reconnected,
}

/// This instance's listener on the other instances' committed changes.
#[derive(Debug)]
pub struct Notices {
    listener: PgListener,
    origin: Uuid,
}

impl Notices {
    /// The next notice from another instance, or [`LiveNotice::Reconnected`].
    ///
    /// # Errors
    /// The listener is broken. Drop it and call [`Store::notices`] again.
    ///
    /// # Cancel safety
    /// Cancel-safe: a cancelled call loses no notice, because sqlx consumes a message only once
    /// it is complete.
    pub async fn recv(&mut self) -> Result<LiveNotice> {
        loop {
            let Some(notification) = self.listener.try_recv().await? else {
                return Ok(LiveNotice::Reconnected);
            };
            if let Some(change) = decode(notification.payload(), self.origin) {
                return Ok(LiveNotice::Change(change));
            }
        }
    }
}

/// Rows of one attempt, or the rows with these IDs.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Scope<'a> {
    Attempt(Uuid),
    Ids(Uuid, &'a [Uuid]),
}

impl Scope<'_> {
    fn parts(&self) -> (Option<Uuid>, Option<Vec<Uuid>>, Option<Uuid>) {
        match self {
            Self::Attempt(a) => (Some(*a), None, None),
            Self::Ids(project, ids) => (None, Some(ids.to_vec()), Some(*project)),
        }
    }
}

impl Store {
    /// Starts listening for the other instances' committed changes. Holds one pool connection.
    pub async fn notices(&self) -> Result<Notices> {
        let mut listener = PgListener::connect_with(self.pool()).await?;
        listener.listen(CHANNEL).await?;
        Ok(Notices {
            listener,
            origin: self.origin(),
        })
    }

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
        let Some(attempt) = attempts(&mut *tx, Some(project_id), Some(&[attempt_id]), 1)
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

    /// One attempt's linked plans, each shard with its actual span: what the plan table shows,
    /// without the rest of the snapshot. `None` when the attempt isn't in `project_id`.
    pub async fn attempt_plans(
        &self,
        project_id: Uuid,
        attempt_id: Uuid,
    ) -> Result<Option<Vec<PlanView>>> {
        let mut conn = self.pool().acquire().await?;
        if attempts(&mut *conn, Some(project_id), Some(&[attempt_id]), 1)
            .await?
            .is_empty()
        {
            return Ok(None);
        }
        Ok(Some(plans(&mut conn, project_id, attempt_id).await?))
    }

    /// The current state of changed entities, for a live delta.
    pub async fn entities(&self, project: Uuid, refs: &[EntityRef]) -> Result<Vec<EntityView>> {
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
            let scope = Scope::Ids(project, &ids);
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

pub(crate) async fn attempts<'e, E: sqlx::PgExecutor<'e>>(
    e: E,
    project: Option<Uuid>,
    ids: Option<&[Uuid]>,
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
          WHERE ($1::uuid IS NULL OR a.project_id = $1) AND ($2::uuid[] IS NULL OR a.id = ANY($2))
          ORDER BY a.first_seen_at DESC
          LIMIT $3",
        project,
        ids,
        limit
    )
    .fetch_all(e)
    .await?)
}

pub(crate) async fn jobs(c: &mut sqlx::PgConnection, scope: Scope<'_>) -> Result<Vec<JobView>> {
    let (attempt, ids, project) = scope.parts();
    Ok(sqlx::query_as!(
        JobView,
        "SELECT id, pipeline_attempt_id, job_key, name, lifecycle, outcome,
                outcome_source::text AS outcome_source, reopened_count, rev
           FROM jobs
          WHERE ($1::uuid IS NULL OR pipeline_attempt_id = $1) AND ($2::uuid[] IS NULL OR id = ANY($2)) AND ($3::uuid IS NULL OR project_id = $3)
          ORDER BY started_at, job_key",
        attempt,
        ids.as_deref(),
        project
    )
    .fetch_all(c)
    .await?)
}

pub(crate) async fn agent_runs(
    c: &mut sqlx::PgConnection,
    scope: Scope<'_>,
) -> Result<Vec<AgentRunView>> {
    let (attempt, ids, project) = scope.parts();
    let rows = sqlx::query!(
        "SELECT r.id, r.job_id, r.worker_id, w.runner_name, r.state,
                r.trust_level::text AS \"trust_level!\", r.committed_seq,
                (SELECT count(*) FROM ingest_gaps g WHERE g.agent_run_id = r.id) AS \"gaps!\",
                r.last_frame_at, r.late_resume, r.plan_id, r.plan_shard_id, r.maestro_exit_code,
                r.maestro_version, r.maestro_profile_supported, r.file_warnings, r.rev, r.origin, NULL::text AS liveness
           FROM agent_runs r JOIN jobs j ON j.id = r.job_id
           LEFT JOIN workers w ON w.id = r.worker_id
          WHERE ($1::uuid IS NULL OR j.pipeline_attempt_id = $1) AND ($2::uuid[] IS NULL OR r.id = ANY($2)) AND ($3::uuid IS NULL OR r.project_id = $3)
          ORDER BY r.started_at",
        attempt,
        ids.as_deref(),
        project
    )
    .fetch_all(c)
    .await?;
    // The server fills `liveness` from live contact; the database has none.
    rows.into_iter()
        .map(|r| {
            Ok(AgentRunView {
                origin: origin_of(&r.origin)?,
                id: r.id,
                job_id: r.job_id,
                worker_id: r.worker_id,
                runner_name: r.runner_name,
                state: r.state,
                trust_level: r.trust_level,
                committed_seq: r.committed_seq,
                gaps: r.gaps,
                last_frame_at: r.last_frame_at,
                late_resume: r.late_resume,
                plan_id: r.plan_id,
                plan_shard_id: r.plan_shard_id,
                maestro_exit_code: r.maestro_exit_code,
                maestro_version: r.maestro_version,
                maestro_profile_supported: r.maestro_profile_supported,
                file_warnings: r.file_warnings,
                rev: r.rev,
                liveness: None,
            })
        })
        .collect()
}

pub(crate) async fn suites(c: &mut sqlx::PgConnection, scope: Scope<'_>) -> Result<Vec<SuiteView>> {
    let (attempt, ids, project) = scope.parts();
    Ok(sqlx::query_as!(
        SuiteView,
        "SELECT s.id, s.job_id, s.agent_run_id, s.shard_mode, s.shard_count, s.planned_count,
                s.plan_source::text AS plan_source, s.lifecycle, s.outcome,
                s.outcome_source::text AS outcome_source, s.report_status, s.exit_code, s.rev
           FROM suite_executions s JOIN jobs j ON j.id = s.job_id
          WHERE ($1::uuid IS NULL OR j.pipeline_attempt_id = $1) AND ($2::uuid[] IS NULL OR s.id = ANY($2)) AND ($3::uuid IS NULL OR s.project_id = $3)
          ORDER BY s.started_at NULLS LAST, s.id",
        attempt,
        ids.as_deref(),
        project
    )
    .fetch_all(c)
    .await?)
}

pub(crate) async fn devices(
    c: &mut sqlx::PgConnection,
    scope: Scope<'_>,
) -> Result<Vec<DeviceView>> {
    let (attempt, ids, project) = scope.parts();
    Ok(sqlx::query_as!(
        DeviceView,
        "SELECT d.id, d.agent_run_id, d.serial, d.state, d.rev
           FROM device_sessions d JOIN agent_runs r ON r.id = d.agent_run_id
           JOIN jobs j ON j.id = r.job_id
          WHERE ($1::uuid IS NULL OR j.pipeline_attempt_id = $1) AND ($2::uuid[] IS NULL OR d.id = ANY($2)) AND ($3::uuid IS NULL OR d.project_id = $3)
          ORDER BY d.first_seen_at, d.id",
        attempt,
        ids.as_deref(),
        project
    )
    .fetch_all(c)
    .await?)
}

pub(crate) async fn flows(c: &mut sqlx::PgConnection, scope: Scope<'_>) -> Result<Vec<FlowView>> {
    let (attempt, ids, project) = scope.parts();
    let rows = sqlx::query!(
        "SELECT f.id, f.job_id, f.suite_execution_id, f.agent_run_id, f.device_session_id,
                f.shard_number, f.ordinal, f.flow_name AS name, f.flow_path AS path, f.state,
                f.outcome_source::text AS outcome_source, f.finality::text AS \"finality!\",
                f.has_conflict, f.failure_message, f.duration_ms, f.rev, r.origin,
                (SELECT count(*) FROM step_executions st
                  WHERE st.run_day = r.run_day AND st.flow_execution_id = f.id) AS \"steps!\"
           FROM flow_executions f JOIN agent_runs r ON r.id = f.agent_run_id
          WHERE ($1::uuid IS NULL OR f.pipeline_attempt_id = $1) AND ($2::uuid[] IS NULL OR f.id = ANY($2)) AND ($3::uuid IS NULL OR f.project_id = $3)
          ORDER BY f.shard_number NULLS LAST, f.ordinal NULLS LAST, f.id",
        attempt,
        ids.as_deref(),
        project
    )
    .fetch_all(c)
    .await?;
    rows.into_iter()
        .map(|f| {
            Ok(FlowView {
                origin: origin_of(&f.origin)?,
                id: f.id,
                job_id: f.job_id,
                suite_execution_id: f.suite_execution_id,
                agent_run_id: f.agent_run_id,
                device_session_id: f.device_session_id,
                shard_number: f.shard_number,
                ordinal: f.ordinal,
                name: f.name,
                path: f.path,
                state: f.state,
                outcome_source: f.outcome_source,
                finality: f.finality,
                has_conflict: f.has_conflict,
                failure_message: f.failure_message,
                duration_ms: f.duration_ms,
                rev: f.rev,
                steps: f.steps,
            })
        })
        .collect()
}

async fn plans(c: &mut sqlx::PgConnection, project: Uuid, attempt: Uuid) -> Result<Vec<PlanView>> {
    let rows = sqlx::query!(
        "SELECT p.plan_id, p.planned_total, s.shard_id, s.flows, s.estimate_ms,
                (p.manifest->>'equalCountEstimatedSeconds')::bigint * 1000 AS equal_count_estimate_ms,
                coalesce(array_agg(DISTINCT r.id) FILTER (WHERE r.id IS NOT NULL), '{}') AS \"runs!\",
                (SELECT count(*) FROM flow_executions f JOIN agent_runs r2 ON r2.id = f.agent_run_id
                  WHERE r2.project_id = p.project_id AND r2.plan_id = p.plan_id
                    AND r2.plan_shard_id = s.shard_id) AS \"executed!\",
                (SELECT CASE WHEN bool_or(f.ended_at IS NULL) THEN NULL
                             ELSE (extract(epoch FROM max(f.ended_at) - min(f.started_at)) * 1000)::bigint
                        END
                   FROM flow_executions f JOIN agent_runs r4 ON r4.id = f.agent_run_id
                  WHERE r4.project_id = p.project_id AND r4.plan_id = p.plan_id
                    AND r4.plan_shard_id = s.shard_id AND f.started_at IS NOT NULL) AS actual_ms
           FROM plans p
           JOIN plan_shards s ON s.project_id = p.project_id AND s.plan_id = p.plan_id
           LEFT JOIN agent_runs r ON r.project_id = p.project_id AND r.plan_id = p.plan_id
                                 AND r.plan_shard_id = s.shard_id
          WHERE p.project_id = $1
            AND (p.pipeline_attempt_id = $2
                 OR p.plan_id IN (SELECT r3.plan_id FROM agent_runs r3 JOIN jobs j ON j.id = r3.job_id
                                   WHERE j.pipeline_attempt_id = $2 AND r3.plan_id IS NOT NULL))
          GROUP BY p.project_id, p.plan_id, p.planned_total, p.manifest, s.shard_id, s.flows,
                   s.estimate_ms
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
            actual_ms: r.actual_ms,
        };
        match out.last_mut() {
            Some(p) if p.plan_id == r.plan_id => p.shards.push(shard),
            _ => out.push(PlanView {
                plan_id: r.plan_id,
                planned_total: r.planned_total,
                equal_count_estimate_ms: r.equal_count_estimate_ms,
                shards: vec![shard],
            }),
        }
    }
    Ok(out)
}
