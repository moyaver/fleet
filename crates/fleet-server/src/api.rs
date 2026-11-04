// SPDX-License-Identifier: AGPL-3.0-only
//! REST API (PLAN §7.1 `api/`) and its OpenAPI document (`/api/v1/openapi.json`).
//!
//! Reads need a viewer: until browser SSO (M4), a configured viewer token, or anonymous reads
//! in development. Agent credentials are write-only and are refused here. `POST /api/v1/plans`
//! takes a publisher credential: the planning CI job stores its plan like an agent publishes.

pub(crate) mod live_ws;

use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, Query, State},
    http::{HeaderMap, StatusCode, request::Parts},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use fleet_protocol::{
    live::{
        AgentRunView, AttemptView, DeviceView, EntityView, FlowView, JobView, Liveness,
        PlanShardView, PlanView, ProjectView, Snapshot, SuiteView,
    },
    plan::PlanManifest,
};
use fleet_store::plans::{PlanError, PlanPipeline};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::{OpenApi, ToSchema};
use uuid::Uuid;

use crate::{ingest::bearer, state::AppState};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Fleet",
        description = "Live observability for mobile UI tests in CI. Agents use the WebSocket \
                       protocol `fleet.v1+json` (`/api/v1/agent/ws`); this document covers REST.",
        license(name = "AGPL-3.0-only", identifier = "AGPL-3.0-only")
    ),
    paths(projects, attempts, snapshot, store_plan),
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

/// Someone allowed to read run data.
#[derive(Debug)]
pub(crate) struct Viewer;

fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

/// Whether these headers carry a viewer credential (shared with the live socket).
pub(crate) fn is_viewer(state: &AppState, headers: &HeaderMap) -> bool {
    if state.config.api.anonymous_read {
        return true;
    }
    let Some(token) = bearer(headers) else {
        return false;
    };
    // Compare digests: equal length, and no early exit on the secret's bytes.
    let given = digest(&token);
    state
        .config
        .api
        .viewer_tokens
        .iter()
        .fold(false, |ok, t| ok | (digest(t.expose()) == given))
}

impl FromRequestParts<AppState> for Viewer {
    type Rejection = Response;

    fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> impl Future<Output = Result<Self, Response>> + Send {
        std::future::ready(if is_viewer(state, &parts.headers) {
            Ok(Self)
        } else {
            Err(error(
                StatusCode::UNAUTHORIZED,
                "a viewer token is required",
            ))
        })
    }
}

async fn project_id(state: &AppState, slug: &str) -> Result<Uuid, Box<Response>> {
    match state.store.project_id(slug).await {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err(Box::new(error(StatusCode::NOT_FOUND, "no such project"))),
        Err(e) => Err(Box::new(internal(&e))),
    }
}

/// Every project.
#[utoipa::path(get, path = "/api/v1/projects", responses(
    (status = 200, body = [ProjectView]), (status = 401, body = ApiError)))]
async fn projects(State(state): State<AppState>, _: Viewer) -> Response {
    match state.store.projects().await {
        Ok(p) => Json(p).into_response(),
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
    _: Viewer,
    Path(project): Path<String>,
    Query(q): Query<Limit>,
) -> Response {
    let project = match project_id(&state, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
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
    _: Viewer,
    Path((project, attempt)): Path<(String, Uuid)>,
) -> Response {
    let project = match project_id(&state, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    match state.store.snapshot(project, attempt).await {
        Ok(Some(mut s)) => {
            state.hub.overlay_snapshot(&mut s);
            Json(s).into_response()
        }
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
