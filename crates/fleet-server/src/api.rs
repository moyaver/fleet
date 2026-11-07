// SPDX-License-Identifier: AGPL-3.0-only
//! REST API (PLAN §7.1 `api/`) and its OpenAPI document (`/api/v1/openapi.json`).
//!
//! Reads need a caller with a role (`fleet_core::policy`): a signed-in browser session, a
//! configured viewer token, or anonymous reads in development. Agent credentials are write-only
//! and are refused here. `POST /api/v1/plans` takes a publisher credential: the planning CI job
//! stores its plan like an agent publishes.

mod compare;
mod history;
pub(crate) mod live_ws;
mod metrics;
mod screens;

#[cfg(test)]
mod tests;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use fleet_core::policy::{Action, Decision, Denied, Principal, Resource, authorize};
use fleet_protocol::{
    artifact::{RejectReason, UploadGrant, UploadRequest, UploadResult},
    live::{
        AgentRunView, AttemptView, DeviceView, EntityView, FlowView, JobView, Liveness,
        PlanShardView, PlanView, ProjectView, Snapshot, SuiteView,
    },
    plan::PlanManifest,
    rest::{
        ActiveAttemptView, ArtifactLinkView, ArtifactListView, ArtifactRefView, ArtifactView,
        BarView, ClaimView, CrashView, DeviceDetailView, DeviceProfileView, DeviceSessionView,
        FailureView, FlowCounts, FlowDetailView, JobCounts, LaneView, LatestReadings,
        MetricAggregate, MetricPoint, MetricReading, MetricResolution, MetricScope, MetricSubject,
        MetricSubjectKind, MetricUnit, MetricsView, OverviewView, PreviousExecutionView,
        RecentAttemptView, SeriesView, StepView, TimelineView, WorkerCounts, WorkerRunPage,
        WorkerRunView, WorkerSummaryView, WorkerView,
        history::{
            ChangeKind, CompareFlowView, CompareRowView, CompareSideView, CompareView,
            DimChangeView, DurationPoint, DurationSummary, EvidenceView, Explanation,
            FailureGroupView, Flakiness, FlakyEvidenceView, FlowFlakinessView, FlowStatsView,
            HistoryEntryView, HistoryPage, OutcomeCounts, Side,
        },
    },
};
use fleet_store::plans::{PlanError, PlanPipeline};
use serde::{Deserialize, Serialize};
use utoipa::{OpenApi, ToSchema};
use uuid::Uuid;

use crate::{ingest::bearer, session::Caller, state::AppState};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Fleet",
        description = "Live observability for mobile UI tests in CI. Agents use the WebSocket \
                       protocol `fleet.v1+json` (`/api/v1/agent/ws`); this document covers REST.",
        license(name = "AGPL-3.0-only", identifier = "AGPL-3.0-only")
    ),
    paths(
        projects,
        attempts,
        snapshot,
        attempt_plans,
        store_plan,
        screens::overview,
        screens::timeline,
        screens::worker,
        screens::worker_runs,
        screens::device,
        metrics::worker_metrics,
        metrics::device_metrics,
        screens::flow,
        crate::artifacts::list::flow_artifacts,
        crate::artifacts::upload::request,
        crate::artifacts::upload::content,
        crate::artifacts::upload::complete,
        history::history,
        history::flow_stats,
        compare::compare
    ),
    components(schemas(
        ProjectView,
        AttemptView,
        Snapshot,
        JobView,
        AgentRunView,
        SuiteView,
        DeviceView,
        FlowView,
        PlanView,
        PlanShardView,
        EntityView,
        Liveness,
        OverviewView,
        ActiveAttemptView,
        RecentAttemptView,
        JobCounts,
        FlowCounts,
        WorkerCounts,
        WorkerSummaryView,
        LatestReadings,
        MetricReading,
        TimelineView,
        LaneView,
        BarView,
        WorkerView,
        WorkerRunView,
        WorkerRunPage,
        DeviceSessionView,
        DeviceProfileView,
        DeviceDetailView,
        MetricsView,
        MetricSubject,
        MetricSubjectKind,
        SeriesView,
        MetricPoint,
        MetricUnit,
        MetricScope,
        MetricResolution,
        MetricAggregate,
        FlowDetailView,
        ClaimView,
        StepView,
        FailureView,
        CrashView,
        PreviousExecutionView,
        ArtifactRefView,
        ArtifactView,
        ArtifactLinkView,
        ArtifactListView,
        UploadRequest,
        UploadGrant,
        UploadResult,
        RejectReason,
        HistoryPage,
        HistoryEntryView,
        FlowStatsView,
        CompareView,
        CompareSideView,
        CompareRowView,
        CompareFlowView,
        ChangeKind,
        Explanation,
        EvidenceView,
        DimChangeView,
        Side,
        OutcomeCounts,
        DurationSummary,
        DurationPoint,
        FailureGroupView,
        FlowFlakinessView,
        FlakyEvidenceView,
        Flakiness,
        PlanUpload,
        PlanStored,
        ApiError
    ))
)]
pub(crate) struct ApiDoc;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/openapi.json", get(openapi))
        .route("/api/v1/projects", get(projects))
        .route("/api/v1/projects/{project}/attempts", get(attempts))
        .route(
            "/api/v1/projects/{project}/attempts/{attempt}",
            get(snapshot),
        )
        .route(
            "/api/v1/projects/{project}/attempts/{attempt}/plans",
            get(attempt_plans),
        )
        .route(
            "/api/v1/projects/{project}/overview",
            get(screens::overview),
        )
        .route(
            "/api/v1/projects/{project}/attempts/{attempt}/timeline",
            get(screens::timeline),
        )
        .route(
            "/api/v1/projects/{project}/workers/{worker}",
            get(screens::worker),
        )
        .route(
            "/api/v1/projects/{project}/workers/{worker}/runs",
            get(screens::worker_runs),
        )
        .route(
            "/api/v1/projects/{project}/devices/{device}",
            get(screens::device),
        )
        .route(
            "/api/v1/projects/{project}/workers/{worker}/metrics",
            get(metrics::worker_metrics),
        )
        .route(
            "/api/v1/projects/{project}/devices/{device}/metrics",
            get(metrics::device_metrics),
        )
        .route(
            "/api/v1/projects/{project}/flows/{execution_id}",
            get(screens::flow),
        )
        .route(
            "/api/v1/projects/{project}/flows/{execution_id}/artifacts",
            get(crate::artifacts::list::flow_artifacts),
        )
        .route("/api/v1/projects/{project}/history", get(history::history))
        .route("/api/v1/projects/{project}/compare", get(compare::compare))
        .route(
            "/api/v1/projects/{project}/flows/{name}/stats",
            get(history::flow_stats),
        )
        .route("/api/v1/plans", post(store_plan))
        .route("/api/v1/live", get(live_ws::upgrade))
}

async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

/// An error body.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct ApiError {
    /// What went wrong.
    error: String,
}

pub(crate) fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ApiError {
            error: message.into(),
        }),
    )
        .into_response()
}

fn internal(e: &dyn std::fmt::Display) -> Response {
    tracing::error!(error = %e, "api request failed");
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

/// What a refusal looks like. Having no role on a project reads as "no such project", so one
/// team can't probe another's project names; a role that is too low says so.
fn denied(d: Denied) -> Response {
    match d {
        Denied::NoRole => error(StatusCode::NOT_FOUND, "no such project"),
        Denied::NeedsRole(_) | Denied::WrongResource => {
            error(StatusCode::FORBIDDEN, "your role does not allow this")
        }
    }
}

/// `Ok` when `principal` may do `action` to `resource`.
pub(crate) fn require(
    principal: &Principal,
    action: Action,
    resource: Resource,
) -> Result<(), Box<Response>> {
    match authorize(principal, action, resource) {
        Decision::Allow => Ok(()),
        Decision::Deny(d) => Err(Box::new(denied(d))),
    }
}

pub(crate) async fn project_id(state: &AppState, slug: &str) -> Result<Uuid, Box<Response>> {
    match state.store.project_id(slug).await {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err(Box::new(error(StatusCode::NOT_FOUND, "no such project"))),
        Err(e) => Err(Box::new(internal(&e))),
    }
}

/// The project a caller may read, by slug. A project that doesn't exist and one the caller has no
/// role on are the same answer: 404.
async fn read_project(
    state: &AppState,
    principal: &Principal,
    slug: &str,
) -> Result<Uuid, Box<Response>> {
    let project = project_id(state, slug).await?;
    require(principal, Action::ReadRuns, Resource::Project(project))?;
    Ok(project)
}

/// Every project.
#[utoipa::path(get, path = "/api/v1/projects", responses(
    (status = 200, body = [ProjectView]), (status = 401, body = ApiError)))]
async fn projects(State(state): State<AppState>, Caller(principal): Caller) -> Response {
    if let Err(r) = require(&principal, Action::ListProjects, Resource::Global) {
        return *r;
    }
    match state.store.projects().await {
        Ok(p) => Json(
            p.into_iter()
                .filter(|p| {
                    authorize(&principal, Action::ReadRuns, Resource::Project(p.id))
                        == Decision::Allow
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal(&e),
    }
}

#[derive(Debug, Deserialize)]
struct Limit {
    limit: Option<i64>,
}

/// A project's recent pipeline attempts, newest first.
#[utoipa::path(get, path = "/api/v1/projects/{project}/attempts",
    params(("project" = String, Path, description = "Project slug"),
           ("limit" = Option<i64>, Query, description = "At most this many (default 50, max 500)")),
    responses((status = 200, body = [AttemptView]), (status = 404, body = ApiError)))]
async fn attempts(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path(project): Path<String>,
    Query(q): Query<Limit>,
) -> Response {
    let project = match project_id(&state, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    if let Err(r) = require(&principal, Action::ReadRuns, Resource::Project(project)) {
        return *r;
    }
    match state
        .store
        .attempts(project, q.limit.unwrap_or(50).clamp(1, 500))
        .await
    {
        Ok(a) => Json(a).into_response(),
        Err(e) => internal(&e),
    }
}

/// One attempt's full state at `attempt.version`. Apply live deltas with a higher version.
#[utoipa::path(get, path = "/api/v1/projects/{project}/attempts/{attempt}",
    params(("project" = String, Path, description = "Project slug"),
           ("attempt" = Uuid, Path, description = "Pipeline attempt ID")),
    responses((status = 200, body = Snapshot), (status = 404, body = ApiError)))]
async fn snapshot(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path((project, attempt)): Path<(String, Uuid)>,
) -> Response {
    let project = match project_id(&state, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    if let Err(r) = require(&principal, Action::ReadRuns, Resource::Project(project)) {
        return *r;
    }
    match state.store.snapshot(project, attempt).await {
        Ok(Some(mut s)) => {
            state.hub.overlay_snapshot(&mut s);
            Json(s).into_response()
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "no such attempt"),
        Err(e) => internal(&e),
    }
}

/// One attempt's Fleet plans: each shard's planned time, its flows and its actual span. Screen B's
/// plan table reads this instead of the whole snapshot.
#[utoipa::path(get, path = "/api/v1/projects/{project}/attempts/{attempt}/plans",
    params(("project" = String, Path, description = "Project slug"),
           ("attempt" = Uuid, Path, description = "Pipeline attempt ID")),
    responses((status = 200, body = [PlanView]), (status = 404, body = ApiError)))]
async fn attempt_plans(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path((project, attempt)): Path<(String, Uuid)>,
) -> Response {
    let project = match read_project(&state, &principal, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    match state.store.attempt_plans(project, attempt).await {
        Ok(Some(plans)) => Json(plans).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "no such attempt"),
        Err(e) => internal(&e),
    }
}

/// A Fleet plan upload (ADR-0013).
#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct PlanUpload {
    /// The manifest the planner wrote (`planId`, `shards[{id, flows, estimatedSeconds}]`).
    #[schema(value_type = Object)]
    manifest: PlanManifest,
    /// The CI run it plans, when no verified OIDC claims say so.
    #[serde(default)]
    pipeline: PlanPipeline,
}

/// The stored plan.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct PlanStored {
    /// The plan.
    plan_id: String,
    /// False when this exact manifest was already stored.
    created: bool,
    /// Flows planned.
    planned_total: usize,
    /// The pipeline attempt it is linked to.
    pipeline_attempt_id: Option<Uuid>,
}

/// Stores an immutable plan. Needs a publisher credential (API key or agent token).
#[utoipa::path(post, path = "/api/v1/plans", request_body = PlanUpload, responses(
    (status = 201, body = PlanStored), (status = 200, body = PlanStored, description = "Already stored"),
    (status = 409, body = ApiError), (status = 422, body = ApiError)))]
async fn store_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(upload): Json<PlanUpload>,
) -> Response {
    let publisher = match bearer(&headers) {
        Some(t) => match state.store.authenticate(&t).await {
            Ok(Some(p)) => p,
            Ok(None) => return error(StatusCode::UNAUTHORIZED, "unknown credential"),
            Err(e) => return internal(&e),
        },
        None => {
            return error(
                StatusCode::UNAUTHORIZED,
                "a publisher credential is required",
            );
        }
    };
    match state
        .store
        .store_plan(&publisher, &upload.manifest, &upload.pipeline)
        .await
    {
        Ok(stored) => {
            let status = if stored.created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            let body = PlanStored {
                plan_id: upload.manifest.plan_id.clone(),
                created: stored.created,
                planned_total: upload.manifest.planned_total(),
                pipeline_attempt_id: stored.pipeline_attempt_id,
            };
            state.publish(stored.change);
            (status, Json(body)).into_response()
        }
        Err(PlanError::Conflict(id)) => error(
            StatusCode::CONFLICT,
            format!("plan {id} already exists with different contents; plans are immutable"),
        ),
        Err(PlanError::Invalid(m)) => error(StatusCode::UNPROCESSABLE_ENTITY, m),
        Err(PlanError::Store(e)) => internal(&e),
    }
}
