// SPDX-License-Identifier: AGPL-3.0-only
//! Re-deriving state from evidence, inside the ingest transaction (research 05 §6, §7).
//!
//! Flow outcomes are `fleet_core::claims::resolve` over the stored claims; suite outcomes and job
//! lifecycles are `fleet_core::outcome`. Each function writes only rows whose derived state
//! actually changed, and returns their IDs for the live hub.

use std::collections::HashMap;

use fleet_core::{
    claims::{self, Claim, Finality, FlowState},
    outcome::{self, JobLifecycle, JobOutcome, RunFacts, RunState, SuiteFacts, SuiteOutcome},
};
use fleet_protocol::{
    event::{ExitClass, FlowOutcome},
    journal::SourceKind,
};
use sqlx::PgConnection;
use uuid::Uuid;

use super::apply::wire;

fn parse<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).ok()
}

/// The stored name of a resolved state.
pub(crate) fn state_name(state: FlowState) -> String {
    match state {
        FlowState::Pending => "pending".into(),
        FlowState::Running => "running".into(),
        FlowState::Ended(FlowOutcome::Unrecognized) => "unknown".into(),
        FlowState::Ended(o) => wire(o),
    }
}

/// The stored state as the lattice's type, for suite derivation.
fn flow_state(s: &str) -> FlowState {
    match s {
        "pending" => FlowState::Pending,
        "running" => FlowState::Running,
        other => FlowState::Ended(parse(other).unwrap_or(FlowOutcome::Unknown)),
    }
}

fn finality_name(f: Finality) -> &'static str {
    match f {
        Finality::Provisional => "provisional",
        Finality::Verified => "verified",
        Finality::UnverifiedFinal => "unverified_final",
        Finality::Inferred => "inferred",
    }
}

/// Re-resolves flows from their claims.
pub(crate) async fn flows(tx: &mut PgConnection, ids: &[Uuid]) -> Result<Vec<Uuid>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query!(
        "SELECT f.id, f.started, s.lifecycle = 'closed' AS \"closed!\"
           FROM flow_executions f JOIN suite_executions s ON s.id = f.suite_execution_id
          WHERE f.id = ANY($1)",
        ids
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut claims_by_flow: HashMap<Uuid, Vec<Claim>> = HashMap::new();
    for c in sqlx::query!(
        "SELECT flow_execution_id, source AS \"source!\", outcome AS \"outcome!\", claim_order
           FROM flow_claims WHERE flow_execution_id = ANY($1)",
        ids
    )
    .fetch_all(&mut *tx)
    .await?
    {
        let (Some(source), Some(outcome)) = (
            parse::<SourceKind>(&c.source),
            parse::<FlowOutcome>(&c.outcome),
        ) else {
            continue;
        };
        if let Ok(claim) = Claim::new(source, outcome, u64::try_from(c.claim_order).unwrap_or(0)) {
            claims_by_flow
                .entry(c.flow_execution_id)
                .or_default()
                .push(claim);
        }
    }
    let (mut fid, mut state, mut source, mut finality, mut conflict) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for row in rows {
        let claims = claims_by_flow.remove(&row.id).unwrap_or_default();
        let r = claims::resolve(&claims, row.started, row.closed);
        fid.push(row.id);
        state.push(state_name(r.state));
        source.push(r.source.map(wire));
        finality.push(finality_name(r.finality).to_owned());
        conflict.push(r.conflict);
    }
    let changed = sqlx::query_scalar!(
        "UPDATE flow_executions f
            SET state = u.state, outcome_source = u.source, finality = u.finality,
                has_conflict = u.conflict, rev = f.rev + 1
           FROM unnest($1::uuid[], $2::text[], $3::text[], $4::text[], $5::bool[])
             AS u(id, state, source, finality, conflict)
          WHERE f.id = u.id
            AND (f.state, f.outcome_source, f.finality::text, f.has_conflict)
                IS DISTINCT FROM (u.state, u.source, u.finality, u.conflict)
          RETURNING f.id",
        &fid,
        &state,
        &source as &[Option<String>],
        &finality,
        &conflict
    )
    .fetch_all(&mut *tx)
    .await?;
    // Failed flows' evidence is kept longer (research 05 §12.3), decided in the same transaction.
    crate::artifacts::promote_failed_evidence(tx, &changed).await?;
    Ok(changed)
}

fn suite_name(o: SuiteOutcome) -> &'static str {
    match o {
        SuiteOutcome::Passed => "passed",
        SuiteOutcome::Failed => "failed",
        SuiteOutcome::Interrupted => "interrupted",
        SuiteOutcome::Unknown => "unknown",
    }
}

/// Re-derives suite outcomes once Maestro has exited.
pub(crate) async fn suites(tx: &mut PgConnection, ids: &[Uuid]) -> Result<Vec<Uuid>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query!(
        "SELECT s.id, s.exit_code, s.exit_class, s.report_status,
                coalesce(array_agg(f.state) FILTER (WHERE f.id IS NOT NULL), '{}') AS \"flows!\"
           FROM suite_executions s LEFT JOIN flow_executions f ON f.suite_execution_id = s.id
          WHERE s.id = ANY($1)
          GROUP BY s.id",
        ids
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut changed = Vec::new();
    for row in rows {
        let flows: Vec<FlowState> = row.flows.iter().map(|s| flow_state(s)).collect();
        let facts = SuiteFacts {
            exit_code: row.exit_code,
            exit_class: row.exit_class.as_deref().and_then(parse::<ExitClass>),
            report_complete: row.report_status.as_deref() == Some("complete"),
            flows: &flows,
        };
        let Some((o, source)) = outcome::suite_outcome(&facts) else {
            continue;
        };
        let updated = sqlx::query_scalar!(
            "UPDATE suite_executions SET outcome = $2, outcome_source = $3, rev = rev + 1
              WHERE id = $1 AND (outcome, outcome_source::text) IS DISTINCT FROM ($2, $3)
              RETURNING id",
            row.id,
            suite_name(o),
            wire(source)
        )
        .fetch_optional(&mut *tx)
        .await?;
        changed.extend(updated);
    }
    Ok(changed)
}

/// Busy while a flow runs on the device, ready otherwise; offline, released and lost stay.
pub(crate) async fn devices(tx: &mut PgConnection, ids: &[Uuid]) -> Result<Vec<Uuid>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_scalar!(
        "WITH next AS (
           SELECT d.id,
                  CASE WHEN EXISTS (SELECT 1 FROM flow_executions f
                                     WHERE f.device_session_id = d.id AND f.state = 'running')
                       THEN 'busy' ELSE 'ready' END AS state
             FROM device_sessions d
            WHERE d.id = ANY($1) AND d.state IN ('ready', 'busy'))
         UPDATE device_sessions d SET state = next.state, rev = d.rev + 1
           FROM next
          WHERE d.id = next.id AND d.state <> next.state
          RETURNING d.id",
        ids
    )
    .fetch_all(&mut *tx)
    .await
}

fn run_state(s: &str) -> RunState {
    match s {
        "finalizing" => RunState::Finalizing,
        "ended" => RunState::Ended,
        "abandoned" => RunState::Abandoned,
        _ => RunState::Open,
    }
}

fn lifecycle_name(l: JobLifecycle) -> &'static str {
    match l {
        JobLifecycle::Starting => "starting",
        JobLifecycle::Running => "running",
        JobLifecycle::Finalizing => "finalizing",
        JobLifecycle::Closed => "closed",
    }
}

fn job_outcome_name(o: JobOutcome) -> &'static str {
    match o {
        JobOutcome::Completed => "completed",
        JobOutcome::Failed => "failed",
        JobOutcome::Cancelled => "cancelled",
        JobOutcome::Interrupted => "interrupted",
        JobOutcome::Unknown => "unknown",
    }
}

/// Re-derives a job's lifecycle and outcome from its runs and suites. Returns whether it changed.
pub(crate) async fn job(tx: &mut PgConnection, job_id: Uuid) -> Result<bool, sqlx::Error> {
    let runs: Vec<RunFacts> = sqlx::query!(
        "SELECT r.state, coalesce(r.signal_received IN (2, 15), false) AS \"cancelled!\",
                EXISTS (SELECT 1 FROM suite_executions s
                         WHERE s.agent_run_id = r.id AND s.lifecycle <> 'closed') AS \"suite_open!\"
           FROM agent_runs r WHERE r.job_id = $1",
        job_id
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|r| RunFacts {
        state: run_state(&r.state),
        cancelled: r.cancelled,
        suite_open: r.suite_open,
    })
    .collect();
    let suites: Vec<Option<SuiteOutcome>> = sqlx::query_scalar!(
        "SELECT outcome FROM suite_executions WHERE job_id = $1",
        job_id
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|o| match o.as_deref() {
        Some("passed") => Some(SuiteOutcome::Passed),
        Some("failed") => Some(SuiteOutcome::Failed),
        Some("interrupted") => Some(SuiteOutcome::Interrupted),
        Some(_) => Some(SuiteOutcome::Unknown),
        None => None,
    })
    .collect();
    let (lifecycle, outcome) = outcome::job_state(&runs, &suites);
    let updated = sqlx::query_scalar!(
        "UPDATE jobs
            SET lifecycle = $2, outcome = $3, outcome_source = $4,
                closed_at = CASE WHEN $2 = 'closed' THEN coalesce(closed_at, now()) END,
                reopened_count = reopened_count + (lifecycle = 'closed' AND $2 <> 'closed')::int,
                rev = rev + 1
          WHERE id = $1
            AND (lifecycle, outcome, outcome_source::text) IS DISTINCT FROM ($2, $3, $4)
          RETURNING id",
        job_id,
        lifecycle_name(lifecycle),
        outcome.map(|o| job_outcome_name(o.0)),
        outcome.map(|o| wire(o.1))
    )
    .fetch_optional(&mut *tx)
    .await?;
    Ok(updated.is_some())
}
