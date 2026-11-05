// SPDX-License-Identifier: AGPL-3.0-only
//! Screen D: one flow execution with its claims, steps and failure.

use fleet_protocol::{
    live::FlowView,
    rest::{
        ArtifactRefView, ClaimView, FailureView, FlowDetailView, PreviousExecutionView, StepView,
    },
};
use uuid::Uuid;

use super::workers::device_sessions;
use crate::{Result, Store, live};

impl Store {
    /// Screen D for one flow execution. `None` when it doesn't exist or belongs to another
    /// project. The agent run's `liveness` is `None` until the server fills it in.
    #[expect(
        clippy::too_many_lines,
        reason = "assemble the project-scoped flow and its evidence in one read transaction"
    )]
    pub async fn flow_detail(
        &self,
        project_id: Uuid,
        flow_id: Uuid,
    ) -> Result<Option<FlowDetailView>> {
        let mut tx = self.pool().begin().await?;
        sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let Some(head) = sqlx::query!(
            "SELECT pipeline_attempt_id, job_id, suite_execution_id, agent_run_id,
                    device_session_id, flow_name, flow_path, started_at, ended_at
               FROM flow_executions WHERE id = $1 AND project_id = $2",
            flow_id,
            project_id
        )
        .fetch_optional(&mut *tx)
        .await?
        else {
            return Ok(None);
        };
        let ids = |id: Uuid| [id];
        let corrupt = |what: &str| crate::Error::Corrupt(format!("flow {flow_id} has no {what}"));
        let flow = live::flows(&mut tx, live::Scope::Ids(project_id, &ids(flow_id)))
            .await?
            .pop()
            .ok_or_else(|| corrupt("view"))?;
        let attempt = live::attempts(
            &mut *tx,
            Some(project_id),
            Some(&ids(head.pipeline_attempt_id)),
            1,
        )
        .await?
        .pop()
        .ok_or_else(|| corrupt("attempt"))?;
        let job = live::jobs(&mut tx, live::Scope::Ids(project_id, &ids(head.job_id)))
            .await?
            .pop()
            .ok_or_else(|| corrupt("job"))?;
        let suite = live::suites(
            &mut tx,
            live::Scope::Ids(project_id, &ids(head.suite_execution_id)),
        )
        .await?
        .pop()
        .ok_or_else(|| corrupt("suite"))?;
        let agent_run = live::agent_runs(
            &mut tx,
            live::Scope::Ids(project_id, &ids(head.agent_run_id)),
        )
        .await?
        .pop()
        .ok_or_else(|| corrupt("agent run"))?;
        let device = match head.device_session_id {
            Some(d) => device_sessions(&mut tx, project_id, None, Some(d), 1)
                .await?
                .pop(),
            None => None,
        };
        let claims = claims_of(&mut tx, flow_id, &flow).await?;
        let steps = steps_of(&mut tx, flow_id).await?;
        let previous = sqlx::query_as!(
            PreviousExecutionView,
            "SELECT id AS flow_id, pipeline_attempt_id, state, outcome_source::text AS outcome_source,
                    finality::text AS \"finality!\", started_at AS \"started_at!\", duration_ms
               FROM flow_executions
              WHERE project_id = $1 AND id <> $2 AND started_at IS NOT NULL
                AND ($3::timestamptz IS NULL OR started_at < $3)
                AND CASE WHEN $4::text IS NOT NULL THEN flow_path = $4 ELSE flow_name = $5 END
              ORDER BY started_at DESC, id DESC
              LIMIT 10",
            project_id,
            flow_id,
            head.started_at,
            head.flow_path,
            head.flow_name
        )
        .fetch_all(&mut *tx)
        .await?;
        let artifacts = sqlx::query_as!(
            ArtifactRefView,
            "SELECT id, class, name FROM artifacts
              WHERE flow_execution_id = $1 AND project_id = $2 AND state = 'available'
              ORDER BY created_at, id",
            flow_id,
            project_id
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let failure = failure_of(&flow, &steps);
        Ok(Some(FlowDetailView {
            started_at: head.started_at,
            ended_at: head.ended_at,
            flow,
            attempt,
            job,
            suite,
            agent_run,
            device,
            claims,
            steps,
            failure,
            previous,
            artifacts,
        }))
    }
}

/// Every claim about a flow's outcome in arrival order, with the winner marked.
async fn claims_of(
    c: &mut sqlx::PgConnection,
    flow_id: Uuid,
    flow: &FlowView,
) -> Result<Vec<ClaimView>> {
    let mut claims: Vec<ClaimView> = sqlx::query!(
        "SELECT source::text AS \"source!\", outcome::text AS \"outcome!\",
                observed_at, recorded_at, detail->>'parser' AS parser
           FROM flow_claims WHERE flow_execution_id = $1
          ORDER BY claim_order, source::text",
        flow_id
    )
    .fetch_all(c)
    .await?
    .into_iter()
    .map(|c| ClaimView {
        source: c.source,
        outcome: c.outcome,
        observed_at: c.observed_at,
        recorded_at: c.recorded_at,
        parser: c.parser,
        winning: false,
    })
    .collect();
    // The winner is the newest claim of the resolved source that says the resolved state.
    let winner = claims
        .iter()
        .rposition(|c| Some(&c.source) == flow.outcome_source.as_ref() && c.outcome == flow.state);
    if let Some(c) = winner.and_then(|i| claims.get_mut(i)) {
        c.winning = true;
    }
    Ok(claims)
}

/// A flow's steps in the order they were observed.
async fn steps_of(c: &mut sqlx::PgConnection, flow_id: Uuid) -> Result<Vec<StepView>> {
    Ok(sqlx::query!(
        "SELECT st.step_index, st.label, st.status, st.source::text AS \"source!\",
                st.started_at, st.ended_at
           FROM step_executions st
           JOIN flow_executions f ON f.id = st.flow_execution_id
           JOIN agent_runs r ON r.id = f.agent_run_id
          WHERE st.flow_execution_id = $1 AND st.run_day = r.run_day
          ORDER BY st.step_index",
        flow_id
    )
    .fetch_all(c)
    .await?
    .into_iter()
    .map(|s| StepView {
        index: s.step_index,
        label: s.label,
        status: s.status,
        source: s.source,
        started_at: s.started_at,
        ended_at: s.ended_at,
        duration_ms: match (s.started_at, s.ended_at) {
            (Some(a), Some(b)) => i64::try_from((b - a).whole_milliseconds()).ok(),
            _ => None,
        },
    })
    .collect())
}

/// Why a flow failed and where: the first failed step and the last good one before it.
fn failure_of(flow: &FlowView, steps: &[StepView]) -> Option<FailureView> {
    if flow.state != "failed" && flow.failure_message.is_none() {
        return None;
    }
    let failed = steps.iter().find(|s| s.status == "failed");
    let last_ok = steps
        .iter()
        .filter(|s| s.status == "completed" && failed.is_none_or(|f| s.index < f.index))
        .map(|s| s.index)
        .max();
    Some(FailureView {
        message: flow.failure_message.clone(),
        failed_step_index: failed.map(|s| s.index),
        last_successful_step_index: last_ok,
    })
}
