// SPDX-License-Identifier: AGPL-3.0-only
//! Screen E: the history of finished flow executions, newest first, and the history rows flaky v1
//! classifies (ADR-0009). Thin SQL over the facts ingest stores; the verdicts are computed by the
//! caller with `fleet_core::flaky`, so the rule is written once.

use std::collections::BTreeSet;

use fleet_core::{claims::Finality, fingerprint::ConditionFingerprint, flaky::HistoryRun};
use fleet_protocol::{
    event::FlowOutcome,
    journal::SourceKind,
    rest::history::{
        DurationPoint, DurationSummary, FailureGroupView, HistoryEntryView, OutcomeCounts,
    },
};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::workers::RunCursor;
use crate::{Result, Store};

/// The history rows classified per flow: at most this many, the latest.
const CLASSIFIED_RUNS_PER_FLOW: i64 = 200;

/// Where a page of the history ends: the oldest execution it showed. Opaque to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCursor(RunCursor);

impl HistoryCursor {
    /// The token a client sends back for the next page.
    #[must_use]
    pub fn encode(&self) -> String {
        self.0.encode()
    }

    /// The cursor a token stands for; `None` for anything this server didn't issue.
    #[must_use]
    pub fn decode(token: &str) -> Option<Self> {
        RunCursor::decode(token).map(Self)
    }
}

/// What to list. Every filter that is `None` matches everything.
#[derive(Debug, Clone)]
pub struct HistoryQuery<'a> {
    /// Text the flow's path or name contains, ignoring case.
    pub flow: Option<&'a str>,
    /// A branch, as `main` or `refs/heads/main`.
    pub branch: Option<&'a str>,
    /// A flow state: `passed`, `failed`, `skipped`, `interrupted` or `unknown`.
    pub outcome: Option<&'a str>,
    /// Start of the window, inclusive.
    pub from: OffsetDateTime,
    /// End of the window, exclusive.
    pub to: OffsetDateTime,
    /// Continue after this entry.
    pub after: Option<HistoryCursor>,
    /// Entries to return.
    pub limit: i64,
}

/// A page of the history, and what flaky v1 needs to judge its flows.
#[derive(Debug, Clone)]
pub struct HistoryData {
    /// The executions, newest first.
    pub items: Vec<HistoryEntryView>,
    /// Set when more entries follow.
    pub next: Option<HistoryCursor>,
    /// The classifiable runs of the flows on the page.
    pub runs: Vec<HistoryRun>,
}

impl Store {
    /// One page of the project's history for `query`.
    pub async fn history(&self, project_id: Uuid, query: &HistoryQuery<'_>) -> Result<HistoryData> {
        let mut tx = self.pool().begin().await?;
        sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let after = query.after.map(|c| c.0.parts());
        let mut items: Vec<HistoryEntryView> = sqlx::query_as!(
            HistoryEntryView,
            "SELECT f.id AS flow_id, coalesce(d.repo_path, f.flow_name) AS \"flow!\", f.flow_name AS name,
                    f.pipeline_attempt_id AS attempt_id, pr.repository, pr.commit_sha, pr.git_ref,
                    f.state, f.outcome_source::text AS outcome_source,
                    f.finality::text AS \"finality!\",
                    coalesce(f.ended_at, f.started_at) AS \"at!\", f.duration_ms, f.failure_message,
                    ff.fingerprint AS \"failure_fingerprint?\",
                    encode(c.fingerprint, 'hex') AS condition_fingerprint
               FROM flow_executions f
               JOIN pipeline_attempts pa ON pa.id = f.pipeline_attempt_id
               JOIN pipeline_runs pr ON pr.id = pa.pipeline_run_id
               LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
               LEFT JOIN failure_fingerprints ff ON ff.flow_execution_id = f.id
               LEFT JOIN condition_fingerprints c ON c.id = f.condition_fingerprint_id
              WHERE f.project_id = $1
                AND f.state NOT IN ('pending', 'running')
                AND coalesce(f.ended_at, f.started_at) >= $2
                AND coalesce(f.ended_at, f.started_at) < $3
                AND ($4::text IS NULL
                     OR strpos(lower(coalesce(d.repo_path, f.flow_name)), lower($4)) > 0
                     OR strpos(lower(f.flow_name), lower($4)) > 0)
                AND ($5::text IS NULL OR pr.git_ref IN ($5, 'refs/heads/' || $5))
                AND ($6::text IS NULL OR f.state = $6)
                AND ($7::timestamptz IS NULL
                     OR (coalesce(f.ended_at, f.started_at), f.id) < ($7, $8))
              ORDER BY coalesce(f.ended_at, f.started_at) DESC, f.id DESC
              LIMIT $9",
            project_id,
            query.from,
            query.to,
            query.flow,
            query.branch,
            query.outcome,
            after.map(|(at, _)| at),
            after.map(|(_, id)| id),
            query.limit.saturating_add(1)
        )
        .fetch_all(&mut *tx)
        .await?;
        let more = usize::try_from(query.limit).is_ok_and(|l| items.len() > l);
        if more {
            items.truncate(usize::try_from(query.limit).unwrap_or(0));
        }
        let next = items
            .last()
            .filter(|_| more)
            .map(|e| HistoryCursor(RunCursor::new(e.at, e.flow_id)));
        let flows: Vec<String> = items
            .iter()
            .map(|i| i.flow.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let runs = classifiable_runs(&mut tx, project_id, &flows, query.to).await?;
        tx.commit().await?;
        Ok(HistoryData { items, next, runs })
    }
}

/// A flow's numbers over a window, and what flaky v1 needs to judge it.
#[derive(Debug, Clone)]
pub struct FlowStatsData {
    /// How its executions ended.
    pub counts: OutcomeCounts,
    /// How long it takes when it passes.
    pub duration: DurationSummary,
    /// The latest settled executions with a duration, oldest first.
    pub durations: Vec<DurationPoint>,
    /// The latest kinds of failure, most recent first.
    pub recent_failures: Vec<FailureGroupView>,
    /// The flow's classifiable runs before the end of the window.
    pub runs: Vec<HistoryRun>,
}

/// The chart carries at most this many points.
const CHART_POINTS: i64 = 200;
/// A flow's panel lists at most this many kinds of failure.
const FAILURE_GROUPS: i64 = 10;
/// A failure group names at most this many of its executions.
const GROUP_EXAMPLES: i32 = 5;

impl Store {
    /// The stats of the flow `flow` (its identity: repository path, else name) over
    /// `[from, to)`. `None` when the project has never run such a flow.
    pub async fn flow_stats(
        &self,
        project_id: Uuid,
        flow: &str,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Result<Option<FlowStatsData>> {
        let mut tx = self.pool().begin().await?;
        sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let known = sqlx::query_scalar!(
            "SELECT true AS \"known!\"
               FROM flow_executions f LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
              WHERE f.project_id = $1 AND coalesce(d.repo_path, f.flow_name) = $2
              LIMIT 1",
            project_id,
            flow
        )
        .fetch_optional(&mut *tx)
        .await?;
        if known.is_none() {
            return Ok(None);
        }
        let scope = Scope {
            project_id,
            flow,
            from,
            to,
        };
        let (counts, duration) = totals(&mut tx, &scope).await?;
        let durations = chart_points(&mut tx, &scope).await?;
        let recent_failures = failure_groups(&mut tx, &scope).await?;
        let runs = classifiable_runs(&mut tx, project_id, &[flow.to_owned()], to).await?;
        tx.commit().await?;
        Ok(Some(FlowStatsData {
            counts,
            duration,
            durations,
            recent_failures,
            runs,
        }))
    }
}

/// One flow in one window of one project.
struct Scope<'a> {
    project_id: Uuid,
    flow: &'a str,
    from: OffsetDateTime,
    to: OffsetDateTime,
}

async fn totals(
    c: &mut sqlx::PgConnection,
    s: &Scope<'_>,
) -> Result<(OutcomeCounts, DurationSummary)> {
    let totals = sqlx::query!(
            "WITH ran AS (
               SELECT f.state, f.duration_ms,
                      f.finality IN ('verified', 'unverified_final') AS settled
                 FROM flow_executions f
                 LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
                WHERE f.project_id = $1 AND coalesce(d.repo_path, f.flow_name) = $2
                  AND f.state NOT IN ('pending', 'running')
                  AND coalesce(f.ended_at, f.started_at) >= $3
                  AND coalesce(f.ended_at, f.started_at) < $4)
             SELECT count(*) FILTER (WHERE state = 'passed' AND settled) AS \"passed!\",
                    count(*) FILTER (WHERE state = 'failed' AND settled) AS \"failed!\",
                    count(*) FILTER (WHERE NOT (state IN ('passed', 'failed') AND settled)) AS \"other!\",
                    count(duration_ms) FILTER (WHERE state = 'passed' AND settled) AS \"samples!\",
                    round(percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_ms)
                          FILTER (WHERE state = 'passed' AND settled))::bigint AS p50_ms,
                    round(percentile_cont(0.9) WITHIN GROUP (ORDER BY duration_ms)
                          FILTER (WHERE state = 'passed' AND settled))::bigint AS p90_ms
               FROM ran",
            s.project_id,
            s.flow,
            s.from,
            s.to
        )
        .fetch_one(&mut *c)
        .await?;
    Ok((
        OutcomeCounts {
            passed: totals.passed,
            failed: totals.failed,
            other: totals.other,
        },
        DurationSummary {
            samples: totals.samples,
            p50_ms: totals.p50_ms,
            p90_ms: totals.p90_ms,
        },
    ))
}

async fn chart_points(c: &mut sqlx::PgConnection, s: &Scope<'_>) -> Result<Vec<DurationPoint>> {
    Ok(sqlx::query_as!(
        DurationPoint,
        "SELECT id AS flow_id, at AS \"at!\", duration_ms AS \"duration_ms!\", state AS \"state!\"
               FROM (SELECT f.id, coalesce(f.ended_at, f.started_at) AS at, f.duration_ms, f.state
                       FROM flow_executions f
                       LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
                      WHERE f.project_id = $1 AND coalesce(d.repo_path, f.flow_name) = $2
                        AND f.state IN ('passed', 'failed')
                        AND f.finality IN ('verified', 'unverified_final')
                        AND f.duration_ms IS NOT NULL
                        AND coalesce(f.ended_at, f.started_at) >= $3
                        AND coalesce(f.ended_at, f.started_at) < $4
                      ORDER BY coalesce(f.ended_at, f.started_at) DESC, f.id DESC
                      LIMIT $5) latest
              ORDER BY at, id",
        s.project_id,
        s.flow,
        s.from,
        s.to,
        CHART_POINTS
    )
    .fetch_all(&mut *c)
    .await?)
}

async fn failure_groups(
    c: &mut sqlx::PgConnection,
    s: &Scope<'_>,
) -> Result<Vec<FailureGroupView>> {
    Ok(sqlx::query_as!(
            FailureGroupView,
            "SELECT ff.fingerprint, min(ff.step_kind) AS \"step_kind!\", min(ff.message) AS \"message!\",
                    count(*) AS \"count!\",
                    min(coalesce(f.ended_at, f.started_at)) AS \"first_at!\",
                    max(coalesce(f.ended_at, f.started_at)) AS \"last_at!\",
                    (array_agg(f.id ORDER BY coalesce(f.ended_at, f.started_at) DESC, f.id DESC))[1:$5]
                        AS \"flow_ids!\"
               FROM failure_fingerprints ff
               JOIN flow_executions f ON f.id = ff.flow_execution_id
               LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
              WHERE ff.project_id = $1 AND coalesce(d.repo_path, f.flow_name) = $2
                AND coalesce(f.ended_at, f.started_at) >= $3
                AND coalesce(f.ended_at, f.started_at) < $4
              GROUP BY ff.fingerprint
              ORDER BY max(coalesce(f.ended_at, f.started_at)) DESC, ff.fingerprint
              LIMIT $6",
            s.project_id,
            s.flow,
            s.from,
            s.to,
            GROUP_EXAMPLES,
            FAILURE_GROUPS
        )
        .fetch_all(&mut *c)
        .await?)
}

/// How far back flaky v1 looks: the longest window a client can ask for.
pub const LOOKBACK: Duration = Duration::days(90);

/// The final passes and failures of `flows` before `before`, as flaky v1 wants them.
pub(super) async fn classifiable_runs(
    c: &mut sqlx::PgConnection,
    project_id: Uuid,
    flows: &[String],
    before: OffsetDateTime,
) -> Result<Vec<HistoryRun>> {
    let rows = sqlx::query!(
        "SELECT flow AS \"flow!\", flow_execution_id AS \"run!\", commit_sha,
                condition_fingerprint AS \"condition!\", state AS \"state!\",
                outcome_source::text AS outcome_source, finality::text AS \"finality!\",
                is_default_branch AS \"default_branch!\", ended_at AS \"ended_at!\"
           FROM (SELECT h.*, row_number() OVER (PARTITION BY h.flow
                                                ORDER BY h.ended_at DESC, h.flow_execution_id DESC) AS n
                   FROM flaky_history h
                  WHERE h.project_id = $1 AND h.flow = ANY($2)
                    AND h.ended_at < $3 AND h.ended_at >= $4) x
          WHERE n <= $5",
        project_id,
        flows,
        before,
        before - LOOKBACK,
        CLASSIFIED_RUNS_PER_FLOW
    )
    .fetch_all(c)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            Some(HistoryRun {
                run: r.run,
                flow: r.flow,
                commit: r.commit_sha,
                condition: ConditionFingerprint::from_bytes(r.condition.try_into().ok()?),
                outcome: match r.state.as_str() {
                    "passed" => FlowOutcome::Passed,
                    "failed" => FlowOutcome::Failed,
                    _ => return None,
                },
                source: serde_json::from_value::<SourceKind>(r.outcome_source?.into()).ok()?,
                finality: match r.finality.as_str() {
                    "verified" => Finality::Verified,
                    "unverified_final" => Finality::UnverifiedFinal,
                    "inferred" => Finality::Inferred,
                    _ => Finality::Provisional,
                },
                default_branch: r.default_branch,
                at_unix_ms: i64::try_from(r.ended_at.unix_timestamp_nanos() / 1_000_000)
                    .unwrap_or(i64::MAX),
                exclusion: None,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    #[sqlx::test(migrations = "../../migrations")]
    async fn history_page_can_use_its_project_and_time_index(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SET LOCAL enable_seqscan = off")
            .execute(&mut *tx)
            .await
            .unwrap();
        let plan: Vec<String> = sqlx::query_scalar(
            "EXPLAIN SELECT id FROM flow_executions
              WHERE project_id = '00000000-0000-0000-0000-000000000001'
                AND state NOT IN ('pending', 'running')
                AND coalesce(ended_at, started_at) >= '2026-01-01'::timestamptz
                AND coalesce(ended_at, started_at) < '2027-01-01'::timestamptz
              ORDER BY coalesce(ended_at, started_at) DESC, id DESC LIMIT 50",
        )
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        assert!(
            plan.iter()
                .any(|line| line.contains("flow_executions_history")),
            "{plan:?}"
        );
    }
}
