// SPDX-License-Identifier: AGPL-3.0-only
//! Screen B: the swimlane timeline of one attempt.

use fleet_protocol::rest::{BarView, LaneView, TimelineView};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Result, Store};

impl Store {
    /// Screen B's swimlane for one attempt, consistent with the attempt's `version`. `None` when
    /// the attempt doesn't exist or belongs to another project.
    pub async fn timeline(
        &self,
        project_id: Uuid,
        attempt_id: Uuid,
    ) -> Result<Option<TimelineView>> {
        let mut tx = self.pool().begin().await?;
        sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let Some(version) = sqlx::query_scalar!(
            "SELECT version FROM pipeline_attempts WHERE id = $1 AND project_id = $2",
            attempt_id,
            project_id
        )
        .fetch_optional(&mut *tx)
        .await?
        else {
            return Ok(None);
        };
        let lanes = sqlx::query_as!(
            LaneView,
            "SELECT d.id, d.serial, d.state, d.agent_run_id, j.id AS job_id, j.job_key,
                    w.runner_name, d.first_seen_at, d.last_seen_at
               FROM device_sessions d
               JOIN agent_runs r ON r.id = d.agent_run_id
               JOIN jobs j ON j.id = r.job_id
               LEFT JOIN workers w ON w.id = r.worker_id
              WHERE j.pipeline_attempt_id = $1 AND d.project_id = $2
              ORDER BY d.first_seen_at, d.id",
            attempt_id,
            project_id
        )
        .fetch_all(&mut *tx)
        .await?;
        let bars = sqlx::query_as!(
            BarView,
            "SELECT f.id AS flow_id, f.device_session_id AS lane_id, f.flow_name AS name,
                    f.shard_number, f.ordinal, f.state, f.outcome_source::text AS outcome_source,
                    f.finality::text AS \"finality!\", f.has_conflict,
                    f.started_at AS \"started_at!\", f.ended_at, f.duration_ms
               FROM flow_executions f
              WHERE f.pipeline_attempt_id = $1 AND f.project_id = $2 AND f.started_at IS NOT NULL
              ORDER BY f.started_at, f.id",
            attempt_id,
            project_id
        )
        .fetch_all(&mut *tx)
        .await?;
        let not_started = sqlx::query_scalar!(
            "SELECT count(*) AS \"n!\" FROM flow_executions
              WHERE pipeline_attempt_id = $1 AND project_id = $2 AND started_at IS NULL",
            attempt_id,
            project_id
        )
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(TimelineView {
            attempt_id,
            version,
            generated_at: OffsetDateTime::now_utc(),
            lanes,
            bars,
            not_started,
        }))
    }
}
