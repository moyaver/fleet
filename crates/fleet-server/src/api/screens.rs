// SPDX-License-Identifier: AGPL-3.0-only
//! Screens A–D's REST reads (PLAN §10.3): overview, timeline, worker and device detail, flow
//! investigation. Every route goes through [`Caller`] (the flow page reads its caller with
//! [`principal`], to redirect a browser) and `authorize(ReadRuns, project)`, and a project the
//! caller can't read looks missing.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use fleet_protocol::{
    live::Liveness,
    rest::{
        DeviceDetailView, FlowDetailView, OverviewView, TimelineView, WorkerCounts, WorkerRunPage,
        WorkerRunView, WorkerSummaryView, WorkerView,
    },
};
use fleet_store::screens::{RunCursor, WorkerRow};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    ApiError, error, internal,
    metrics::{device_latest_view, latest_readings},
    read_project,
};
use crate::{
    live::Hub,
    session::{Caller, principal},
    state::AppState,
};

/// Workers listed on the overview, most recently seen first.
const OVERVIEW_WORKERS: usize = 50;

#[derive(Debug, Deserialize)]
pub(super) struct OverviewQuery {
    recent: Option<i64>,
}

/// Screen A: active attempts with job and flow counts, recently finished attempts, runner health.
#[utoipa::path(get, path = "/api/v1/projects/{project}/overview",
    params(("project" = String, Path, description = "Project slug"),
           ("recent" = Option<i64>, Query,
            description = "Finished attempts to include (default 10, max 50)")),
    responses((status = 200, body = OverviewView), (status = 404, body = ApiError)))]
pub(super) async fn overview(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path(project): Path<String>,
    Query(q): Query<OverviewQuery>,
) -> Response {
    let project = match read_project(&state, &principal, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let data = match state
        .store
        .overview(project, q.recent.unwrap_or(10).clamp(0, 50))
        .await
    {
        Ok(d) => d,
        Err(e) => return internal(&e),
    };
    let workers: Vec<WorkerSummaryView> = data
        .workers
        .into_iter()
        .map(|w| with_liveness(&state.hub, w))
        .collect();
    let mut worker_counts = WorkerCounts::default();
    for w in &workers {
        match w.liveness {
            Some(Liveness::Online) => worker_counts.online += 1,
            Some(Liveness::Stale) => worker_counts.stale += 1,
            Some(Liveness::Disconnected) => worker_counts.disconnected += 1,
            Some(Liveness::Completed) | None => worker_counts.completed += 1,
        }
    }
    Json(OverviewView {
        generated_at: OffsetDateTime::now_utc(),
        active: data.active,
        active_total: data.active_total,
        recent: data.recent,
        worker_counts,
        workers: workers.into_iter().take(OVERVIEW_WORKERS).collect(),
        active_emulators: data.active_emulators,
    })
    .into_response()
}

/// Screen B's swimlane: a lane per device and a bar per started flow, with provenance.
#[utoipa::path(get, path = "/api/v1/projects/{project}/attempts/{attempt}/timeline",
    params(("project" = String, Path, description = "Project slug"),
           ("attempt" = Uuid, Path, description = "Pipeline attempt ID")),
    responses((status = 200, body = TimelineView), (status = 404, body = ApiError)))]
pub(super) async fn timeline(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path((project, attempt)): Path<(String, Uuid)>,
) -> Response {
    let project = match read_project(&state, &principal, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    match state.store.timeline(project, attempt).await {
        Ok(Some(t)) => Json(t).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "no such attempt"),
        Err(e) => internal(&e),
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct RunsQuery {
    cursor: Option<String>,
    limit: Option<i64>,
}

/// Screen C: one worker, its latest runs and the devices it hosted.
#[utoipa::path(get, path = "/api/v1/projects/{project}/workers/{worker}",
    params(("project" = String, Path, description = "Project slug"),
           ("worker" = Uuid, Path, description = "Worker ID")),
    responses((status = 200, body = WorkerView), (status = 404, body = ApiError)))]
pub(super) async fn worker(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path((project, worker)): Path<(String, Uuid)>,
) -> Response {
    let project = match read_project(&state, &principal, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    match state.store.worker(project, worker).await {
        Ok(Some(detail)) => Json(WorkerView {
            worker: with_liveness(&state.hub, detail.row),
            recent_runs: detail
                .recent_runs
                .into_iter()
                .map(|r| with_run_liveness(&state.hub, r))
                .collect(),
            devices: detail.devices,
        })
        .into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "no such worker"),
        Err(e) => internal(&e),
    }
}

/// Screen C: a worker's run history, newest first, in pages.
#[utoipa::path(get, path = "/api/v1/projects/{project}/workers/{worker}/runs",
    params(("project" = String, Path, description = "Project slug"),
           ("worker" = Uuid, Path, description = "Worker ID"),
           ("cursor" = Option<String>, Query, description = "`next_cursor` of the previous page"),
           ("limit" = Option<i64>, Query, description = "Runs per page (default 20, max 100)")),
    responses((status = 200, body = WorkerRunPage), (status = 400, body = ApiError),
              (status = 404, body = ApiError)))]
pub(super) async fn worker_runs(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path((project, worker)): Path<(String, Uuid)>,
    Query(q): Query<RunsQuery>,
) -> Response {
    let project = match read_project(&state, &principal, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let after = match q.cursor.as_deref().map(RunCursor::decode) {
        Some(Some(c)) => Some(c),
        Some(None) => return error(StatusCode::BAD_REQUEST, "unknown cursor"),
        None => None,
    };
    match state
        .store
        .worker_runs_page(project, worker, after, q.limit.unwrap_or(20).clamp(1, 100))
        .await
    {
        Ok(Some((items, next))) => Json(WorkerRunPage {
            items: items
                .into_iter()
                .map(|r| with_run_liveness(&state.hub, r))
                .collect(),
            next_cursor: next.map(|c| c.encode()),
        })
        .into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "no such worker"),
        Err(e) => internal(&e),
    }
}

/// Screen C: one device session and the flows that ran on it.
#[utoipa::path(get, path = "/api/v1/projects/{project}/devices/{device}",
    params(("project" = String, Path, description = "Project slug"),
           ("device" = Uuid, Path, description = "Device session ID")),
    responses((status = 200, body = DeviceDetailView), (status = 404, body = ApiError)))]
pub(super) async fn device(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path((project, device)): Path<(String, Uuid)>,
) -> Response {
    let project = match read_project(&state, &principal, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let mut view = match state.store.device(project, device).await {
        Ok(Some(d)) => d,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such device"),
        Err(e) => return internal(&e),
    };
    view.latest = match state.store.device_latest(project, device).await {
        Ok(latest) => latest.as_ref().map(device_latest_view),
        Err(e) => return internal(&e),
    };
    Json(view).into_response()
}

fn with_run_liveness(hub: &Hub, mut r: WorkerRunView) -> WorkerRunView {
    r.run.liveness = Some(hub.run_liveness(r.run.id, &r.run.state, r.run.last_frame_at));
    r
}

/// Screen D's address for a flow known only by its ID, such as the job summary's link (the agent
/// does not know the project's slug): a redirect to `/flows/{id}?project={slug}` for a caller who
/// may read the flow's project. Any other caller gets the answer for a flow that does not exist, so
/// the slug is never named to someone who could not read it.
///
/// A browser (`Accept: text/html`) with no session is sent to browser sign-in, which comes back to
/// this address, as the dashboard's own sign-in does. API clients keep their `401`.
pub(super) async fn flow_page(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path(execution_id): Path<Uuid>,
) -> Response {
    let who = match principal(&state, &method, &headers).await {
        Ok(who) => who,
        Err(refusal)
            if refusal.status() == StatusCode::UNAUTHORIZED
                && wants_html(&headers)
                && state.config.login.is_some() =>
        {
            return Redirect::to(&sign_in_path(execution_id)).into_response();
        }
        Err(refusal) => return *refusal,
    };
    let missing = || error(StatusCode::NOT_FOUND, "no such flow");
    let slug = match state.store.flow_project_slug(execution_id).await {
        Ok(Some(slug)) => slug,
        Ok(None) => return missing(),
        Err(e) => return internal(&e),
    };
    match read_project(&state, &who, &slug).await {
        Ok(_) => Redirect::to(&format!("/flows/{execution_id}?project={slug}")).into_response(),
        Err(_) => missing(),
    }
}

/// Browser sign-in for a flow's page: the sign-in route, returning to that page (`return_to` names a
/// path on this site, which the callback accepts).
fn sign_in_path(execution_id: Uuid) -> String {
    format!("/api/v1/auth/login?return_to=%2Fapi%2Fv1%2Fflows%2F{execution_id}%2Fpage")
}

/// Whether the client asked for a web page (a browser's navigation) rather than an API answer.
fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| {
            accept.split(',').any(|media| {
                media
                    .split(';')
                    .next()
                    .is_some_and(|m| m.trim().eq_ignore_ascii_case("text/html"))
            })
        })
}

/// Screen D: one flow execution with all its evidence.
#[utoipa::path(get, path = "/api/v1/projects/{project}/flows/{execution_id}",
    params(("project" = String, Path, description = "Project slug"),
           ("execution_id" = Uuid, Path, description = "Flow execution ID")),
    responses((status = 200, body = FlowDetailView), (status = 404, body = ApiError)))]
pub(super) async fn flow(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path((project, flow)): Path<(String, Uuid)>,
) -> Response {
    let project = match read_project(&state, &principal, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    match state.store.flow_detail(project, flow).await {
        Ok(Some(mut d)) => {
            d.agent_run.liveness = Some(state.hub.run_liveness(
                d.agent_run.id,
                &d.agent_run.state,
                d.agent_run.last_frame_at,
            ));
            // A final failure shows its fingerprint and what was recorded about it (issue #84).
            match state.store.execution_failure_triage(project, flow).await {
                Ok(Some(triage)) => {
                    if let Some(failure) = d.failure.as_mut() {
                        failure.fingerprint = Some(triage.fingerprint.clone());
                        failure.triage = Some(triage);
                    }
                }
                Ok(None) => {}
                Err(e) => return internal(&e),
            }
            Json(d).into_response()
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "no such flow"),
        Err(e) => internal(&e),
    }
}

/// A worker's display liveness: the best of its live runs (online over stale over
/// disconnected), `completed` when it has none.
fn with_liveness(hub: &Hub, row: WorkerRow) -> WorkerSummaryView {
    let rank = |l: Liveness| match l {
        Liveness::Online => 3,
        Liveness::Stale => 2,
        Liveness::Disconnected => 1,
        Liveness::Completed => 0,
    };
    let liveness = row
        .runs
        .iter()
        .map(|r| hub.run_liveness(r.id, &r.state, r.last_frame_at))
        .max_by_key(|l| rank(*l))
        .unwrap_or(Liveness::Completed);
    WorkerSummaryView {
        liveness: Some(liveness),
        latest: row.latest.as_ref().map(latest_readings),
        ..row.view
    }
}
