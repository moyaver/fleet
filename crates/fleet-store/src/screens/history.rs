// SPDX-License-Identifier: AGPL-3.0-only
//! Screen E: the history of finished flow executions, newest first, and the history rows flaky v1
//! classifies (ADR-0009). Thin SQL over the facts ingest stores; the verdicts are computed by the
//! caller with `fleet_core::flaky`, so the rule is written once.

use std::collections::BTreeSet;

use fleet_core::{
    claims::Finality,
    fingerprint::ConditionFingerprint,
    flaky::{HistoryRun, exclusion_of},
};
use fleet_protocol::{
    event::FlowOutcome,
    journal::SourceKind,
    rest::history::{
        DurationPoint, DurationSummary, FailureGroupView, HistoryEntryView, OutcomeCounts,
    },
};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::{flow::infrastructure_of, workers::RunCursor};
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

/// One history row as the query reads it: an entry before its infrastructure label, which is
/// judged from the state and the failure message (issue #145).
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
}

impl HistoryRow {
    fn into_entry(self) -> Result<HistoryEntryView> {
        Ok(HistoryEntryView {
            infrastructure: infrastructure_of(&self.state, self.failure_message.as_deref()),
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
        sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let after = query.after.map(|c| c.0.parts());
        let rows = sqlx::query_as!(
            HistoryRow,
            "SELECT f.id AS flow_id, coalesce(d.repo_path, f.flow_name) AS \"flow!\", f.flow_name AS name,
                    f.pipeline_attempt_id AS attempt_id, pr.repository, pr.commit_sha, pr.git_ref,
                    f.state, f.outcome_source::text AS outcome_source,
                    f.finality::text AS \"finality!\",
                    coalesce(f.ended_at, f.started_at) AS \"at!\", f.duration_ms, f.failure_message,
                    ff.fingerprint AS \"failure_fingerprint?\",
                    encode(c.fingerprint, 'hex') AS condition_fingerprint, r.origin
               FROM flow_executions f
               JOIN agent_runs r ON r.id = f.agent_run_id
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
    use sqlx::PgPool;
    use time::{Duration, OffsetDateTime};

    use super::HistoryQuery;
    use crate::{
        Store,
        auth::{Publisher, TrustLevel},
        ingest::{
            Transport,
            tests::{Agent, Script, SuiteIds, publisher, suite_run_scripted},
        },
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
                    flow: None,
                    branch: None,
                    outcome: None,
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
                    flow: None,
                    branch: None,
                    outcome: None,
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
                    flow: None,
                    branch: None,
                    outcome: None,
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
}
