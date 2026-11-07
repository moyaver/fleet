// SPDX-License-Identifier: AGPL-3.0-only
//! Screen A: active attempts, recently finished ones and the runner health.

use std::collections::HashMap;

use fleet_protocol::{
    live::{AttemptView, JobView},
    rest::{ActiveAttemptView, FlowCounts, JobCounts, RecentAttemptView, WorkerSummaryView},
};
use time::OffsetDateTime;
use uuid::Uuid;

use super::samples::HostRow;
use crate::{Result, Store, live};

/// Active attempts shown at most.
const ACTIVE_LIMIT: i64 = 100;
/// Workers listed at most; the counts cover every worker seen.
const WORKER_LIMIT: i64 = 1000;

/// One of a worker's agent runs that may still be alive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunState {
    /// The agent run.
    pub id: Uuid,
    /// Its stored state (`active`, `detached`, `finalizing`, `abandoned`).
    pub state: String,
    /// Its last persisted contact.
    pub last_frame_at: OffsetDateTime,
}

/// A worker, and the runs its display liveness is derived from.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerRow {
    /// The view; `liveness` is `None` until the server fills it in.
    pub view: WorkerSummaryView,
    /// Runs that started or were heard from within the last day and haven't ended.
    pub runs: Vec<RunState>,
    /// Its newest host sample from the last hour, if any.
    pub latest: Option<HostRow>,
}

/// Screen A's data, before display liveness is applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Overview {
    /// Active attempts, newest first.
    pub active: Vec<ActiveAttemptView>,
    /// How many attempts are active in all.
    pub active_total: i64,
    /// Recently finished attempts, newest first.
    pub recent: Vec<RecentAttemptView>,
    /// Workers seen in the last day, most recently seen first.
    pub workers: Vec<WorkerRow>,
    /// Emulators seen in the last two minutes with adb reporting them online (issue #123).
    pub active_emulators: i64,
}

impl Store {
    /// Screen A for one project: active attempts, the `recent` latest finished ones (finished
    /// within the last 7 days), and the workers seen in the last day.
    pub async fn overview(&self, project_id: Uuid, recent: i64) -> Result<Overview> {
        let mut tx = self.pool().begin().await?;
        sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let active_rows = sqlx::query!(
            "SELECT a.id, count(*) OVER () AS \"total!\"
               FROM pipeline_attempts a
              WHERE a.project_id = $1
                AND EXISTS (SELECT 1 FROM jobs j
                             WHERE j.pipeline_attempt_id = a.id AND j.lifecycle <> 'closed')
              ORDER BY a.first_seen_at DESC, a.id
              LIMIT $2",
            project_id,
            ACTIVE_LIMIT
        )
        .fetch_all(&mut *tx)
        .await?;
        let recent_rows = sqlx::query!(
            "SELECT a.id, coalesce(max(j.closed_at), a.first_seen_at) AS \"finished_at!\"
               FROM pipeline_attempts a JOIN jobs j ON j.pipeline_attempt_id = a.id
              WHERE a.project_id = $1 AND a.first_seen_at > now() - interval '7 days'
              GROUP BY a.id, a.first_seen_at
             HAVING bool_and(j.lifecycle = 'closed')
              ORDER BY 2 DESC, a.id
              LIMIT $2",
            project_id,
            recent
        )
        .fetch_all(&mut *tx)
        .await?;
        let ids: Vec<Uuid> = active_rows
            .iter()
            .map(|r| r.id)
            .chain(recent_rows.iter().map(|r| r.id))
            .collect();
        let attempts: HashMap<Uuid, AttemptView> =
            live::attempts(&mut *tx, Some(project_id), Some(&ids), i64::MAX)
                .await?
                .into_iter()
                .map(|a| (a.id, a))
                .collect();
        let mut job_counts = job_counts(&mut tx, &ids).await?;
        let mut flow_counts = flow_counts(&mut tx, &ids).await?;
        let recent_ids: Vec<Uuid> = recent_rows.iter().map(|r| r.id).collect();
        let mut jobs = jobs_of(&mut tx, &recent_ids).await?;
        let workers = workers(&mut tx, project_id, None).await?;
        let active_emulators = active_emulators(&mut tx, project_id).await?;
        tx.commit().await?;

        let active_total = active_rows.first().map_or(0, |r| r.total);
        let active = active_rows
            .iter()
            .filter_map(|r| {
                Some(ActiveAttemptView {
                    attempt: attempts.get(&r.id)?.clone(),
                    jobs: job_counts.remove(&r.id).unwrap_or_default(),
                    flows: flow_counts.remove(&r.id).unwrap_or_default(),
                })
            })
            .collect();
        let recent = recent_rows
            .iter()
            .filter_map(|r| {
                Some(RecentAttemptView {
                    attempt: attempts.get(&r.id)?.clone(),
                    finished_at: r.finished_at,
                    jobs: jobs.remove(&r.id).unwrap_or_default(),
                    flows: flow_counts.remove(&r.id).unwrap_or_default(),
                })
            })
            .collect();
        Ok(Overview {
            active,
            active_total,
            recent,
            workers,
            active_emulators,
        })
    }
}

/// How many emulators the project's devices were seen on in the last two minutes, adb reporting
/// them `device` (online). An emulator is a device whose serial is `emulator-<port>`. Device
/// samples come every 30 s per device, so two minutes covers the last few.
async fn active_emulators(
    c: &mut sqlx::PgConnection,
    project_id: Uuid,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(DISTINCT s.device_session_id) AS "active!"
             FROM device_samples s JOIN device_sessions d ON d.id = s.device_session_id
            WHERE s.project_id = $1
              AND s.ts > now() - interval '2 minutes'
              AND s.adb_state = 0 -- adb's `device` state (telemetry::adb_code)
              AND d.serial LIKE 'emulator-%'"#,
        project_id
    )
    .fetch_one(c)
    .await
}

async fn job_counts(
    c: &mut sqlx::PgConnection,
    attempts: &[Uuid],
) -> Result<HashMap<Uuid, JobCounts>> {
    Ok(sqlx::query!(
        "SELECT pipeline_attempt_id AS \"attempt!\",
                count(*) FILTER (WHERE lifecycle = 'pending') AS \"pending!\",
                count(*) FILTER (WHERE lifecycle = 'starting') AS \"starting!\",
                count(*) FILTER (WHERE lifecycle = 'running') AS \"running!\",
                count(*) FILTER (WHERE lifecycle = 'finalizing') AS \"finalizing!\",
                count(*) FILTER (WHERE lifecycle = 'closed') AS \"closed!\"
           FROM jobs WHERE pipeline_attempt_id = ANY($1) GROUP BY pipeline_attempt_id",
        attempts
    )
    .fetch_all(c)
    .await?
    .into_iter()
    .map(|r| {
        (
            r.attempt,
            JobCounts {
                pending: r.pending,
                starting: r.starting,
                running: r.running,
                finalizing: r.finalizing,
                closed: r.closed,
            },
        )
    })
    .collect())
}

async fn flow_counts(
    c: &mut sqlx::PgConnection,
    attempts: &[Uuid],
) -> Result<HashMap<Uuid, FlowCounts>> {
    Ok(sqlx::query!(
        "SELECT pipeline_attempt_id AS \"attempt!\",
                count(*) FILTER (WHERE state = 'pending') AS \"pending!\",
                count(*) FILTER (WHERE state = 'running') AS \"running!\",
                count(*) FILTER (WHERE state = 'passed') AS \"passed!\",
                count(*) FILTER (WHERE state = 'failed') AS \"failed!\",
                count(*) FILTER (WHERE state = 'skipped') AS \"skipped!\",
                count(*) FILTER (WHERE state = 'interrupted') AS \"interrupted!\",
                count(*) FILTER (WHERE state = 'unknown') AS \"unknown!\"
           FROM flow_executions WHERE pipeline_attempt_id = ANY($1) GROUP BY pipeline_attempt_id",
        attempts
    )
    .fetch_all(c)
    .await?
    .into_iter()
    .map(|r| {
        (
            r.attempt,
            FlowCounts {
                pending: r.pending,
                running: r.running,
                passed: r.passed,
                failed: r.failed,
                skipped: r.skipped,
                interrupted: r.interrupted,
                unknown: r.unknown,
            },
        )
    })
    .collect())
}

async fn jobs_of(
    c: &mut sqlx::PgConnection,
    attempts: &[Uuid],
) -> Result<HashMap<Uuid, Vec<JobView>>> {
    let rows = sqlx::query_as!(
        JobView,
        "SELECT id, pipeline_attempt_id, job_key, name, lifecycle, outcome,
                outcome_source::text AS outcome_source, reopened_count, rev
           FROM jobs WHERE pipeline_attempt_id = ANY($1) ORDER BY started_at, job_key",
        attempts
    )
    .fetch_all(c)
    .await?;
    let mut out: HashMap<Uuid, Vec<JobView>> = HashMap::new();
    for j in rows {
        out.entry(j.pipeline_attempt_id).or_default().push(j);
    }
    Ok(out)
}

/// Workers seen in the last day (or one worker by ID, whenever it was seen).
pub(super) async fn workers(
    c: &mut sqlx::PgConnection,
    project: Uuid,
    only: Option<Uuid>,
) -> Result<Vec<WorkerRow>> {
    let mut rows: Vec<WorkerRow> = sqlx::query!(
        "SELECT w.id, w.runner_name, w.os, w.arch, w.first_seen_at, w.last_seen_at,
                coalesce(array_agg(r.id) FILTER (WHERE r.id IS NOT NULL), '{}') AS \"run_ids!\",
                coalesce(array_agg(r.state) FILTER (WHERE r.id IS NOT NULL), '{}') AS \"run_states!\",
                coalesce(array_agg(r.last_frame_at) FILTER (WHERE r.id IS NOT NULL), '{}') AS \"run_frames!\"
           FROM workers w
           LEFT JOIN agent_runs r ON r.worker_id = w.id AND r.state <> 'ended'
                                 AND r.last_frame_at > now() - interval '24 hours'
          WHERE w.project_id = $1
            AND CASE WHEN $2::uuid IS NULL THEN w.last_seen_at > now() - interval '24 hours'
                     ELSE w.id = $2 END
          GROUP BY w.id
          ORDER BY w.last_seen_at DESC, w.id
          LIMIT $3",
        project,
        only,
        WORKER_LIMIT
    )
    .fetch_all(&mut *c)
    .await?
    .into_iter()
    .map(|r| {
        let runs: Vec<RunState> = r
            .run_ids
            .into_iter()
            .zip(r.run_states.into_iter().zip(r.run_frames))
            .map(|(id, (state, last_frame_at))| RunState {
                id,
                state,
                last_frame_at,
            })
            .collect();
        WorkerRow {
            view: WorkerSummaryView {
                id: r.id,
                runner_name: r.runner_name,
                os: r.os,
                arch: r.arch,
                first_seen_at: r.first_seen_at,
                last_seen_at: r.last_seen_at,
                liveness: None,
                active_runs: i64::try_from(runs.len()).unwrap_or(i64::MAX),
                latest: None,
            },
            runs,
            latest: None,
        }
    })
    .collect();
    let ids: Vec<Uuid> = rows.iter().map(|w| w.view.id).collect();
    let mut latest: HashMap<Uuid, HostRow> = latest_host_samples(c, project, &ids).await?;
    for w in &mut rows {
        w.latest = latest.remove(&w.view.id);
    }
    Ok(rows)
}

/// Each worker's newest host sample from the last hour. Only the machine's readings: a process
/// reading belongs to one run, and a worker with several runs has no one of them to stand for it.
async fn latest_host_samples(
    c: &mut sqlx::PgConnection,
    project: Uuid,
    workers: &[Uuid],
) -> Result<HashMap<Uuid, HostRow>> {
    Ok(sqlx::query!(
        "SELECT DISTINCT ON (worker_id) worker_id AS \"worker_id!\", ts,
                host_cpu_pct::float8 AS host_cpu_pct,
                host_mem_used_bytes::float8 AS host_mem_used_bytes,
                host_mem_total_bytes::float8 AS host_mem_total_bytes,
                host_load_1m::float8 AS host_load_1m,
                host_cpu_cores::float8 AS host_cpu_cores,
                host_swap_used_bytes::float8 AS host_swap_used_bytes,
                host_swap_total_bytes::float8 AS host_swap_total_bytes,
                host_psi_cpu_pct::float8 AS host_psi_cpu_pct,
                host_psi_memory_pct::float8 AS host_psi_memory_pct,
                host_psi_io_pct::float8 AS host_psi_io_pct
           FROM host_samples
          WHERE project_id = $1 AND worker_id = ANY($2) AND ts > now() - interval '1 hour'
          ORDER BY worker_id, ts DESC",
        project,
        workers
    )
    .fetch_all(c)
    .await?
    .into_iter()
    .map(|r| {
        (
            r.worker_id,
            HostRow {
                at: r.ts,
                host_cpu_pct: r.host_cpu_pct,
                host_mem_used_bytes: r.host_mem_used_bytes,
                host_mem_total_bytes: r.host_mem_total_bytes,
                host_load_1m: r.host_load_1m,
                host_cpu_cores: r.host_cpu_cores,
                host_swap_used_bytes: r.host_swap_used_bytes,
                host_swap_total_bytes: r.host_swap_total_bytes,
                host_psi_cpu_pct: r.host_psi_cpu_pct,
                host_psi_memory_pct: r.host_psi_memory_pct,
                host_psi_io_pct: r.host_psi_io_pct,
            },
        )
    })
    .collect())
}
