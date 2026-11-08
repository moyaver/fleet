// SPDX-License-Identifier: AGPL-3.0-only
//! Screen C: worker and device detail, and a worker's run history.

use std::collections::HashMap;

use base64::Engine as _;
use fleet_protocol::{
    live::{AgentRunView, DeviceView, FlowView},
    rest::{DeviceDetailView, DeviceProfileView, DeviceSessionView, WorkerRunView},
};
use time::OffsetDateTime;
use uuid::Uuid;

use super::overview::{WorkerRow, workers};
use crate::{Result, Store, live};

/// Where a page of a worker's runs ends: the oldest run it showed. Opaque to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunCursor {
    started_at: OffsetDateTime,
    id: Uuid,
}

impl RunCursor {
    /// A cursor after the entry at `at` with `id`.
    pub(super) const fn new(at: OffsetDateTime, id: Uuid) -> Self {
        Self { started_at: at, id }
    }

    /// The time and ID the cursor stands after.
    pub(super) const fn parts(&self) -> (OffsetDateTime, Uuid) {
        (self.started_at, self.id)
    }

    /// The token a client sends back for the next page.
    #[must_use]
    pub fn encode(&self) -> String {
        let micros = self.started_at.unix_timestamp_nanos() / 1000;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{micros}.{}", self.id))
    }

    /// The cursor a token stands for; `None` for anything this server didn't issue.
    #[must_use]
    pub fn decode(token: &str) -> Option<Self> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token)
            .ok()?;
        let raw = String::from_utf8(raw).ok()?;
        let (micros, id) = raw.split_once('.')?;
        Some(Self {
            started_at: OffsetDateTime::from_unix_timestamp_nanos(
                micros.parse::<i128>().ok()?.checked_mul(1000)?,
            )
            .ok()?,
            id: id.parse().ok()?,
        })
    }
}

/// A worker's detail, before display liveness is applied.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerDetail {
    /// The worker and the runs its liveness comes from.
    pub row: WorkerRow,
    /// Its 10 latest runs (their `liveness` is `None` until the server fills it in).
    pub recent_runs: Vec<WorkerRunView>,
    /// Its 20 latest device sessions.
    pub devices: Vec<DeviceSessionView>,
}

impl Store {
    /// One worker of a project. `None` when it doesn't exist or belongs to another project.
    pub async fn worker(&self, project_id: Uuid, worker_id: Uuid) -> Result<Option<WorkerDetail>> {
        let mut tx = self.pool().begin().await?;
        sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let Some(row) = workers(&mut tx, project_id, Some(worker_id)).await?.pop() else {
            return Ok(None);
        };
        let recent_runs = worker_runs(&mut tx, project_id, worker_id, None, 10)
            .await?
            .0;
        let devices = device_sessions(&mut tx, project_id, Some(worker_id), None, 20).await?;
        tx.commit().await?;
        Ok(Some(WorkerDetail {
            row,
            recent_runs,
            devices,
        }))
    }

    /// A page of a worker's runs, newest first, after `after`. `None` when the worker doesn't
    /// exist in the project. The cursor is `Some` when more runs follow.
    pub async fn worker_runs_page(
        &self,
        project_id: Uuid,
        worker_id: Uuid,
        after: Option<RunCursor>,
        limit: i64,
    ) -> Result<Option<(Vec<WorkerRunView>, Option<RunCursor>)>> {
        let exists = sqlx::query_scalar!(
            "SELECT true AS \"x!\" FROM workers WHERE id = $1 AND project_id = $2",
            worker_id,
            project_id
        )
        .fetch_optional(self.pool())
        .await?;
        if exists.is_none() {
            return Ok(None);
        }
        let mut conn = self.pool().acquire().await?;
        Ok(Some(
            worker_runs(&mut conn, project_id, worker_id, after, limit).await?,
        ))
    }

    /// One device session of a project, with its latest flows. `None` when it doesn't exist or
    /// belongs to another project.
    pub async fn device(
        &self,
        project_id: Uuid,
        device_id: Uuid,
    ) -> Result<Option<DeviceDetailView>> {
        let mut tx = self.pool().begin().await?;
        sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let Some(session) = device_sessions(&mut tx, project_id, None, Some(device_id), 1)
            .await?
            .pop()
        else {
            return Ok(None);
        };
        let ids = sqlx::query_scalar!(
            "SELECT id FROM flow_executions
              WHERE device_session_id = $1 AND project_id = $2
              ORDER BY (state = 'running') DESC, started_at DESC NULLS LAST, id DESC
              LIMIT 100",
            device_id,
            project_id
        )
        .fetch_all(&mut *tx)
        .await?;
        let mut by_id: HashMap<Uuid, FlowView> =
            live::flows(&mut tx, live::Scope::Ids(project_id, &ids))
                .await?
                .into_iter()
                .map(|f| (f.id, f))
                .collect();
        let flow_count = sqlx::query_scalar!(
            "SELECT count(*) AS \"n!\" FROM flow_executions
              WHERE device_session_id = $1 AND project_id = $2",
            device_id,
            project_id
        )
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(DeviceDetailView {
            session,
            flows: ids.iter().filter_map(|id| by_id.remove(id)).collect(),
            flow_count,
            // The newest sample is read by the server, which turns its readings into the view.
            latest: None,
        }))
    }
}

/// Runs of one worker, newest first: one page and the cursor of the next, if there is one.
async fn worker_runs(
    c: &mut sqlx::PgConnection,
    project: Uuid,
    worker: Uuid,
    after: Option<RunCursor>,
    limit: i64,
) -> Result<(Vec<WorkerRunView>, Option<RunCursor>)> {
    let rows = sqlx::query!(
        "SELECT r.id, r.started_at, r.ended_at, j.pipeline_attempt_id, j.job_key
           FROM agent_runs r JOIN jobs j ON j.id = r.job_id
          WHERE r.project_id = $1 AND r.worker_id = $2
            AND ($3::timestamptz IS NULL OR (r.started_at, r.id) < ($3, $4))
          ORDER BY r.started_at DESC, r.id DESC
          LIMIT $5",
        project,
        worker,
        after.map(|a| a.started_at),
        after.map(|a| a.id),
        limit + 1
    )
    .fetch_all(&mut *c)
    .await?;
    let more = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
    let page: Vec<_> = rows
        .into_iter()
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .collect();
    let next = more
        .then(|| {
            page.last().map(|r| RunCursor {
                started_at: r.started_at,
                id: r.id,
            })
        })
        .flatten();
    let ids: Vec<Uuid> = page.iter().map(|r| r.id).collect();
    let mut runs: HashMap<Uuid, AgentRunView> =
        live::agent_runs(c, live::Scope::Ids(project, &ids))
            .await?
            .into_iter()
            .map(|r| (r.id, r))
            .collect();
    let items = page
        .into_iter()
        .filter_map(|r| {
            Some(WorkerRunView {
                run: runs.remove(&r.id)?,
                pipeline_attempt_id: r.pipeline_attempt_id,
                job_key: r.job_key,
                started_at: r.started_at,
                ended_at: r.ended_at,
            })
        })
        .collect();
    Ok((items, next))
}

/// Device sessions of a project, newest first: those of one worker, or one by ID.
pub(super) async fn device_sessions(
    c: &mut sqlx::PgConnection,
    project: Uuid,
    worker: Option<Uuid>,
    device: Option<Uuid>,
    limit: i64,
) -> Result<Vec<DeviceSessionView>> {
    Ok(sqlx::query!(
        "SELECT d.id, d.agent_run_id, d.serial, d.state, d.rev, r.worker_id, w.runner_name,
                j.pipeline_attempt_id, j.id AS job_id, j.job_key, d.first_seen_at, d.last_seen_at,
                p.platform AS \"platform?\", p.kind AS \"kind?\", p.model, p.avd_or_runtime,
                p.os_version, p.api_level::int AS api_level, p.abi
           FROM device_sessions d
           JOIN agent_runs r ON r.id = d.agent_run_id
           JOIN jobs j ON j.id = r.job_id
           LEFT JOIN workers w ON w.id = r.worker_id
           LEFT JOIN device_profiles p ON p.id = d.profile_id
          WHERE d.project_id = $1
            AND ($2::uuid IS NULL OR r.worker_id = $2)
            AND ($3::uuid IS NULL OR d.id = $3)
          ORDER BY d.first_seen_at DESC, d.id DESC
          LIMIT $4",
        project,
        worker,
        device,
        limit
    )
    .fetch_all(c)
    .await?
    .into_iter()
    .map(|r| DeviceSessionView {
        device: DeviceView {
            id: r.id,
            agent_run_id: r.agent_run_id,
            serial: r.serial,
            state: r.state,
            rev: r.rev,
        },
        worker_id: r.worker_id,
        runner_name: r.runner_name,
        pipeline_attempt_id: r.pipeline_attempt_id,
        job_id: r.job_id,
        job_key: r.job_key,
        first_seen_at: r.first_seen_at,
        last_seen_at: r.last_seen_at,
        profile: match (r.platform, r.kind) {
            (Some(platform), Some(kind)) => Some(DeviceProfileView {
                platform,
                kind,
                model: r.model,
                avd_or_runtime: r.avd_or_runtime,
                os_version: r.os_version,
                api_level: r.api_level,
                abi: r.abi,
            }),
            _ => None,
        },
    })
    .collect())
}
