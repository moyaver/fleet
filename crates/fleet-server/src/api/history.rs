// SPDX-License-Identifier: AGPL-3.0-only
//! Screens E and F's REST reads (PLAN §10.3): the history of flow executions, a flow's stats and
//! the comparison of two attempts. Every route goes through [`Caller`] and
//! `authorize(ReadRuns, project)`, and a project the caller can't read looks missing.
//!
//! Windows are bounded: a request names at most 90 days, and a page at most 200 executions.
//! Flakiness is `fleet_core::flaky` over the store's history rows; nothing is classified here.

use std::collections::BTreeSet;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use fleet_core::flaky::{self, Evidence, Flakiness, FlakyConfig, HistoryRun};
use fleet_protocol::rest::history::{
    Flakiness as FlakinessView, FlakyEvidenceView, FlowFlakinessView, FlowStatsView, HistoryPage,
};
use fleet_store::screens::{HistoryCursor, HistoryQuery, LOOKBACK};
use serde::Deserialize;
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

use super::{ApiError, error, internal, read_project, triage::triage_of};
use crate::{session::Caller, state::AppState};

/// The window a request gets when it names none.
const DEFAULT_WINDOW: Duration = Duration::days(30);
/// The most executions one page carries.
const MAX_PAGE: i64 = 200;
/// The page size when a request names none.
const DEFAULT_PAGE: i64 = 50;
/// The flow states a client can filter on.
const OUTCOMES: [&str; 5] = ["passed", "failed", "skipped", "interrupted", "unknown"];

#[derive(Debug, Deserialize)]
pub(super) struct HistoryParams {
    flow: Option<String>,
    branch: Option<String>,
    outcome: Option<String>,
    from: Option<String>,
    to: Option<String>,
    cursor: Option<String>,
    limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct WindowParams {
    from: Option<String>,
    to: Option<String>,
}

/// An RFC 3339 instant, or a 400 saying which parameter is wrong.
fn instant(name: &str, value: Option<&str>) -> Result<Option<OffsetDateTime>, Box<Response>> {
    value
        .map(|v| {
            OffsetDateTime::parse(v, &Rfc3339).map_err(|_| {
                Box::new(error(
                    StatusCode::BAD_REQUEST,
                    format!("`{name}` is not an RFC 3339 time"),
                ))
            })
        })
        .transpose()
}

/// The `[from, to)` window of a request: `to` defaults to now and `from` to 30 days before it.
fn window(
    from: Option<&str>,
    to: Option<&str>,
    now: OffsetDateTime,
) -> Result<(OffsetDateTime, OffsetDateTime), Box<Response>> {
    let to = instant("to", to)?.unwrap_or(now);
    let from = instant("from", from)?.unwrap_or(to - DEFAULT_WINDOW);
    if from >= to {
        return Err(Box::new(error(
            StatusCode::BAD_REQUEST,
            "`from` must be before `to`",
        )));
    }
    if to - from > LOOKBACK {
        return Err(Box::new(error(
            StatusCode::BAD_REQUEST,
            "the window is longer than 90 days",
        )));
    }
    Ok((from, to))
}

/// Screen E: finished flow executions over time, with provenance, fingerprints and flakiness.
#[utoipa::path(get, path = "/api/v1/projects/{project}/history",
    params(("project" = String, Path, description = "Project slug"),
           ("flow" = Option<String>, Query, description = "Text the flow's path or name contains"),
           ("branch" = Option<String>, Query, description = "A branch, as `main` or `refs/heads/main`"),
           ("outcome" = Option<String>, Query,
            description = "`passed`, `failed`, `skipped`, `interrupted` or `unknown`"),
           ("from" = Option<String>, Query, description = "Window start, RFC 3339 (default: 30 days before `to`)"),
           ("to" = Option<String>, Query, description = "Window end, RFC 3339 (default: now); at most 90 days after `from`"),
           ("cursor" = Option<String>, Query, description = "`next_cursor` of the previous page"),
           ("limit" = Option<i64>, Query, description = "Executions per page (default 50, max 200)")),
    responses((status = 200, body = HistoryPage), (status = 400, body = ApiError),
              (status = 404, body = ApiError)))]
pub(super) async fn history(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path(project): Path<String>,
    Query(q): Query<HistoryParams>,
) -> Response {
    let project = match read_project(&state, &principal, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let (from, to) = match window(
        q.from.as_deref(),
        q.to.as_deref(),
        OffsetDateTime::now_utc(),
    ) {
        Ok(w) => w,
        Err(r) => return *r,
    };
    if let Some(o) = q.outcome.as_deref()
        && !OUTCOMES.contains(&o)
    {
        return error(StatusCode::BAD_REQUEST, "unknown outcome");
    }
    let after = match q
        .cursor
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(HistoryCursor::decode)
    {
        Some(Some(c)) => Some(c),
        Some(None) => return error(StatusCode::BAD_REQUEST, "unknown cursor"),
        None => None,
    };
    let query = HistoryQuery {
        flow: q.flow.as_deref().filter(|f| !f.is_empty()),
        branch: q.branch.as_deref().filter(|b| !b.is_empty()),
        outcome: q.outcome.as_deref(),
        from,
        to,
        after,
        limit: q.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE),
    };
    match state.store.history(project, &query).await {
        Ok(data) => {
            let flows: BTreeSet<String> = data.items.iter().map(|i| i.flow.clone()).collect();
            let flows = flakiness(&data.runs, flows.iter().map(String::as_str));
            let mut items = data.items;
            let fingerprints: Vec<String> = items
                .iter()
                .filter_map(|i| i.failure_fingerprint.clone())
                .collect();
            let triage = match triage_of(&state, project, &fingerprints).await {
                Ok(t) => t,
                Err(r) => return *r,
            };
            for item in &mut items {
                item.triage = item
                    .failure_fingerprint
                    .as_ref()
                    .and_then(|f| triage.get(f).cloned());
            }
            Json(HistoryPage {
                from,
                to,
                flows,
                items,
                next_cursor: data.next.map(|c| c.encode()),
            })
            .into_response()
        }
        Err(e) => internal(&e),
    }
}

/// Screen E's panel for one flow: counts, duration percentiles, the chart's points and the
/// failures grouped by fingerprint. `name` is the flow's identity, as the history lists it.
#[utoipa::path(get, path = "/api/v1/projects/{project}/flows/{name}/stats",
    params(("project" = String, Path, description = "Project slug"),
           ("name" = String, Path, description = "The flow's identity (repository path, else name), percent-encoded"),
           ("from" = Option<String>, Query, description = "Window start, RFC 3339 (default: 30 days before `to`)"),
           ("to" = Option<String>, Query, description = "Window end, RFC 3339 (default: now); at most 90 days after `from`")),
    responses((status = 200, body = FlowStatsView), (status = 400, body = ApiError),
              (status = 404, body = ApiError)))]
pub(super) async fn flow_stats(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path((project, name)): Path<(String, String)>,
    Query(q): Query<WindowParams>,
) -> Response {
    let project = match read_project(&state, &principal, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let (from, to) = match window(
        q.from.as_deref(),
        q.to.as_deref(),
        OffsetDateTime::now_utc(),
    ) {
        Ok(w) => w,
        Err(r) => return *r,
    };
    match state.store.flow_stats(project, &name, from, to).await {
        Ok(Some(mut data)) => {
            let fingerprints: Vec<String> = data
                .recent_failures
                .iter()
                .map(|f| f.fingerprint.clone())
                .collect();
            let triage = match triage_of(&state, project, &fingerprints).await {
                Ok(t) => t,
                Err(r) => return *r,
            };
            for failure in &mut data.recent_failures {
                failure.triage = triage.get(&failure.fingerprint).cloned();
            }
            let flakiness = flakiness(&data.runs, [name.as_str()])
                .pop()
                .unwrap_or_else(|| FlowFlakinessView {
                    flow: name.clone(),
                    flakiness: FlakinessView::NotFlaky,
                    evidence: Vec::new(),
                    excluded: 0,
                });
            let flaky_commits = flakiness
                .evidence
                .iter()
                .filter(|e| matches!(e, FlakyEvidenceView::PassedAndFailed { .. }))
                .count();
            Json(FlowStatsView {
                flow: name,
                from,
                to,
                counts: data.counts,
                flakiness,
                flaky_commits: i64::try_from(flaky_commits).unwrap_or(i64::MAX),
                duration: data.duration,
                durations: data.durations,
                recent_failures: data.recent_failures,
            })
            .into_response()
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "no such flow"),
        Err(e) => internal(&e),
    }
}

/// The verdict of each of `flows` over `runs`, in flow order; a flow with no counted run is not
/// flaky.
pub(super) fn flakiness<'a>(
    runs: &[HistoryRun],
    flows: impl IntoIterator<Item = &'a str>,
) -> Vec<FlowFlakinessView> {
    let verdicts = flaky::classify(runs, &FlakyConfig::default());
    flows
        .into_iter()
        .map(|flow| {
            verdicts.iter().find(|v| v.flow == flow).map_or_else(
                || FlowFlakinessView {
                    flow: flow.to_owned(),
                    flakiness: FlakinessView::NotFlaky,
                    evidence: Vec::new(),
                    excluded: 0,
                },
                |v| FlowFlakinessView {
                    flow: v.flow.clone(),
                    flakiness: match v.flakiness {
                        Flakiness::NotFlaky => FlakinessView::NotFlaky,
                        Flakiness::Suspected => FlakinessView::Suspected,
                        Flakiness::Confirmed => FlakinessView::Confirmed,
                    },
                    evidence: v.evidence.iter().map(evidence).collect(),
                    excluded: i64::try_from(v.excluded).unwrap_or(i64::MAX),
                },
            )
        })
        .collect()
}

fn evidence(e: &Evidence) -> FlakyEvidenceView {
    match e {
        Evidence::PassedAndFailed {
            commit,
            condition,
            passed,
            failed,
        } => FlakyEvidenceView::PassedAndFailed {
            commit: commit.clone(),
            condition: condition.to_string(),
            passed: passed.clone(),
            failed: failed.clone(),
        },
        Evidence::Flips {
            condition,
            runs,
            flips,
        } => FlakyEvidenceView::Flips {
            condition: condition.to_string(),
            runs: runs.clone(),
            flips: *flips,
        },
    }
}
