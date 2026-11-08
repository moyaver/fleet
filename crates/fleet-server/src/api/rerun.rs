// SPDX-License-Identifier: AGPL-3.0-only
//! Rerunning failed flows (issue #82): the plan manifest that reruns an attempt's failed flows with
//! `fleet-agent run --plan` (ADR-0013). It is a download, read by anyone who may read the project.

use axum::{
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use fleet_core::reproduce::{ManifestError, rerun_manifest};
use uuid::Uuid;

use super::{error, internal, read_project};
use crate::{session::Caller, state::AppState};

/// `GET /api/v1/projects/{project}/attempts/{attempt}/failed-flows`: the manifest of the failed
/// flows of the attempt, one shard per run. `404` when the attempt is missing or has no failed
/// flow (there is nothing to rerun), and `409` when a failed flow has no path: a manifest without
/// it would silently rerun fewer flows than failed.
pub(super) async fn failed_flows(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path((project, attempt)): Path<(String, Uuid)>,
) -> Response {
    let project = match read_project(&state, &principal, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let runs = match state.store.failed_flows(project, attempt).await {
        Ok(Some(runs)) => runs,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such attempt"),
        Err(e) => return internal(&e),
    };
    if runs.iter().all(Vec::is_empty) {
        return error(
            StatusCode::NOT_FOUND,
            "this attempt has no failed flows to rerun",
        );
    }
    match rerun_manifest(&attempt.to_string(), &runs) {
        Ok(manifest) => match serde_json::to_vec_pretty(&manifest) {
            Ok(body) => (
                [
                    (header::CONTENT_TYPE, "application/json".to_owned()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"fleet-rerun-{attempt}.json\""),
                    ),
                    (header::CACHE_CONTROL, "no-store".to_owned()),
                ],
                body,
            )
                .into_response(),
            Err(e) => internal(&e),
        },
        Err(ManifestError::NoPath(names)) => error(
            StatusCode::CONFLICT,
            format!(
                "{} failed flow(s) have no repository path, so a manifest could not rerun them all: {}",
                names.len(),
                names.join(", ")
            ),
        ),
    }
}
