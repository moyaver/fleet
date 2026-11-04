// SPDX-License-Identifier: AGPL-3.0-only
//! Queries behind REST snapshots and live deltas; the views are `fleet_protocol::live` (research 03 §5.3; invariant I4).
//!
//! A snapshot is one pipeline attempt read in a single `REPEATABLE READ` transaction together
//! with the attempt's live `version`, so a client that applies deltas with a higher version
//! never misses or double-applies a change. Every outcome travels with its source and, for
//! flows, its finality: nothing is shown without a provenance.

use std::collections::BTreeMap;

use fleet_protocol::live::{
    AgentRunView, AttemptView, DeviceView, EntityView, FlowView, JobView, PlanShardView, PlanView,
    ProjectView, Snapshot, SuiteView,
};
use uuid::Uuid;

use crate::{Result, Store, ingest::EntityRef};

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
    let rows = sqlx::query!(
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
    .await?;
    // The server fills `liveness` from live contact; the database has none.
    Ok(rows
        .into_iter()
        .map(|r| AgentRunView {
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
            rev: r.rev,
            liveness: None,
        })
        .collect())
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
