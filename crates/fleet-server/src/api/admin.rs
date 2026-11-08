// SPDX-License-Identifier: AGPL-3.0-only
//! Screen G's REST reads (spec §8 "Screen G — Administration"; PLAN §10.3, M6): installation-wide
//! administration, read only. Every route goes through [`Caller`] and
//! `authorize(Global, ViewAdministration)` (the audit history: `ViewAudit`). Anyone else is refused
//! with `403`: an administration page has no project to hide, so "no role" is not a `404` here.
//!
//! Settings that come from the running configuration are read from [`AppState`]'s config and
//! say so (`source: "config"`). Nothing here writes; the edit routes come with the forms.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use fleet_core::policy::{Action, Decision, Principal, Resource, authorize};
use fleet_protocol::{
    PROTOCOL_VERSION,
    rest::admin::{
        AdminAgentRow, AdminAgentsView, AdminApiKeyView, AdminAuditPage, AdminIngestionView,
        AdminProjectView, AdminRetentionView, AdminRoleMappingsView, AdminSettingView,
        AdminTrustPolicyView,
    },
};
use fleet_store::admin::AuditCursor;
use serde::Deserialize;
use utoipa::IntoParams;

use super::{ApiError, error, internal};
use crate::{
    config::{Config, StorageBackend},
    session::Caller,
    state::AppState,
};

/// The most audit events one page carries, and the page a request gets when it names no limit.
const AUDIT_PAGE: i64 = 50;
const AUDIT_PAGE_MAX: i64 = 200;

/// Refuses anyone without a global administrator grant. Anonymous callers never get this far:
/// [`Caller`] answers them `401` first.
fn admin_gate(principal: &Principal, action: Action) -> Result<(), Box<Response>> {
    match authorize(principal, action, Resource::Global) {
        Decision::Allow => Ok(()),
        Decision::Deny(_) => Err(Box::new(error(
            StatusCode::FORBIDDEN,
            "your role does not allow this",
        ))),
    }
}

/// A count of days as text: `0` keeps for ever, the way every `retention.*_days` reads it.
fn days(d: u32) -> String {
    if d == 0 {
        "0 (kept for ever)".to_owned()
    } else {
        format!("{d} days")
    }
}

/// One configuration setting, read-only.
fn setting(key: &str, value: &impl ToString) -> AdminSettingView {
    AdminSettingView {
        key: key.to_owned(),
        value: value.to_string(),
        source: "config".to_owned(),
    }
}

/// Retention windows from the running configuration (`retention.*`).
#[must_use]
pub(crate) fn retention_windows(config: &Config) -> Vec<AdminSettingView> {
    let r = &config.retention;
    vec![
        setting("retention.raw_samples_days", &days(r.raw_samples_days)),
        setting("retention.rollup_1m_days", &days(r.rollup_1m_days)),
        setting("retention.rollup_1h_days", &days(r.rollup_1h_days)),
        setting("retention.journal_days", &days(r.journal_days)),
        setting("retention.steps_days", &days(r.steps_days)),
        setting("retention.audit_days", &days(r.audit_days)),
        setting("retention.blob_grace_hours", &r.blob_grace_hours),
        setting(
            "retention.expired_artifact_days",
            &days(r.expired_artifact_days),
        ),
        setting(
            "retention.stale_upload_hours",
            &r.stale_upload_hours.to_string(),
        ),
        setting(
            "retention.rollup_interval_seconds",
            &r.rollup_interval_seconds,
        ),
        setting(
            "retention.gc_interval_minutes",
            &r.gc_interval_minutes.to_string(),
        ),
    ]
}

/// The configured blob store, as the configuration names it.
fn backend_name(backend: StorageBackend) -> &'static str {
    match backend {
        StorageBackend::LocalFs => "local_fs",
        StorageBackend::S3 => "s3",
    }
}

/// The artifact policy from the running configuration: where bytes live, what one upload, run and
/// project may store, and how long a link works. Nothing here is a secret.
#[must_use]
pub(crate) fn artifact_policy(config: &Config) -> Vec<AdminSettingView> {
    let a = &config.artifacts;
    let c = &a.max_bytes;
    vec![
        setting("storage.backend", &backend_name(config.storage.backend)),
        setting(
            "artifacts.grant_ttl_seconds",
            &a.grant_ttl_seconds.to_string(),
        ),
        setting("artifacts.max_run_bytes", &a.max_run_bytes),
        setting("artifacts.max_run_files", &a.max_run_files),
        setting(
            "artifacts.max_project_bytes_per_day",
            &a.max_project_bytes_per_day,
        ),
        setting(
            "artifacts.max_project_files_per_day",
            &a.max_project_files_per_day,
        ),
        setting(
            "artifacts.max_bytes.failure_screenshot",
            &c.failure_screenshot,
        ),
        setting(
            "artifacts.max_bytes.failure_hierarchy",
            &c.failure_hierarchy.to_string(),
        ),
        setting("artifacts.max_bytes.commands", &c.commands),
        setting("artifacts.max_bytes.junit", &c.junit),
        setting("artifacts.max_bytes.maestro_log", &c.maestro_log),
        setting("artifacts.max_bytes.html_report", &c.html_report),
        setting(
            "artifacts.max_bytes.device_logcat",
            &c.device_logcat.to_string(),
        ),
        setting(
            "usercontent.link_ttl_seconds",
            &config.usercontent.link_ttl_seconds,
        ),
    ]
}

/// The metric collection policy: how often telemetry is checkpointed and kept, from the
/// configuration, and the sampling the agents do, which no server setting changes.
#[must_use]
pub(crate) fn metric_policy(config: &Config) -> Vec<AdminSettingView> {
    let r = &config.retention;
    vec![
        setting(
            "ingest.telemetry_flush_ms",
            &config.ingest.telemetry_flush_ms,
        ),
        setting(
            "ingest.heartbeat_interval_ms",
            &config.ingest.heartbeat_interval_ms,
        ),
        setting("retention.raw_samples_days", &days(r.raw_samples_days)),
        setting("retention.rollup_1m_days", &days(r.rollup_1m_days)),
        setting("retention.rollup_1h_days", &days(r.rollup_1h_days)),
        setting(
            "retention.rollup_interval_seconds",
            &r.rollup_interval_seconds,
        ),
        AdminSettingView {
            key: "agent.sample_interval".to_owned(),
            value: "5 s; 15 s while the host is under pressure".to_owned(),
            source: "agent".to_owned(),
        },
    ]
}

/// Every project, by slug, with its working API keys and trust policies.
#[utoipa::path(get, path = "/api/v1/admin/projects", responses(
    (status = 200, body = [AdminProjectView]), (status = 401, body = ApiError),
    (status = 403, body = ApiError)))]
pub(super) async fn projects(State(state): State<AppState>, Caller(principal): Caller) -> Response {
    if let Err(r) = admin_gate(&principal, Action::ViewAdministration) {
        return *r;
    }
    match state.store.admin_projects().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(&e),
    }
}

/// Every publisher trust policy, by project.
#[utoipa::path(get, path = "/api/v1/admin/trust-policies", responses(
    (status = 200, body = [AdminTrustPolicyView]), (status = 401, body = ApiError),
    (status = 403, body = ApiError)))]
pub(super) async fn trust_policies(
    State(state): State<AppState>,
    Caller(principal): Caller,
) -> Response {
    if let Err(r) = admin_gate(&principal, Action::ViewAdministration) {
        return *r;
    }
    match state.store.admin_trust_policies().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(&e),
    }
}

/// Every API key's metadata, newest first. No secret and no hash.
#[utoipa::path(get, path = "/api/v1/admin/api-keys", responses(
    (status = 200, body = [AdminApiKeyView]), (status = 401, body = ApiError),
    (status = 403, body = ApiError)))]
pub(super) async fn api_keys(State(state): State<AppState>, Caller(principal): Caller) -> Response {
    if let Err(r) = admin_gate(&principal, Action::ViewAdministration) {
        return *r;
    }
    match state.store.admin_api_keys().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(&e),
    }
}

/// Roles bound to people, and the identity-provider groups that grant them.
#[utoipa::path(get, path = "/api/v1/admin/role-mappings", responses(
    (status = 200, body = AdminRoleMappingsView), (status = 401, body = ApiError),
    (status = 403, body = ApiError)))]
pub(super) async fn role_mappings(
    State(state): State<AppState>,
    Caller(principal): Caller,
) -> Response {
    if let Err(r) = admin_gate(&principal, Action::ViewAdministration) {
        return *r;
    }
    match state.store.admin_role_mappings().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(&e),
    }
}

/// Each project's retention classes, and the server's retention windows.
#[utoipa::path(get, path = "/api/v1/admin/retention", responses(
    (status = 200, body = AdminRetentionView), (status = 401, body = ApiError),
    (status = 403, body = ApiError)))]
pub(super) async fn retention(
    State(state): State<AppState>,
    Caller(principal): Caller,
) -> Response {
    if let Err(r) = admin_gate(&principal, Action::ViewAdministration) {
        return *r;
    }
    match state.store.admin_retention_classes().await {
        Ok(classes) => Json(AdminRetentionView {
            classes,
            windows: retention_windows(&state.config),
        })
        .into_response(),
        Err(e) => internal(&e),
    }
}

/// The artifact policy, read from the running configuration.
#[utoipa::path(get, path = "/api/v1/admin/artifact-policy", responses(
    (status = 200, body = [AdminSettingView]), (status = 401, body = ApiError),
    (status = 403, body = ApiError)))]
pub(super) async fn artifact_policy_view(
    State(state): State<AppState>,
    Caller(principal): Caller,
) -> Response {
    if let Err(r) = admin_gate(&principal, Action::ViewAdministration) {
        return *r;
    }
    Json(artifact_policy(&state.config)).into_response()
}

/// The metric collection policy, read from the running configuration.
#[utoipa::path(get, path = "/api/v1/admin/metric-policy", responses(
    (status = 200, body = [AdminSettingView]), (status = 401, body = ApiError),
    (status = 403, body = ApiError)))]
pub(super) async fn metric_policy_view(
    State(state): State<AppState>,
    Caller(principal): Caller,
) -> Response {
    if let Err(r) = admin_gate(&principal, Action::ViewAdministration) {
        return *r;
    }
    Json(metric_policy(&state.config)).into_response()
}

/// The agent versions seen, and whether this server speaks their protocol.
#[utoipa::path(get, path = "/api/v1/admin/agents", responses(
    (status = 200, body = AdminAgentsView), (status = 401, body = ApiError),
    (status = 403, body = ApiError)))]
pub(super) async fn agents(State(state): State<AppState>, Caller(principal): Caller) -> Response {
    if let Err(r) = admin_gate(&principal, Action::ViewAdministration) {
        return *r;
    }
    match state.store.admin_agents().await {
        Ok(rows) => Json(AdminAgentsView {
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: i32::from(PROTOCOL_VERSION),
            agents: rows
                .into_iter()
                .map(|r| AdminAgentRow {
                    compatible: r.protocol_version == i32::from(PROTOCOL_VERSION),
                    ..r
                })
                .collect(),
        })
        .into_response(),
        Err(e) => internal(&e),
    }
}

/// Event ingestion health: the newest refusals and losses, and the open runs with their lag.
#[utoipa::path(get, path = "/api/v1/admin/ingestion", responses(
    (status = 200, body = AdminIngestionView), (status = 401, body = ApiError),
    (status = 403, body = ApiError)))]
pub(super) async fn ingestion(
    State(state): State<AppState>,
    Caller(principal): Caller,
) -> Response {
    if let Err(r) = admin_gate(&principal, Action::ViewAdministration) {
        return *r;
    }
    match state.store.admin_ingestion().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(&e),
    }
}

/// Query of the audit history.
#[derive(Debug, Deserialize, IntoParams)]
pub(super) struct AuditParams {
    /// `next_cursor` of the previous page.
    cursor: Option<String>,
    /// Events per page (default 50, max 200).
    limit: Option<i64>,
}

/// The audit history, newest first, a page at a time.
#[utoipa::path(get, path = "/api/v1/admin/audit", params(AuditParams), responses(
    (status = 200, body = AdminAuditPage), (status = 400, body = ApiError),
    (status = 401, body = ApiError), (status = 403, body = ApiError)))]
pub(super) async fn audit(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Query(q): Query<AuditParams>,
) -> Response {
    if let Err(r) = admin_gate(&principal, Action::ViewAudit) {
        return *r;
    }
    let after = match q.cursor.as_deref() {
        None => None,
        Some(token) => match AuditCursor::decode(token) {
            Some(c) => Some(c),
            None => return error(StatusCode::BAD_REQUEST, "unknown cursor"),
        },
    };
    let limit = q.limit.unwrap_or(AUDIT_PAGE).clamp(1, AUDIT_PAGE_MAX);
    match state.store.admin_audit(after, limit).await {
        Ok((items, next)) => Json(AdminAuditPage {
            items,
            next_cursor: next.map(|c| c.encode()),
        })
        .into_response(),
        Err(e) => internal(&e),
    }
}
