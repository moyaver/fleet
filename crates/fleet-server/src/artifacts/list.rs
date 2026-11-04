// SPDX-License-Identifier: AGPL-3.0-only
//! `GET /api/v1/projects/{project}/flows/{execution_id}/artifacts`: a flow's artifacts, each
//! with a freshly signed link to the usercontent origin.
//!
//! Needs a role that may read artifacts (`Action::ReadArtifacts`). Minting a link is the one
//! place that decision is made: the usercontent origin only checks the signature.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use fleet_core::policy::{Action, Resource};
use fleet_protocol::rest::{ArtifactLinkView, ArtifactListView, ArtifactView};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    api::{ApiError, error, project_id, require},
    session::Caller,
    state::AppState,
};

/// A flow's artifacts with links that open them for a few minutes.
#[utoipa::path(get, path = "/api/v1/projects/{project}/flows/{execution_id}/artifacts",
    params(("project" = String, Path, description = "Project slug"),
           ("execution_id" = Uuid, Path, description = "Flow execution ID")),
    responses((status = 200, body = ArtifactListView), (status = 404, body = ApiError)))]
pub(crate) async fn flow_artifacts(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path((project, flow)): Path<(String, Uuid)>,
) -> Response {
    let project = match project_id(&state, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    if let Err(r) = require(
        &principal,
        Action::ReadArtifacts,
        Resource::Project(project),
    ) {
        return *r;
    }
    let rows = match state.store.flow_artifacts(project, flow).await {
        Ok(Some(rows)) => rows,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such flow"),
        Err(e) => {
            tracing::error!(error = %e, "listing artifacts");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    let now = OffsetDateTime::now_utc();
    let artifacts = rows
        .into_iter()
        .map(|a| {
            let link = state
                .signer
                .as_ref()
                .filter(|_| a.state == "available")
                .map(|signer| {
                    let (url, expires_at) = signer.link(project, a.id, &a.name, now);
                    ArtifactLinkView { url, expires_at }
                });
            ArtifactView {
                id: a.id,
                flow_execution_id: a.flow_execution_id,
                class: a.class,
                name: a.name,
                path: a.path,
                content_type: a.content_type,
                size_bytes: a.size_bytes,
                sha256: a.sha256,
                state: a.state,
                reject_reason: a.reject_reason,
                retention_class: a.retention_class,
                created_at: a.created_at,
                link,
            }
        })
        .collect();
    Json(ArtifactListView {
        artifacts,
        usercontent_url: state
            .config
            .usercontent
            .url
            .as_ref()
            .map(|u| u.trim_end_matches('/').to_owned()),
    })
    .into_response()
}
