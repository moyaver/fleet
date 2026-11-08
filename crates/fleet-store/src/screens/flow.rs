// SPDX-License-Identifier: AGPL-3.0-only
//! Screen D: one flow execution with its claims, steps and failure.

use fleet_core::{
    flaky::{DEVICE_LOST, Exclusion, exclusion_of},
    reproduce::command_for,
};
use fleet_protocol::{
    event::{FlowOutcome, Reproduction},
    live::FlowView,
    rest::{
        ArtifactRefView, ClaimView, CrashView, FailureView, FlowDetailView, InfrastructureView,
        PreviousExecutionView, StepView,
    },
};
use uuid::Uuid;

use super::workers::device_sessions;
use crate::{Result, Store, live};

impl Store {
    /// The slug of the project a flow execution belongs to; `None` when there is no such flow.
    pub async fn flow_project_slug(&self, flow_id: Uuid) -> Result<Option<String>> {
        Ok(sqlx::query_scalar!(
            "SELECT p.slug FROM flow_executions f JOIN projects p ON p.id = f.project_id
              WHERE f.id = $1",
            flow_id
        )
        .fetch_optional(self.pool())
        .await?)
    }

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
                    device_session_id, flow_name, flow_path, started_at, ended_at,
                    crash_package, crash_exception, step_warnings, cut_short
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
        let stored_reproduction = sqlx::query_scalar!(
            "SELECT reproduction AS \"reproduction?: serde_json::Value\" FROM agent_runs WHERE id = $1",
            head.agent_run_id
        )
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
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
        let crash = match (head.crash_package, head.crash_exception) {
            (Some(package), Some(exception)) => Some(CrashView { package, exception }),
            _ => None,
        };
        let infrastructure = infrastructure_of(
            &flow.state,
            flow.outcome_source.as_deref(),
            &flow.finality,
            flow.failure_message.as_deref(),
            head.cut_short.as_deref(),
        );
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
            steps: nest(steps),
            failure,
            crash,
            infrastructure,
            step_warnings: head.step_warnings,
            previous,
            artifacts,
            reproduction: command_for(
                reproduction_of(stored_reproduction).as_ref(),
                head.flow_path.as_deref(),
            ),
        }))
    }
}

/// The rerun words a run stored, if it stored a readable set (issue #82).
pub(super) fn reproduction_of(stored: Option<serde_json::Value>) -> Option<Reproduction> {
    stored.and_then(|v| serde_json::from_value(v).ok())
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

/// A flow's steps in the order they were observed, flat: the nesting is [`nest`]'s job.
async fn steps_of(c: &mut sqlx::PgConnection, flow_id: Uuid) -> Result<Vec<StepView>> {
    Ok(sqlx::query!(
        "SELECT st.step_index, st.label, st.status, st.source::text AS \"source!\",
                st.started_at, st.ended_at, st.depth, st.parent_step_index, st.attempt
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
        parent_index: s.parent_step_index,
        depth: s.depth,
        attempt: s.attempt,
        children: Vec::new(),
    })
    .collect())
}

/// Nests flat steps under the steps that ran them (issue #38). A step whose parent is missing, or
/// not earlier, stays at the top, so no step is lost. Children keep their observed order.
fn nest(flat: Vec<StepView>) -> Vec<StepView> {
    let present: std::collections::HashSet<i32> = flat.iter().map(|s| s.index).collect();
    // A block starts before the steps it runs, so reading the steps backwards meets every child
    // before its block, and each block is complete when it is placed.
    let mut under = std::collections::BTreeMap::<i32, Vec<StepView>>::new();
    let mut roots = Vec::new();
    for mut step in flat.into_iter().rev() {
        step.children = under
            .remove(&step.index)
            .map(|mut c| {
                c.reverse();
                c
            })
            .unwrap_or_default();
        match step
            .parent_index
            .filter(|p| *p < step.index && present.contains(p))
        {
            Some(parent) => under.entry(parent).or_default().push(step),
            None => roots.push(step),
        }
    }
    roots.reverse();
    roots
}

/// The device loss that makes a failed flow's failure the device's (issue #145). It needs both
/// halves of the evidence, and the flow stays failed: this only labels it.
///
/// - The failure names the lost device (`DEVICE_LOST`), the same text rule flaky analysis
///   applies (ADR-0009), so the label and the exclusion agree on what a device loss is.
/// - The journal recorded that the flow's commands were cut short (`commands_not_final`), which
///   is what the agent saw when the device stopped answering. Without it a text match alone is
///   not enough to call the failure the device's.
///
/// The label carries the failure's source and finality, so it is as settled as the failure.
pub(super) fn infrastructure_of(
    state: &str,
    source: Option<&str>,
    finality: &str,
    failure_message: Option<&str>,
    cut_short: Option<&str>,
) -> Option<InfrastructureView> {
    if state != "failed"
        || exclusion_of(FlowOutcome::Failed, failure_message) != Some(Exclusion::Infrastructure)
    {
        return None;
    }
    let warning = cut_short?;
    Some(InfrastructureView {
        evidence: DEVICE_LOST.to_owned(),
        warning: warning.to_owned(),
        source: source.map(str::to_owned),
        finality: finality.to_owned(),
    })
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
        fingerprint: None,
        triage: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEATH: &str = "maestro.android.DeviceServerDiedException: Device server died";
    const CUT: &str = "run/debug/slow/commands.json is incomplete: the flow was cut short";
    const ASSERTION: &str = "Assertion is false: \"Done\" is visible";

    #[test]
    fn a_lost_device_is_labelled_with_its_recorded_warning_and_the_failures_provenance() {
        let label = infrastructure_of(
            "failed",
            Some("junit_final"),
            "verified",
            Some(DEATH),
            Some(CUT),
        )
        .expect("labelled");
        assert_eq!(label.evidence, DEVICE_LOST);
        assert_eq!(label.warning, CUT);
        assert_eq!(label.source.as_deref(), Some("junit_final"));
        assert_eq!(label.finality, "verified");
    }

    #[test]
    fn a_failure_text_alone_is_not_the_recorded_evidence() {
        assert_eq!(
            infrastructure_of("failed", Some("junit_final"), "verified", Some(DEATH), None),
            None,
            "the agent recorded no cut-short warning"
        );
    }

    #[test]
    fn an_assertion_or_an_interrupted_flow_is_never_the_devices() {
        assert_eq!(
            infrastructure_of(
                "failed",
                Some("junit_final"),
                "verified",
                Some(ASSERTION),
                Some(CUT)
            ),
            None,
            "an assertion is the app's"
        );
        assert_eq!(
            infrastructure_of("interrupted", None, "inferred", Some(DEATH), Some(CUT)),
            None,
            "an interrupted run is not a failed flow"
        );
        assert_eq!(
            infrastructure_of("passed", Some("junit_final"), "verified", None, Some(CUT)),
            None
        );
    }
}
