// SPDX-License-Identifier: AGPL-3.0-only
//! Screen E: the history of finished flow executions, newest first, the flow catalog's filters
//! (issue #85), the project trend, and the history rows flaky v1 classifies (ADR-0009). Thin SQL
//! over the facts ingest stores; the verdicts are computed by the caller with `fleet_core::flaky`,
//! so the rule is written once.

use std::collections::{BTreeMap, BTreeSet};

use fleet_core::{
    claims::Finality,
    fingerprint::ConditionFingerprint,
    flaky::{self, FlakyConfig, HistoryRun, exclusion_of},
    reproduce::command_for,
};
use fleet_protocol::{
    event::FlowOutcome,
    journal::SourceKind,
    rest::history::{
        DurationPoint, DurationSummary, FailureGroupView, HistoryEntryView, HistoryTrendView,
        OutcomeCounts, TrendDayView,
    },
};
use sqlx::{Postgres, QueryBuilder};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::{
    flow::{infrastructure_of, reproduction_of},
    workers::RunCursor,
};
use crate::{Result, Store, live};

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

/// The filters a screen E query takes (issue #85). Every filter that is `None` matches everything;
/// a text filter matches what its name says (see each field). A filter reads only what the store
/// holds for the execution: a fact the agent never reported matches no filter that names it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Filters<'a> {
    /// Text the flow's path or name contains, ignoring case.
    pub flow: Option<&'a str>,
    /// A tag the flow's header carries, ignoring case.
    pub tag: Option<&'a str>,
    /// A branch, as `main` or `refs/heads/main`.
    pub branch: Option<&'a str>,
    /// A flow state: `passed`, `failed`, `skipped`, `interrupted` or `unknown`.
    pub outcome: Option<&'a str>,
    /// Text the device's model contains, ignoring case.
    pub device_model: Option<&'a str>,
    /// Text the device's OS version contains, ignoring case.
    pub os_version: Option<&'a str>,
    /// Text the runner's name contains, ignoring case.
    pub runner: Option<&'a str>,
    /// Text the device's serial contains, ignoring case.
    pub device: Option<&'a str>,
    /// The CI provider, `github_actions` or `generic`.
    pub ci_provider: Option<&'a str>,
    /// A commit SHA, or the start of one.
    pub commit: Option<&'a str>,
    /// A failure fingerprint, `v1:` and 16 hex digits.
    pub fingerprint: Option<&'a str>,
    /// A suite execution (one Maestro invocation), by its ID.
    pub suite: Option<Uuid>,
}

impl Filters<'_> {
    /// Whether a filter other than the flow's identity or tag is set: those narrow executions, not
    /// flows, so a flow with no matching execution still counts as known (issue #85).
    #[must_use]
    pub fn narrows_executions(&self) -> bool {
        self.branch.is_some()
            || self.outcome.is_some()
            || self.device_model.is_some()
            || self.os_version.is_some()
            || self.runner.is_some()
            || self.device.is_some()
            || self.ci_provider.is_some()
            || self.commit.is_some()
            || self.fingerprint.is_some()
            || self.suite.is_some()
    }
}

/// What to list.
#[derive(Debug, Clone, Copy)]
pub struct HistoryQuery<'a> {
    /// The filters.
    pub filters: Filters<'a>,
    /// Start of the window, inclusive.
    pub from: OffsetDateTime,
    /// End of the window, exclusive.
    pub to: OffsetDateTime,
    /// Continue after this entry.
    pub after: Option<HistoryCursor>,
    /// Entries to return.
    pub limit: i64,
}

/// The columns of a history row, and the joins every screen E query needs to judge the filters.
pub(crate) const FROM_EXECUTIONS: &str = "FROM flow_executions f
    JOIN agent_runs r ON r.id = f.agent_run_id
    JOIN pipeline_attempts pa ON pa.id = f.pipeline_attempt_id
    JOIN pipeline_runs pr ON pr.id = pa.pipeline_run_id
    LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
    LEFT JOIN failure_fingerprints ff ON ff.flow_execution_id = f.id
    LEFT JOIN condition_fingerprints c ON c.id = f.condition_fingerprint_id
    LEFT JOIN device_sessions ds ON ds.id = f.device_session_id
    LEFT JOIN device_profiles dp ON dp.id = ds.profile_id
    LEFT JOIN workers w ON w.id = r.worker_id";

/// Adds one `AND …` per filter that is set. Text filters bind their value, never splice it.
pub(crate) fn push_filters(qb: &mut QueryBuilder<Postgres>, f: &Filters<'_>) {
    if let Some(v) = f.flow {
        qb.push(" AND (strpos(lower(coalesce(d.repo_path, f.flow_name)), lower(")
            .push_bind(v)
            .push(")) > 0 OR strpos(lower(f.flow_name), lower(")
            .push_bind(v)
            .push(")) > 0)");
    }
    if let Some(v) = f.tag {
        qb.push(" AND EXISTS (SELECT 1 FROM unnest(coalesce(d.tags, '{}')) AS t WHERE lower(t) = lower(")
            .push_bind(v)
            .push("))");
    }
    if let Some(v) = f.branch {
        qb.push(" AND pr.git_ref IN (")
            .push_bind(v)
            .push(", 'refs/heads/' || ")
            .push_bind(v)
            .push(")");
    }
    if let Some(v) = f.outcome {
        qb.push(" AND f.state = ").push_bind(v);
    }
    for (value, column) in [
        (f.device_model, "coalesce(dp.model, '')"),
        (f.os_version, "coalesce(dp.os_version, '')"),
        (f.runner, "coalesce(w.runner_name, '')"),
        (f.device, "coalesce(ds.serial, '')"),
    ] {
        if let Some(v) = value {
            qb.push(" AND strpos(lower(")
                .push(column)
                .push("), lower(")
                .push_bind(v)
                .push(")) > 0");
        }
    }
    if let Some(v) = f.ci_provider {
        qb.push(" AND pr.provider = ").push_bind(v);
    }
    if let Some(v) = f.commit {
        qb.push(" AND starts_with(lower(coalesce(pr.commit_sha, '')), lower(")
            .push_bind(v)
            .push("))");
    }
    if let Some(v) = f.fingerprint {
        qb.push(" AND ff.fingerprint = ").push_bind(v);
    }
    if let Some(v) = f.suite {
        qb.push(" AND f.suite_execution_id = ").push_bind(v);
    }
}

/// One history row as the query reads it: an entry before its infrastructure label, which is
/// judged from the state, the failure message and the journal's cut-short warning (issue #145).
#[derive(sqlx::FromRow)]
struct HistoryRow {
    flow_id: Uuid,
    flow: String,
    name: String,
    attempt_id: Uuid,
    repository: Option<String>,
    commit_sha: Option<String>,
    git_ref: Option<String>,
    state: String,
    outcome_source: Option<String>,
    finality: String,
    at: OffsetDateTime,
    duration_ms: Option<i64>,
    failure_message: Option<String>,
    failure_fingerprint: Option<String>,
    condition_fingerprint: Option<String>,
    origin: String,
    cut_short: Option<String>,
    flow_path: Option<String>,
    reproduction: Option<serde_json::Value>,
    tags: Vec<String>,
    ci_provider: String,
    runner: Option<String>,
    device: Option<String>,
    device_model: Option<String>,
    os_version: Option<String>,
    suite_id: Uuid,
}

impl HistoryRow {
    fn into_entry(self) -> Result<HistoryEntryView> {
        let reproduction = command_for(
            reproduction_of(self.reproduction).as_ref(),
            self.flow_path.as_deref(),
        );
        Ok(HistoryEntryView {
            reproduction,
            infrastructure: infrastructure_of(
                &self.state,
                self.outcome_source.as_deref(),
                &self.finality,
                self.failure_message.as_deref(),
                self.cut_short.as_deref(),
            ),
            origin: live::origin_of(&self.origin)?,
            flow_id: self.flow_id,
            flow: self.flow,
            name: self.name,
            attempt_id: self.attempt_id,
            repository: self.repository,
            commit_sha: self.commit_sha,
            git_ref: self.git_ref,
            state: self.state,
            outcome_source: self.outcome_source,
            finality: self.finality,
            at: self.at,
            duration_ms: self.duration_ms,
            failure_message: self.failure_message,
            failure_fingerprint: self.failure_fingerprint,
            condition_fingerprint: self.condition_fingerprint,
            triage: None,
            tags: self.tags,
            ci_provider: Some(self.ci_provider),
            runner: self.runner,
            device: self.device,
            device_model: self.device_model,
            os_version: self.os_version,
            suite_id: Some(self.suite_id),
        })
    }
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
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let after = query.after.map(|c| c.0.parts());
        let mut qb = QueryBuilder::<Postgres>::new(
            "SELECT f.id AS flow_id, coalesce(d.repo_path, f.flow_name) AS flow, f.flow_name AS name,
                    f.pipeline_attempt_id AS attempt_id, pr.repository, pr.commit_sha, pr.git_ref,
                    f.state, f.outcome_source::text AS outcome_source,
                    f.finality::text AS finality,
                    coalesce(f.ended_at, f.started_at) AS at, f.duration_ms, f.failure_message,
                    ff.fingerprint AS failure_fingerprint,
                    encode(c.fingerprint, 'hex') AS condition_fingerprint, r.origin,
                    f.cut_short, f.flow_path, r.reproduction,
                    coalesce(d.tags, '{}') AS tags, pr.provider AS ci_provider, w.runner_name AS runner,
                    ds.serial AS device, dp.model AS device_model, dp.os_version,
                    f.suite_execution_id AS suite_id ",
        );
        qb.push(FROM_EXECUTIONS)
            .push(" WHERE f.project_id = ")
            .push_bind(project_id)
            .push(" AND f.state NOT IN ('pending', 'running')")
            .push(" AND coalesce(f.ended_at, f.started_at) >= ")
            .push_bind(query.from)
            .push(" AND coalesce(f.ended_at, f.started_at) < ")
            .push_bind(query.to);
        push_filters(&mut qb, &query.filters);
        if let Some((at, id)) = after {
            qb.push(" AND (coalesce(f.ended_at, f.started_at), f.id) < (")
                .push_bind(at)
                .push(", ")
                .push_bind(id)
                .push(")");
        }
        qb.push(" ORDER BY coalesce(f.ended_at, f.started_at) DESC, f.id DESC LIMIT ")
            .push_bind(query.limit.saturating_add(1));
        let rows: Vec<HistoryRow> = qb.build_query_as().fetch_all(&mut *tx).await?;
        let mut items: Vec<HistoryEntryView> = rows
            .into_iter()
            .map(HistoryRow::into_entry)
            .collect::<Result<_>>()?;
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

    /// The project's trend over the filtered runs (issue #85): per UTC day, the settled passes,
    /// failures and flaky runs, and the duration percentiles of the settled runs that reported
    /// one, and the same over the whole window. Trusted runs only (research 06 T2), and only
    /// settled results count (invariant I3), as the per-flow statistics do.
    pub async fn history_trend(
        &self,
        project_id: Uuid,
        filters: &Filters<'_>,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Result<HistoryTrendView> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let mut daily = QueryBuilder::<Postgres>::new(
            "SELECT to_char(date_trunc('day', coalesce(f.ended_at, f.started_at) AT TIME ZONE 'UTC'), 'YYYY-MM-DD') AS day,
                    count(*) FILTER (WHERE f.state = 'passed') AS passed,
                    count(*) FILTER (WHERE f.state = 'failed') AS failed,
                    round(percentile_cont(0.5) WITHIN GROUP (ORDER BY f.duration_ms))::bigint AS p50,
                    round(percentile_cont(0.95) WITHIN GROUP (ORDER BY f.duration_ms))::bigint AS p95 ",
        );
        settled_scope(&mut daily, project_id, from, to, filters);
        daily.push(" GROUP BY 1 ORDER BY 1");
        let rows: Vec<TrendRow> = daily.build_query_as().fetch_all(&mut *tx).await?;

        let mut whole = QueryBuilder::<Postgres>::new(
            "SELECT count(*) FILTER (WHERE f.state = 'passed') AS passed,
                    count(*) FILTER (WHERE f.state = 'failed') AS failed,
                    count(f.duration_ms) AS samples,
                    round(percentile_cont(0.5) WITHIN GROUP (ORDER BY f.duration_ms))::bigint AS p50,
                    round(percentile_cont(0.95) WITHIN GROUP (ORDER BY f.duration_ms))::bigint AS p95 ",
        );
        settled_scope(&mut whole, project_id, from, to, filters);
        let totals: WholeTotals = whole.build_query_as().fetch_one(&mut *tx).await?;

        // The flaky runs: every run the verdicts of the filtered flows name as evidence, counted on
        // the day it ended. The verdicts are the ones the history page shows.
        let mut flows = QueryBuilder::<Postgres>::new(
            "SELECT DISTINCT coalesce(d.repo_path, f.flow_name) AS flow ",
        );
        settled_scope(&mut flows, project_id, from, to, filters);
        let flows: Vec<String> = flows.build_query_scalar().fetch_all(&mut *tx).await?;
        let runs = classifiable_runs(&mut tx, project_id, &flows, to).await?;
        let evidence: Vec<Uuid> = flaky::classify(&runs, &FlakyConfig::default())
            .iter()
            .flat_map(|v| v.evidence.iter())
            .flat_map(|e| match e {
                flaky::Evidence::PassedAndFailed { passed, failed, .. } => passed
                    .iter()
                    .chain(failed.iter())
                    .copied()
                    .collect::<Vec<_>>(),
                flaky::Evidence::Flips { runs, .. } => runs.clone(),
            })
            .collect();
        let mut flaky_days: BTreeMap<String, i64> = BTreeMap::new();
        let mut flaky_total = 0;
        if !evidence.is_empty() {
            let mut counted = QueryBuilder::<Postgres>::new(
                "SELECT to_char(date_trunc('day', coalesce(f.ended_at, f.started_at) AT TIME ZONE 'UTC'), 'YYYY-MM-DD') AS day,
                        count(*) AS runs ",
            );
            settled_scope(&mut counted, project_id, from, to, filters);
            counted
                .push(" AND f.id = ANY(")
                .push_bind(evidence)
                .push(") GROUP BY 1");
            let rows: Vec<(String, i64)> = counted.build_query_as().fetch_all(&mut *tx).await?;
            for (day, n) in rows {
                flaky_total += n;
                flaky_days.insert(day, n);
            }
        }
        tx.commit().await?;

        let mut days: BTreeMap<String, TrendDayView> = BTreeMap::new();
        for r in rows {
            days.insert(
                r.day.clone(),
                TrendDayView {
                    day: r.day,
                    passed: r.passed,
                    failed: r.failed,
                    flaky: 0,
                    duration_p50_ms: r.p50,
                    duration_p95_ms: r.p95,
                },
            );
        }
        for (day, n) in flaky_days {
            days.entry(day.clone())
                .or_insert_with(|| TrendDayView {
                    day,
                    passed: 0,
                    failed: 0,
                    flaky: 0,
                    duration_p50_ms: None,
                    duration_p95_ms: None,
                })
                .flaky = n;
        }
        Ok(HistoryTrendView {
            from,
            to,
            days: days.into_values().collect(),
            passed: totals.passed,
            failed: totals.failed,
            flaky: flaky_total,
            duration_samples: totals.samples,
            duration_p50_ms: totals.p50,
            duration_p95_ms: totals.p95,
        })
    }
}

/// The settled, trusted executions of a project in a window that pass `filters`, as a `FROM … WHERE`
/// tail for a trend query.
fn settled_scope(
    qb: &mut QueryBuilder<Postgres>,
    project_id: Uuid,
    from: OffsetDateTime,
    to: OffsetDateTime,
    filters: &Filters<'_>,
) {
    qb.push(FROM_EXECUTIONS)
        .push(" WHERE f.project_id = ")
        .push_bind(project_id)
        .push(" AND f.state IN ('passed', 'failed')")
        .push(" AND f.finality IN ('verified', 'unverified_final')")
        .push(" AND r.trust_level = 'trusted'")
        .push(" AND coalesce(f.ended_at, f.started_at) >= ")
        .push_bind(from)
        .push(" AND coalesce(f.ended_at, f.started_at) < ")
        .push_bind(to);
    push_filters(qb, filters);
}

#[derive(sqlx::FromRow)]
struct TrendRow {
    day: String,
    passed: i64,
    failed: i64,
    p50: Option<i64>,
    p95: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct WholeTotals {
    passed: i64,
    failed: i64,
    samples: i64,
    p50: Option<i64>,
    p95: Option<i64>,
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
    ///
    /// Only runs whose trust is `trusted` count (research 06 T2): a pull request or untrusted
    /// run stays in the history but cannot move these numbers.
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
                 JOIN agent_runs r ON r.id = f.agent_run_id
                 LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
                WHERE f.project_id = $1 AND coalesce(d.repo_path, f.flow_name) = $2
                  AND r.trust_level = 'trusted'
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
                       JOIN agent_runs r ON r.id = f.agent_run_id
                       LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
                      WHERE f.project_id = $1 AND coalesce(d.repo_path, f.flow_name) = $2
                        AND r.trust_level = 'trusted'
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
    // `f.project_id` and the state filter match `flow_executions_history` (0010); without them
    // the planner cannot seek the window. Failure fingerprints exist only for final failures, so
    // the state filter drops nothing.
    let rows = sqlx::query!(
            "SELECT ff.fingerprint, min(ff.step_kind) AS \"step_kind!\", min(ff.message) AS \"message!\",
                    count(*) AS \"count!\",
                    min(coalesce(f.ended_at, f.started_at)) AS \"first_at!\",
                    max(coalesce(f.ended_at, f.started_at)) AS \"last_at!\",
                    (array_agg(f.id ORDER BY coalesce(f.ended_at, f.started_at) DESC, f.id DESC))[1:$5]
                        AS \"flow_ids!\"
               FROM failure_fingerprints ff
               JOIN flow_executions f ON f.id = ff.flow_execution_id
               JOIN agent_runs r ON r.id = f.agent_run_id
               LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
              WHERE ff.project_id = $1 AND f.project_id = $1
                AND r.trust_level = 'trusted'
                AND f.state NOT IN ('pending', 'running')
                AND coalesce(d.repo_path, f.flow_name) = $2
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
        .await?;
    // Triage is attached by the server, which reads it from `failure_triage`.
    Ok(rows
        .into_iter()
        .map(|r| FailureGroupView {
            fingerprint: r.fingerprint,
            step_kind: r.step_kind,
            message: r.message,
            count: r.count,
            first_at: r.first_at,
            last_at: r.last_at,
            flow_ids: r.flow_ids,
            triage: None,
        })
        .collect())
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
                is_default_branch AS \"default_branch!\", ended_at AS \"ended_at!\",
                failure_message
           FROM (SELECT h.*, fe.failure_message, row_number() OVER (PARTITION BY h.flow
                                                ORDER BY h.ended_at DESC, h.flow_execution_id DESC) AS n
                   FROM flaky_history h
                   JOIN flow_executions fe ON fe.id = h.flow_execution_id
                   JOIN agent_runs ar ON ar.id = fe.agent_run_id
                  WHERE h.project_id = $1 AND h.flow = ANY($2)
                    AND ar.trust_level = 'trusted'
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
            let outcome = match r.state.as_str() {
                "passed" => FlowOutcome::Passed,
                "failed" => FlowOutcome::Failed,
                _ => return None,
            };
            Some(HistoryRun {
                run: r.run,
                flow: r.flow,
                commit: r.commit_sha,
                condition: ConditionFingerprint::from_bytes(r.condition.try_into().ok()?),
                outcome,
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
                exclusion: exclusion_of(outcome, r.failure_message.as_deref()),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use fleet_core::flaky::{self, Flakiness, FlakyConfig};
    use fleet_protocol::{
        event::AdbState,
        frame::{CatalogFlow, CiProvider, DeviceSample, DeviceUnavailable, FlowCatalog, Telemetry},
        journal::ClockSample,
    };
    use sqlx::PgPool;
    use time::{Duration, OffsetDateTime};
    use uuid::Uuid;

    use super::{Filters, HistoryEntryView, HistoryQuery};
    use crate::{
        Store,
        auth::{Publisher, TrustLevel},
        ingest::{
            Transport,
            tests::{
                Agent, Script, SuiteIds, publisher, suite_run_scripted, suite_run_scripted_on,
            },
        },
        telemetry::TelemetryBatch,
    };

    /// Maestro's failure when the device's driver server died mid-flow (fixture `device-killed`).
    const DEVICE_DEATH: &str = "maestro.android.DeviceServerDiedException: Device server died \
        during 'viewHierarchy' on emulator-5602 (2267ms since last byte, connection age 34368ms): \
        StatusRuntimeException: UNAVAILABLE";
    const ASSERTION: &str = r#"Assertion is false: "Order confirmed" is visible"#;

    /// One run of `flows` on `commit`, under the same conditions every time.
    async fn run(store: &Store, p: &Publisher, run_id: &str, commit: &str, flows: &[Script<'_>]) {
        let mut a = Agent::new(run_id, "e2e");
        let mut hello = a.hello(None);
        hello.ci.sha = Some(commit.into());
        let opened = store
            .open_run(p, &hello, Transport::WebSocket)
            .await
            .unwrap();
        let events = suite_run_scripted(&mut a, SuiteIds::new(), flows);
        let applied = store.apply_batch(&opened.handle, &events).await.unwrap();
        assert!(applied.rejected.is_empty(), "{:?}", applied.rejected);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_flow_whose_only_failure_is_infrastructure_is_not_flaky(pool: PgPool) {
        let store = Store {
            pool,
            origin: uuid::Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        // Both flows pass on `abc`, then fail on `abc` under the same conditions. The login's
        // failure is the device dying, which says nothing about the test; the checkout's is the
        // app's, so it still counts.
        let pass = [
            Script {
                name: "login",
                failure: None,
                failed_step: None,
            },
            Script {
                name: "checkout",
                failure: None,
                failed_step: None,
            },
        ];
        let fail = [
            Script {
                name: "login",
                failure: Some(DEVICE_DEATH),
                failed_step: None,
            },
            Script {
                name: "checkout",
                failure: Some(ASSERTION),
                failed_step: None,
            },
        ];
        run(&store, &p, "400", "abc", &pass).await;
        run(&store, &p, "401", "abc", &fail).await;
        let now = OffsetDateTime::now_utc();
        let runs = store
            .history(
                p.project_id,
                &HistoryQuery {
                    filters: crate::screens::Filters {
                        flow: None,
                        branch: None,
                        outcome: None,
                        ..Default::default()
                    },
                    from: now - Duration::days(1),
                    to: now + Duration::hours(1),
                    after: None,
                    limit: 50,
                },
            )
            .await
            .unwrap()
            .runs;
        let verdict = |flow: &str| {
            flaky::classify(&runs, &FlakyConfig::default())
                .into_iter()
                .find(|f| f.flow == flow)
                .unwrap_or_else(|| panic!("no verdict for {flow}"))
        };
        // The lost device is counted as excluded, once, and it is the only excluded run.
        let login = verdict("flows/login.yaml");
        assert_eq!((login.flakiness, login.excluded), (Flakiness::NotFlaky, 1));
        let checkout = verdict("flows/checkout.yaml");
        assert_eq!(
            (checkout.flakiness, checkout.excluded),
            (Flakiness::Confirmed, 0)
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_pr_failure_moves_neither_the_stats_nor_the_flakiness_verdict(pool: PgPool) {
        let store = Store {
            pool,
            origin: uuid::Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        let pr = Publisher {
            trust: TrustLevel::Pr,
            ..p.clone()
        };
        let pass = [Script {
            name: "checkout",
            failure: None,
            failed_step: None,
        }];
        let fail = [Script {
            name: "checkout",
            failure: Some(ASSERTION),
            failed_step: None,
        }];
        // A passing trusted run, then the same conditions failing in a pull request.
        run(&store, &p, "500", "abc", &pass).await;
        run(&store, &pr, "501", "abc", &fail).await;
        let now = OffsetDateTime::now_utc();
        let (from, to) = (now - Duration::days(1), now + Duration::hours(1));
        let stats = store
            .flow_stats(p.project_id, "flows/checkout.yaml", from, to)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((stats.counts.passed, stats.counts.failed), (1, 0));
        assert_eq!(stats.recent_failures.len(), 0);
        let history = store
            .history(
                p.project_id,
                &HistoryQuery {
                    filters: crate::screens::Filters {
                        flow: None,
                        branch: None,
                        outcome: None,
                        ..Default::default()
                    },
                    from,
                    to,
                    after: None,
                    limit: 50,
                },
            )
            .await
            .unwrap();
        // The pull request's run is still in the history, as its own entry.
        assert_eq!(history.items.len(), 2);
        assert_eq!(
            flaky::classify(&history.runs, &FlakyConfig::default())[0].flakiness,
            Flakiness::NotFlaky
        );
        // The same failure from a trusted run does count.
        run(&store, &p, "502", "abc", &fail).await;
        let stats = store
            .flow_stats(p.project_id, "flows/checkout.yaml", from, to)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((stats.counts.passed, stats.counts.failed), (1, 1));
        assert_eq!(stats.recent_failures.len(), 1);
        let history = store
            .history(
                p.project_id,
                &HistoryQuery {
                    filters: crate::screens::Filters {
                        flow: None,
                        branch: None,
                        outcome: None,
                        ..Default::default()
                    },
                    from,
                    to,
                    after: None,
                    limit: 50,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            flaky::classify(&history.runs, &FlakyConfig::default())[0].flakiness,
            Flakiness::Confirmed
        );
    }

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

    #[sqlx::test(migrations = "../../migrations")]
    async fn flow_stats_failure_groups_can_use_its_project_and_time_index(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SET LOCAL enable_seqscan = off")
            .execute(&mut *tx)
            .await
            .unwrap();
        let plan: Vec<String> = sqlx::query_scalar(
            "EXPLAIN SELECT ff.fingerprint
               FROM failure_fingerprints ff
               JOIN flow_executions f ON f.id = ff.flow_execution_id
               LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
              WHERE ff.project_id = '00000000-0000-0000-0000-000000000001'
                AND f.project_id = '00000000-0000-0000-0000-000000000001'
                AND f.state NOT IN ('pending', 'running')
                AND coalesce(f.ended_at, f.started_at) >= '2026-01-01'::timestamptz
                AND coalesce(f.ended_at, f.started_at) < '2027-01-01'::timestamptz
                AND coalesce(d.repo_path, f.flow_name) = 'flows/login.yaml'",
        )
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        let plan = plan.join("\n");
        assert!(
            plan.contains("Index Scan using flow_executions_history on flow_executions f")
                && plan.contains("Index Cond: ((project_id = "),
            "{plan}"
        );
    }

    /// One run's shape: what the filters can tell apart.
    struct Seed<'a> {
        run: &'a str,
        job: &'a str,
        commit: &'a str,
        serial: &'a str,
        provider: CiProvider,
        catalog: FlowCatalog,
    }

    impl<'a> Seed<'a> {
        fn on(run: &'a str, commit: &'a str) -> Self {
            Self {
                run,
                job: "e2e",
                commit,
                serial: "emulator-5554",
                provider: CiProvider::Generic,
                catalog: FlowCatalog::default(),
            }
        }
    }

    /// Runs `flows` as `seed` says. Returns the run's suite and its agent run ID.
    async fn seed_run(
        store: &Store,
        p: &Publisher,
        seed: Seed<'_>,
        flows: &[Script<'_>],
    ) -> (SuiteIds, Uuid) {
        let mut a = Agent::new(seed.run, seed.job);
        let mut hello = a.hello(None);
        hello.ci.sha = Some(seed.commit.into());
        hello.ci.provider = seed.provider;
        hello.catalog = seed.catalog;
        let opened = store
            .open_run(p, &hello, Transport::WebSocket)
            .await
            .unwrap();
        let ids = SuiteIds::new();
        let events = suite_run_scripted_on(&mut a, ids, flows, seed.serial);
        let applied = store.apply_batch(&opened.handle, &events).await.unwrap();
        assert!(applied.rejected.is_empty(), "{:?}", applied.rejected);
        (ids, a.id)
    }

    /// Tells the store what the device of `session` is (its model and OS), as a telemetry sample.
    async fn report_device(
        store: &Store,
        p: &Publisher,
        run: Uuid,
        session: Uuid,
        model: &str,
        os: &str,
    ) {
        let sample = DeviceSample {
            model: Some(model.into()),
            os_version: Some(os.into()),
            at: ClockSample {
                wall: OffsetDateTime::now_utc() - Duration::seconds(60),
                mono_ms: 0,
            },
            device_session_id: session,
            adb_state: Some(AdbState::Device),
            boot_completed: None,
            guest_cpu_pct: None,
            guest_mem_available_bytes: None,
            emu_cpu_pct: None,
            emu_rss_bytes: None,
            avd_name: None,
            api_level: None,
            foreground_activity: None,
            shard: None,
            unavailable: DeviceUnavailable::default(),
        };
        let batch = TelemetryBatch {
            agent_run_id: run,
            project_id: p.project_id,
            worker_id: Uuid::nil(),
            batch: Telemetry {
                tseq_from: 1,
                tseq_to: 1,
                dropped_since_last: 0,
                samples: Vec::new(),
                devices: vec![sample],
            },
        };
        store.store_telemetry(&[batch]).await.unwrap();
    }

    /// Every history entry of the project in the last day, under `filters`.
    async fn history_of(
        store: &Store,
        p: &Publisher,
        filters: Filters<'_>,
    ) -> Vec<HistoryEntryView> {
        let now = OffsetDateTime::now_utc();
        store
            .history(
                p.project_id,
                &HistoryQuery {
                    filters,
                    from: now - Duration::days(1),
                    to: now + Duration::hours(1),
                    after: None,
                    limit: 50,
                },
            )
            .await
            .unwrap()
            .items
    }

    const LOGIN: Script<'static> = Script {
        name: "login",
        failure: None,
        failed_step: None,
    };
    const CHECKOUT: Script<'static> = Script {
        name: "checkout",
        failure: None,
        failed_step: None,
    };

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_flow_the_catalog_names_is_listed_before_it_runs_and_is_never_a_pass(pool: PgPool) {
        let store = Store {
            pool,
            origin: Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        let mut seed = Seed::on("700", "abc");
        seed.catalog = FlowCatalog {
            flows: vec![
                CatalogFlow {
                    path: "flows/login.yaml".into(),
                    name: Some("login".into()),
                    tags: vec!["smoke".into()],
                },
                CatalogFlow {
                    path: "flows/checkout.yaml".into(),
                    name: None,
                    tags: vec!["regression".into()],
                },
                CatalogFlow {
                    path: "flows/refund.yaml".into(),
                    name: None,
                    tags: Vec::new(),
                },
            ],
            truncated: false,
        };
        // Only login runs. A plan names one more flow, which has not run either.
        seed_run(&store, &p, seed, &[LOGIN]).await;
        let plan = fleet_protocol::plan::PlanManifest {
            plan_id: "catalog-plan".into(),
            total_flows: None,
            config_path: None,
            config_sha256: None,
            equal_count_estimated_seconds: None,
            shards: vec![fleet_protocol::plan::PlanShard {
                id: 0,
                estimated_seconds: None,
                flows: vec!["flows/planned.yaml".into()],
            }],
        };
        store
            .store_plan(&p, &plan, &crate::plans::PlanPipeline::default())
            .await
            .unwrap();

        let now = OffsetDateTime::now_utc();
        let page = store
            .flow_catalog(
                p.project_id,
                &crate::catalog::CatalogQuery {
                    filters: Filters::default(),
                    from: now - Duration::days(1),
                    to: now + Duration::hours(1),
                    after: None,
                    limit: 50,
                },
            )
            .await
            .unwrap();
        let by = |flow: &str| {
            page.items
                .iter()
                .find(|f| f.flow == flow)
                .unwrap_or_else(|| panic!("no catalog entry for {flow}: {page:?}"))
                .clone()
        };
        let login = by("flows/login.yaml");
        assert_eq!((login.executions, login.name.as_str()), (1, "login"));
        assert_eq!(login.tags, ["smoke"]);
        assert!(login.last.is_some());
        // Never run: no execution, no last run, and no pass.
        for never in [
            "flows/checkout.yaml",
            "flows/refund.yaml",
            "flows/planned.yaml",
        ] {
            let f = by(never);
            assert_eq!((f.executions, f.last.clone()), (0, None), "{never}");
        }
        assert_eq!(by("flows/checkout.yaml").tags, ["regression"]);
        // The history lists the one execution, and no outcome for the flows that never ran.
        let history = history_of(&store, &p, Filters::default()).await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].tags, ["smoke"]);

        // An execution filter keeps only the flows with a matching execution: never-run flows drop.
        let filtered = store
            .flow_catalog(
                p.project_id,
                &crate::catalog::CatalogQuery {
                    filters: Filters {
                        device_model: Some("pixel"),
                        ..Filters::default()
                    },
                    from: now - Duration::days(1),
                    to: now + Duration::hours(1),
                    after: None,
                    limit: 50,
                },
            )
            .await
            .unwrap();
        assert!(filtered.items.is_empty(), "{filtered:?}");
        // A tag filter narrows the flows themselves, so a never-run flow with the tag is listed.
        let tagged = store
            .flow_catalog(
                p.project_id,
                &crate::catalog::CatalogQuery {
                    filters: Filters {
                        tag: Some("regression"),
                        ..Filters::default()
                    },
                    from: now - Duration::days(1),
                    to: now + Duration::hours(1),
                    after: None,
                    limit: 50,
                },
            )
            .await
            .unwrap();
        assert_eq!(tagged.items.len(), 1);
        assert_eq!(tagged.items[0].executions, 0);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_device_model_filter_keeps_only_the_runs_on_that_model(pool: PgPool) {
        let store = Store {
            pool,
            origin: Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        let (a, agent_a) = seed_run(&store, &p, Seed::on("800", "abc"), &[LOGIN]).await;
        let (b, agent_b) = seed_run(&store, &p, Seed::on("801", "abc"), &[LOGIN]).await;
        report_device(&store, &p, agent_a, a.device, "Pixel_8", "15").await;
        report_device(&store, &p, agent_b, b.device, "Pixel_9", "15").await;
        assert_eq!(history_of(&store, &p, Filters::default()).await.len(), 2);
        let items = history_of(
            &store,
            &p,
            Filters {
                device_model: Some("pixel_8"),
                ..Filters::default()
            },
        )
        .await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].device_model.as_deref(), Some("Pixel_8"));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_os_version_filter_keeps_only_the_runs_on_that_version(pool: PgPool) {
        let store = Store {
            pool,
            origin: Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        let (a, agent_a) = seed_run(&store, &p, Seed::on("810", "abc"), &[LOGIN]).await;
        let (b, agent_b) = seed_run(&store, &p, Seed::on("811", "abc"), &[LOGIN]).await;
        report_device(&store, &p, agent_a, a.device, "Pixel_8", "15").await;
        report_device(&store, &p, agent_b, b.device, "Pixel_8", "16").await;
        let items = history_of(
            &store,
            &p,
            Filters {
                os_version: Some("16"),
                ..Filters::default()
            },
        )
        .await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].os_version.as_deref(), Some("16"));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_runner_filter_keeps_only_the_runs_of_that_runner(pool: PgPool) {
        let store = Store {
            pool,
            origin: Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        seed_run(
            &store,
            &p,
            Seed {
                job: "e2e-a",
                ..Seed::on("820", "abc")
            },
            &[LOGIN],
        )
        .await;
        seed_run(
            &store,
            &p,
            Seed {
                job: "e2e-b",
                ..Seed::on("821", "abc")
            },
            &[LOGIN],
        )
        .await;
        let items = history_of(
            &store,
            &p,
            Filters {
                runner: Some("runner-e2e-b"),
                ..Filters::default()
            },
        )
        .await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].runner.as_deref(), Some("runner-e2e-b"));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_failure_fingerprint_filter_keeps_only_that_failure(pool: PgPool) {
        let store = Store {
            pool,
            origin: Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        let fail = |message| Script {
            name: "checkout",
            failure: Some(message),
            failed_step: None,
        };
        seed_run(&store, &p, Seed::on("830", "abc"), &[fail(ASSERTION)]).await;
        seed_run(
            &store,
            &p,
            Seed::on("831", "abc"),
            &[fail("Element not found: Text matching regex: Checkout")],
        )
        .await;
        let all = history_of(&store, &p, Filters::default()).await;
        let print = |message: &str| {
            all.iter()
                .find(|e| e.failure_message.as_deref() == Some(message))
                .and_then(|e| e.failure_fingerprint.clone())
                .unwrap_or_else(|| panic!("no fingerprint for {message}"))
        };
        let (first, second) = (
            print(ASSERTION),
            print("Element not found: Text matching regex: Checkout"),
        );
        assert_ne!(first, second);
        let items = history_of(
            &store,
            &p,
            Filters {
                fingerprint: Some(&first),
                ..Filters::default()
            },
        )
        .await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].failure_message.as_deref(), Some(ASSERTION));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_commit_filter_keeps_only_the_runs_of_that_commit(pool: PgPool) {
        let store = Store {
            pool,
            origin: Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        seed_run(&store, &p, Seed::on("840", "aaa1111"), &[LOGIN]).await;
        seed_run(&store, &p, Seed::on("841", "bbb2222"), &[LOGIN]).await;
        let items = history_of(
            &store,
            &p,
            Filters {
                commit: Some("AAA1"),
                ..Filters::default()
            },
        )
        .await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].commit_sha.as_deref(), Some("aaa1111"));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_suite_filter_keeps_only_the_executions_of_that_suite(pool: PgPool) {
        let store = Store {
            pool,
            origin: Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        let (a, _) = seed_run(&store, &p, Seed::on("850", "abc"), &[LOGIN, CHECKOUT]).await;
        seed_run(&store, &p, Seed::on("851", "abc"), &[LOGIN, CHECKOUT]).await;
        let items = history_of(
            &store,
            &p,
            Filters {
                suite: Some(a.suite),
                ..Filters::default()
            },
        )
        .await;
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.suite_id == Some(a.suite)));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_device_filter_keeps_only_the_runs_on_that_serial(pool: PgPool) {
        let store = Store {
            pool,
            origin: Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        seed_run(
            &store,
            &p,
            Seed {
                serial: "emulator-5554",
                ..Seed::on("860", "abc")
            },
            &[LOGIN],
        )
        .await;
        seed_run(
            &store,
            &p,
            Seed {
                serial: "emulator-5556",
                ..Seed::on("861", "abc")
            },
            &[LOGIN],
        )
        .await;
        let items = history_of(
            &store,
            &p,
            Filters {
                device: Some("5556"),
                ..Filters::default()
            },
        )
        .await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].device.as_deref(), Some("emulator-5556"));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_tag_filter_keeps_only_the_flows_whose_header_carries_the_tag(pool: PgPool) {
        let store = Store {
            pool,
            origin: Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        let mut seed = Seed::on("870", "abc");
        seed.catalog = FlowCatalog {
            flows: vec![
                CatalogFlow {
                    path: "flows/login.yaml".into(),
                    name: None,
                    tags: vec!["smoke".into()],
                },
                CatalogFlow {
                    path: "flows/checkout.yaml".into(),
                    name: None,
                    tags: vec!["regression".into()],
                },
            ],
            truncated: false,
        };
        seed_run(&store, &p, seed, &[LOGIN, CHECKOUT]).await;
        let items = history_of(
            &store,
            &p,
            Filters {
                tag: Some("SMOKE"),
                ..Filters::default()
            },
        )
        .await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].flow, "flows/login.yaml");
        assert_eq!(items[0].tags, ["smoke"]);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_ci_provider_filter_keeps_only_the_runs_of_that_provider(pool: PgPool) {
        let store = Store {
            pool,
            origin: Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        seed_run(
            &store,
            &p,
            Seed {
                provider: CiProvider::GithubActions,
                ..Seed::on("880", "abc")
            },
            &[LOGIN],
        )
        .await;
        seed_run(&store, &p, Seed::on("881", "abc"), &[LOGIN]).await;
        let items = history_of(
            &store,
            &p,
            Filters {
                ci_provider: Some("github_actions"),
                ..Filters::default()
            },
        )
        .await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].ci_provider.as_deref(), Some("github_actions"));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_trend_counts_settled_trusted_runs_and_the_flaky_evidence_per_day(pool: PgPool) {
        let store = Store {
            pool,
            origin: Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        let pr = Publisher {
            trust: TrustLevel::Pr,
            ..p.clone()
        };
        // Checkout passes, then fails under the same conditions: flaky, both runs are evidence. A
        // pull request's failure of the same flow is untrusted: it counts nowhere.
        seed_run(&store, &p, Seed::on("900", "abc"), &[CHECKOUT]).await;
        seed_run(
            &store,
            &p,
            Seed::on("901", "abc"),
            &[Script {
                name: "checkout",
                failure: Some(ASSERTION),
                failed_step: None,
            }],
        )
        .await;
        seed_run(
            &store,
            &pr,
            Seed::on("902", "abc"),
            &[Script {
                name: "checkout",
                failure: Some(ASSERTION),
                failed_step: None,
            }],
        )
        .await;
        let now = OffsetDateTime::now_utc();
        let trend = store
            .history_trend(
                p.project_id,
                &Filters::default(),
                now - Duration::days(1),
                now + Duration::hours(1),
            )
            .await
            .unwrap();
        assert_eq!((trend.passed, trend.failed, trend.flaky), (1, 1, 2));
        assert_eq!(
            (
                trend.duration_samples,
                trend.duration_p50_ms,
                trend.duration_p95_ms
            ),
            (2, Some(1000), Some(1000))
        );
        assert_eq!(trend.days.len(), 1);
        let day = &trend.days[0];
        assert_eq!((day.passed, day.failed, day.flaky), (1, 1, 2));
        assert_eq!(
            (day.duration_p50_ms, day.duration_p95_ms),
            (Some(1000), Some(1000))
        );

        // A filter narrows the trend the same way it narrows the history: nothing matches here.
        let none = store
            .history_trend(
                p.project_id,
                &Filters {
                    device_model: Some("nothing"),
                    ..Filters::default()
                },
                now - Duration::days(1),
                now + Duration::hours(1),
            )
            .await
            .unwrap();
        assert_eq!(
            (none.passed, none.failed, none.flaky, none.days.len()),
            (0, 0, 0, 0)
        );
    }
}
