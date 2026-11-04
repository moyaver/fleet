// SPDX-License-Identifier: AGPL-3.0-only
//! Screen F: the flows of two attempts side by side. The store returns each side's latest
//! execution per flow with the facts that explain a difference (conditions, failure fingerprint,
//! provenance); the caller turns them into rows and explanations.

use std::collections::BTreeMap;

use fleet_protocol::{live::AttemptView, rest::history::CompareFlowView};
use uuid::Uuid;

use crate::{Result, Store, live};

/// A side shows at most this many flows.
pub const SIDE_LIMIT: i64 = 5_000;

/// One flow on one side, with the dimensions of its condition fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideFlow {
    /// The flow's identity: its repository path when known, else its name.
    pub flow: String,
    /// Its latest execution.
    pub view: CompareFlowView,
    /// The condition fingerprint's dimensions; empty without a fingerprint.
    pub dims: BTreeMap<String, String>,
}

/// One compared attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareSide {
    /// The attempt.
    pub attempt: AttemptView,
    /// Its flows, sorted by identity.
    pub flows: Vec<SideFlow>,
    /// More flows than [`SIDE_LIMIT`] exist.
    pub truncated: bool,
}

impl Store {
    /// Both attempts' flows. `None` when either doesn't exist in the project.
    pub async fn compare(
        &self,
        project_id: Uuid,
        a: Uuid,
        b: Uuid,
    ) -> Result<Option<(CompareSide, CompareSide)>> {
        let mut tx = self.pool().begin().await?;
        sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let Some(a) = side(&mut tx, project_id, a).await? else {
            return Ok(None);
        };
        let Some(b) = side(&mut tx, project_id, b).await? else {
            return Ok(None);
        };
        tx.commit().await?;
        Ok(Some((a, b)))
    }
}

async fn side(
    c: &mut sqlx::PgConnection,
    project_id: Uuid,
    attempt: Uuid,
) -> Result<Option<CompareSide>> {
    let Some(attempt_view) = live::attempts(&mut *c, Some(project_id), Some(&[attempt]), 1)
        .await?
        .pop()
    else {
        return Ok(None);
    };
    let rows = sqlx::query!(
        "SELECT DISTINCT ON (coalesce(d.repo_path, f.flow_name))
                coalesce(d.repo_path, f.flow_name) AS \"flow!\", f.id, f.flow_name, f.state,
                f.outcome_source::text AS outcome_source, f.finality::text AS \"finality!\",
                f.duration_ms, f.failure_message,
                encode(c.fingerprint, 'hex') AS condition_fingerprint, c.dims AS \"dims?\",
                ff.fingerprint AS \"failure_fingerprint?\",
                count(*) OVER (PARTITION BY coalesce(d.repo_path, f.flow_name)) AS \"executions!\"
           FROM flow_executions f
           LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
           LEFT JOIN condition_fingerprints c ON c.id = f.condition_fingerprint_id
           LEFT JOIN failure_fingerprints ff ON ff.flow_execution_id = f.id
          WHERE f.project_id = $1 AND f.pipeline_attempt_id = $2
          ORDER BY coalesce(d.repo_path, f.flow_name), f.started_at DESC NULLS LAST, f.id DESC
          LIMIT $3",
        project_id,
        attempt,
        SIDE_LIMIT + 1
    )
    .fetch_all(c)
    .await?;
    let truncated = i64::try_from(rows.len()).is_ok_and(|n| n > SIDE_LIMIT);
    let flows = rows
        .into_iter()
        .take(usize::try_from(SIDE_LIMIT).unwrap_or(usize::MAX))
        .map(|r| SideFlow {
            flow: r.flow,
            dims: r
                .dims
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default(),
            view: CompareFlowView {
                flow_id: r.id,
                name: r.flow_name,
                state: r.state,
                outcome_source: r.outcome_source,
                finality: r.finality,
                duration_ms: r.duration_ms,
                executions: r.executions,
                condition_fingerprint: r.condition_fingerprint,
                failure_fingerprint: r.failure_fingerprint,
                failure_message: r.failure_message,
            },
        })
        .collect();
    Ok(Some(CompareSide {
        attempt: attempt_view,
        flows,
        truncated,
    }))
}
