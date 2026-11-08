// SPDX-License-Identifier: AGPL-3.0-only
//! Failure triage (issue #84): a maintainer marks a failure fingerprint known (with the issue that
//! tracks it) or resolved, or clears that. The routes take a signed-in session, CSRF-checked by
//! [`SignedIn`], and `authorize(TriageFailures)`; every change is audited by the store.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use fleet_core::policy::{Action, Principal, Resource};
use fleet_protocol::rest::history::{FailureTriageRequest, FailureTriageView, TriageState};
use fleet_store::auth::AuditActor;
use uuid::Uuid;

use super::{ApiError, error, internal, project_id, require};
use crate::{session::SignedIn, state::AppState};

/// The longest issue link a maintainer may record.
const MAX_URL: usize = 2000;

/// Records a fingerprint as known (with `issue_url`, required) or resolved (with an optional
/// link), or changes that record.
#[utoipa::path(put, path = "/api/v1/projects/{project}/failures/{fingerprint}/triage",
    params(("project" = String, Path, description = "Project slug"),
           ("fingerprint" = String, Path, description = "The failure fingerprint, `v1:…`")),
    request_body = FailureTriageRequest,
    responses((status = 200, body = FailureTriageView), (status = 400, body = ApiError),
               (status = 403, body = ApiError), (status = 404, body = ApiError)))]
pub(super) async fn set(
    State(state): State<AppState>,
    SignedIn(active): SignedIn,
    Path((project, fingerprint)): Path<(String, String)>,
    Json(req): Json<FailureTriageRequest>,
) -> Response {
    let project = match authorized(&state, &active, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let url = req
        .issue_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());
    if let Some(url) = url
        && !is_link(url)
    {
        return error(
            StatusCode::BAD_REQUEST,
            "`issue_url` must be an http or https link",
        );
    }
    if req.state == TriageState::Known && url.is_none() {
        return error(
            StatusCode::BAD_REQUEST,
            "a known failure needs its `issue_url`",
        );
    }
    let actor = AuditActor::User(active.user_id);
    match state
        .store
        .set_failure_triage(project, &fingerprint, req.state, url, actor)
        .await
    {
        Ok(Some(view)) => Json(view).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "no such failure"),
        Err(e) => internal(&e),
    }
}

/// Clears a fingerprint back to untriaged.
#[utoipa::path(delete, path = "/api/v1/projects/{project}/failures/{fingerprint}/triage",
    params(("project" = String, Path, description = "Project slug"),
           ("fingerprint" = String, Path, description = "The failure fingerprint, `v1:…`")),
    responses((status = 200, body = FailureTriageView), (status = 403, body = ApiError),
               (status = 404, body = ApiError)))]
pub(super) async fn clear(
    State(state): State<AppState>,
    SignedIn(active): SignedIn,
    Path((project, fingerprint)): Path<(String, String)>,
) -> Response {
    let project = match authorized(&state, &active, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let actor = AuditActor::User(active.user_id);
    match state
        .store
        .clear_failure_triage(project, &fingerprint, actor)
        .await
    {
        Ok(Some(view)) => Json(view).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "no such failure"),
        Err(e) => internal(&e),
    }
}

/// The project a signed-in person may triage in, or the refusal: 404 without any role on it,
/// 403 with a role too low to triage.
async fn authorized(
    state: &AppState,
    active: &crate::session::Active,
    slug: &str,
) -> Result<Uuid, Box<Response>> {
    let project = project_id(state, slug).await?;
    let principal = Principal::User {
        grants: active.data.grants.clone(),
    };
    require(
        &principal,
        Action::TriageFailures,
        Resource::Project(project),
    )?;
    Ok(project)
}

/// Whether `url` is an absolute `http` or `https` link without whitespace, of a sane length.
fn is_link(url: &str) -> bool {
    url.len() <= MAX_URL
        && (url.starts_with("https://") || url.starts_with("http://"))
        && !url.chars().any(char::is_whitespace)
}

/// The triage of each fingerprint in a project, keyed by fingerprint, for the views that show
/// one. A fingerprint the project has never seen is absent.
pub(super) async fn triage_of(
    state: &AppState,
    project: Uuid,
    fingerprints: &[String],
) -> Result<HashMap<String, FailureTriageView>, Box<Response>> {
    match state.store.failure_triage(project, fingerprints).await {
        Ok(rows) => Ok(rows
            .into_iter()
            .map(|t| (t.fingerprint.clone(), t))
            .collect()),
        Err(e) => Err(Box::new(internal(&e))),
    }
}
