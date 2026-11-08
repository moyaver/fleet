// SPDX-License-Identifier: AGPL-3.0-only
//! Screen G's writes (issue #139): the changes a global administrator makes, on the same routes
//! for a browser form and an API client.
//!
//! Who may write: a signed-in session whose CSRF token is in `X-CSRF-Token` or, for a form, in its
//! `csrf` field, from the dashboard's own origin ([`crate::session::write_session`]), and a global
//! administrator ([`Action::ManageAdministration`]). Both are checked before the body is read, so
//! anyone else is refused with nothing changed.
//!
//! The shape of an answer follows the request. A form (`application/x-www-form-urlencoded`) is
//! answered with a redirect back to its section of `/admin`, or, for a refusal, a page that says
//! why. Anything else is JSON. A new API key's secret is the one thing a form answers with a page
//! of its own: it is shown once, in the response, and sent with `Cache-Control: no-store`.
//!
//! Every change is made and audited by the store in one transaction; a refused change is not made.

use std::fmt::Write as _;

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use fleet_core::policy::{Action, Decision, Principal, Resource, Role, authorize};
use fleet_protocol::rest::admin::{AdminChangedView, AdminCreatedKeyView};
use fleet_store::{
    admin::write::{Refusal, WriteError},
    auth::{AuditActor, PolicyRow, TrustLevel},
};
use serde::{Deserialize, de::DeserializeOwned};
use time::{Duration, OffsetDateTime};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{api::error, config::TrustPolicyConfig, session, state::AppState};

/// The administration page, where a form goes back to.
const ADMIN_PATH: &str = "/admin";

/// The longest a key may be given, in days (ten years).
const MAX_DAYS: u32 = 3650;

/// Where a form goes back to, per section of `/admin`.
const PROJECTS: &str = "projects";
const KEYS: &str = "keys";
const ROLES: &str = "roles";
const GROUPS: &str = "roles";
const TRUST: &str = "trust";

/// How an answer is shaped: for a browser form, or for an API client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Via {
    Form,
    Json,
}

impl Via {
    fn of(headers: &HeaderMap) -> Self {
        let form = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
        if form { Self::Form } else { Self::Json }
    }
}

/// A refusal, answered for the request's shape.
fn fail(via: Via, status: StatusCode, message: &str) -> Response {
    match via {
        Via::Json => error(status, message),
        Via::Form => page(status, "Not saved", &format!("<p>{}</p>", escape(message))),
    }
}

/// [`fail`], boxed for the `Err` of a helper: a response is large, and a refusal is rare.
#[expect(
    clippy::unnecessary_box_returns,
    reason = "the boxed response is the Err of a Result, which stays small (result_large_err)"
)]
fn refuse(via: Via, status: StatusCode, message: &str) -> Box<Response> {
    Box::new(fail(via, status, message))
}

/// A plain HTML page, with no script and no store of its own.
fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{}</title></head><body><h1>{}</h1>{body}\
         <p><a href=\"{ADMIN_PATH}\">Back to administration</a></p></body></html>",
        escape(title),
        escape(title)
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'",
            ),
        ],
        html,
    )
        .into_response()
}

/// Text for HTML: the five characters that can start markup.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The answer to a change that was made: the new row's ID as JSON, or back to its section.
fn made(via: Via, status: StatusCode, id: Uuid, section: &str) -> Response {
    match via {
        Via::Json => (status, Json(AdminChangedView { id })).into_response(),
        Via::Form => see_other(section),
    }
}

/// The answer to a removal: no content, or back to its section.
fn removed(via: Via, section: &str) -> Response {
    match via {
        Via::Json => StatusCode::NO_CONTENT.into_response(),
        Via::Form => see_other(section),
    }
}

fn see_other(section: &str) -> Response {
    let to = format!("{ADMIN_PATH}#{section}");
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, to.as_str()),
            (header::CACHE_CONTROL, "no-store"),
        ],
    )
        .into_response()
}

/// A store refusal or failure, answered for the request's shape. A refusal names what to change.
fn write_failed(via: Via, e: WriteError) -> Response {
    match e {
        WriteError::Refused(r) => {
            let status = match r {
                Refusal::NotFound(_) => StatusCode::NOT_FOUND,
                Refusal::FromConfig(_) | Refusal::Conflict(_) | Refusal::LastGlobalAdmin => {
                    StatusCode::CONFLICT
                }
                Refusal::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            };
            fail(via, status, &r.to_string())
        }
        WriteError::Store(e) => {
            tracing::error!(error = %e, "an administration change failed");
            fail(via, StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// Who may change Screen G: a global administrator with a valid session and CSRF token. The
/// refusal is already answered for the request's shape.
async fn administrator(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
    via: Via,
) -> Result<AuditActor, Box<Response>> {
    let token = match via {
        Via::Form => serde_urlencoded::from_bytes::<CsrfField>(body)
            .ok()
            .and_then(|f| f.csrf),
        Via::Json => None,
    };
    let active = session::write_session(state, headers, token.as_deref())
        .await
        .map_err(|(status, message)| refuse(via, status, message))?;
    let principal = Principal::User {
        grants: active.data.grants.clone(),
    };
    match authorize(&principal, Action::ManageAdministration, Resource::Global) {
        Decision::Allow => Ok(AuditActor::User(active.user_id)),
        Decision::Deny(_) => Err(refuse(
            via,
            StatusCode::FORBIDDEN,
            "your role does not allow this",
        )),
    }
}

/// The CSRF field of a form. A form's other fields are read by [`decode`]; an API client sends its
/// token in `X-CSRF-Token`, never in the body.
#[derive(Deserialize)]
struct CsrfField {
    csrf: Option<String>,
}

/// A request's body, decoded for its shape: a form's fields, or JSON.
fn decode<T: DeserializeOwned>(via: Via, body: &[u8]) -> Result<T, Box<Response>> {
    let parsed = match via {
        Via::Form => serde_urlencoded::from_bytes::<T>(body).map_err(|e| e.to_string()),
        Via::Json => serde_json::from_slice::<T>(body).map_err(|e| e.to_string()),
    };
    parsed.map_err(|e| {
        refuse(
            via,
            StatusCode::BAD_REQUEST,
            &format!("the request body is not valid: {e}"),
        )
    })
}

/// A row ID in a path. Anything else is a row that does not exist.
fn id_of(via: Via, raw: &str, what: &str) -> Result<Uuid, Box<Response>> {
    Uuid::parse_str(raw).map_err(|_| refuse(via, StatusCode::NOT_FOUND, &format!("no such {what}")))
}

/// A project by slug, or the answer that it does not exist.
async fn project_of(state: &AppState, via: Via, slug: &str) -> Result<Uuid, Box<Response>> {
    match state.store.project_id(slug).await {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err(refuse(via, StatusCode::NOT_FOUND, "no such project")),
        Err(e) => Err(Box::new(write_failed(via, WriteError::Store(e)))),
    }
}

/// A scope from a form: a project's slug, or blank for every project.
async fn scope_of(state: &AppState, via: Via, slug: &str) -> Result<Option<Uuid>, Box<Response>> {
    let slug = slug.trim();
    if slug.is_empty() {
        return Ok(None);
    }
    project_of(state, via, slug).await.map(Some)
}

/// A role as the forms and the API spell it.
fn role_of(via: Via, name: &str) -> Result<Role, Box<Response>> {
    match name.trim() {
        "viewer" => Ok(Role::Viewer),
        "maintainer" => Ok(Role::Maintainer),
        "admin" => Ok(Role::Admin),
        _ => Err(refuse(
            via,
            StatusCode::UNPROCESSABLE_ENTITY,
            "the role must be viewer, maintainer or admin",
        )),
    }
}

/// Text that may be left blank: `None` when it is.
fn optional(value: &str) -> Option<String> {
    let v = value.trim();
    (!v.is_empty()).then(|| v.to_owned())
}

/// A list typed as text, one item per line or comma: blank items dropped.
fn list(value: &str) -> Vec<String> {
    value
        .split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

// ---------------------------------------------------------------- projects

/// A project to create.
#[derive(Debug, Deserialize, ToSchema)]
pub(in crate::api) struct ProjectInput {
    /// URL-safe identifier: 2 to 63 lowercase letters, digits and hyphens.
    slug: String,
    /// Display name.
    name: String,
}

/// A project's new display name.
#[derive(Debug, Deserialize, ToSchema)]
pub(in crate::api) struct RenameInput {
    /// The new display name.
    name: String,
}

/// Creates a project. Its slug must be new: a create never renames.
#[utoipa::path(post, path = "/api/v1/admin/projects", request_body = ProjectInput, responses(
    (status = 201, body = AdminChangedView), (status = 401, body = super::super::ApiError),
    (status = 403, body = super::super::ApiError), (status = 409, body = super::super::ApiError),
    (status = 422, body = super::super::ApiError)))]
pub(in crate::api) async fn create_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let via = Via::of(&headers);
    let by = match administrator(&state, &headers, &body, via).await {
        Ok(by) => by,
        Err(r) => return *r,
    };
    let input: ProjectInput = match decode(via, &body) {
        Ok(i) => i,
        Err(r) => return *r,
    };
    match state
        .store
        .create_project(by, input.slug.trim(), &input.name)
        .await
    {
        Ok(id) => made(via, StatusCode::CREATED, id, PROJECTS),
        Err(e) => write_failed(via, e),
    }
}

/// Renames a project. Its slug stays.
#[utoipa::path(put, path = "/api/v1/admin/projects/{project}", request_body = RenameInput,
    params(("project" = String, Path, description = "Project slug")), responses(
    (status = 200, body = AdminChangedView), (status = 401, body = super::super::ApiError),
    (status = 403, body = super::super::ApiError), (status = 404, body = super::super::ApiError),
    (status = 409, body = super::super::ApiError), (status = 422, body = super::super::ApiError)))]
pub(in crate::api) async fn rename_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    body: Bytes,
) -> Response {
    let via = Via::of(&headers);
    let by = match administrator(&state, &headers, &body, via).await {
        Ok(by) => by,
        Err(r) => return *r,
    };
    let project = match project_of(&state, via, &slug).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let input: RenameInput = match decode(via, &body) {
        Ok(i) => i,
        Err(r) => return *r,
    };
    match state.store.rename_project(by, project, &input.name).await {
        Ok(()) => made(via, StatusCode::OK, project, PROJECTS),
        Err(e) => write_failed(via, e),
    }
}

// ---------------------------------------------------------------- API keys

/// A key to create.
#[derive(Debug, Deserialize, ToSchema)]
pub(in crate::api) struct KeyInput {
    /// The project's slug.
    project: String,
    /// What the key is for (its name in the list).
    name: String,
    /// `trusted` (the default), `pr` or `untrusted`: the trust its runs get.
    #[serde(default)]
    trust: Option<String>,
    /// Days until it stops working (1 to 3650). Defaults to `api.api_key_ttl_days`.
    #[serde(default)]
    days: Option<u32>,
}

/// Creates an API key. The secret is in the answer, once: a form gets it on a page of its own, and
/// an API client gets it in the JSON.
#[utoipa::path(post, path = "/api/v1/admin/api-keys", request_body = KeyInput, responses(
    (status = 201, body = AdminCreatedKeyView), (status = 401, body = super::super::ApiError),
    (status = 403, body = super::super::ApiError), (status = 404, body = super::super::ApiError),
    (status = 422, body = super::super::ApiError)))]
pub(in crate::api) async fn create_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let via = Via::of(&headers);
    let by = match administrator(&state, &headers, &body, via).await {
        Ok(by) => by,
        Err(r) => return *r,
    };
    let input: KeyInput = match decode(via, &body) {
        Ok(i) => i,
        Err(r) => return *r,
    };
    let project = match project_of(&state, via, input.project.trim()).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let trust = match input.trust.as_deref().map(str::trim) {
        None | Some("" | "trusted") => TrustLevel::Trusted,
        Some("pr") => TrustLevel::Pr,
        Some("untrusted") => TrustLevel::Untrusted,
        Some(_) => {
            return fail(
                via,
                StatusCode::UNPROCESSABLE_ENTITY,
                "the trust must be trusted, pr or untrusted",
            );
        }
    };
    let days = input.days.unwrap_or(state.config.api.api_key_ttl_days);
    if !(1..=MAX_DAYS).contains(&days) {
        return fail(
            via,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a key lasts from 1 to 3650 days",
        );
    }
    let expires_at = OffsetDateTime::now_utc() + Duration::days(i64::from(days));
    let (id, secret) = match state
        .store
        .create_api_key_as(by, project, &input.name, trust, expires_at)
        .await
    {
        Ok(created) => created,
        Err(e) => return write_failed(via, e),
    };
    let key = match state.store.admin_api_key(id).await {
        Ok(Some(view)) => view,
        Ok(None) => return fail(via, StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
        Err(e) => return write_failed(via, WriteError::Store(e)),
    };
    match via {
        Via::Json => (
            StatusCode::CREATED,
            Json(AdminCreatedKeyView {
                key,
                secret: secret.expose().to_owned(),
            }),
        )
            .into_response(),
        Via::Form => secret_page(&key.name, &key.project, expires_at, secret.expose()),
    }
}

/// The page that shows a new key's secret, once. Nothing on it is a script, and it is never cached.
fn secret_page(name: &str, project: &str, expires_at: OffsetDateTime, secret: &str) -> Response {
    let expires = expires_at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let mut body = String::new();
    let _ = write!(
        body,
        "<p>The key <strong>{}</strong> for project <strong>{}</strong> is ready. Copy the secret now: \
         Fleet keeps only its hash, so it is never shown again.</p>\
         <pre id=\"secret\">{}</pre><p>It stops working at {}.</p>",
        escape(name),
        escape(project),
        escape(secret),
        escape(&expires)
    );
    page(StatusCode::CREATED, "API key created", &body)
}

/// Revokes an API key. The agent tokens minted from it stop with it.
#[utoipa::path(delete, path = "/api/v1/admin/api-keys/{id}",
    params(("id" = String, Path, description = "The key's ID")), responses(
    (status = 200, body = AdminChangedView), (status = 401, body = super::super::ApiError),
    (status = 403, body = super::super::ApiError), (status = 404, body = super::super::ApiError),
    (status = 409, body = super::super::ApiError)))]
pub(in crate::api) async fn revoke_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    body: Bytes,
) -> Response {
    let via = Via::of(&headers);
    let by = match administrator(&state, &headers, &body, via).await {
        Ok(by) => by,
        Err(r) => return *r,
    };
    let id = match id_of(via, &raw, "API key") {
        Ok(id) => id,
        Err(r) => return *r,
    };
    match state.store.revoke_api_key_as(by, id).await {
        Ok(()) => made(via, StatusCode::OK, id, KEYS),
        Err(e) => write_failed(via, e),
    }
}

/// An API key's new expiry.
#[derive(Debug, Deserialize, ToSchema)]
pub(in crate::api) struct ExpireInput {
    /// Days from now until it stops working (0 ends it now, up to 3650).
    days: u32,
}

/// Sets when an API key stops working, in days from now.
#[utoipa::path(post, path = "/api/v1/admin/api-keys/{id}/expire", request_body = ExpireInput,
    params(("id" = String, Path, description = "The key's ID")), responses(
    (status = 200, body = AdminChangedView), (status = 401, body = super::super::ApiError),
    (status = 403, body = super::super::ApiError), (status = 404, body = super::super::ApiError),
    (status = 409, body = super::super::ApiError), (status = 422, body = super::super::ApiError)))]
pub(in crate::api) async fn expire_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    body: Bytes,
) -> Response {
    let via = Via::of(&headers);
    let by = match administrator(&state, &headers, &body, via).await {
        Ok(by) => by,
        Err(r) => return *r,
    };
    let id = match id_of(via, &raw, "API key") {
        Ok(id) => id,
        Err(r) => return *r,
    };
    let input: ExpireInput = match decode(via, &body) {
        Ok(i) => i,
        Err(r) => return *r,
    };
    if input.days > MAX_DAYS {
        return fail(
            via,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a key lasts at most 3650 days",
        );
    }
    let at = OffsetDateTime::now_utc() + Duration::days(i64::from(input.days));
    match state.store.expire_api_key_as(by, id, at).await {
        Ok(()) => made(via, StatusCode::OK, id, KEYS),
        Err(e) => write_failed(via, e),
    }
}

// ---------------------------------------------------------------- role bindings

/// A role to bind to a person.
#[derive(Debug, Deserialize, ToSchema)]
pub(in crate::api) struct BindingInput {
    /// The project's slug, or blank for every project.
    #[serde(default)]
    project: String,
    /// The person, by ID (the people list of the page).
    #[schema(value_type = String)]
    person: Uuid,
    /// `viewer`, `maintainer` or `admin`.
    role: String,
}

/// Binds a role to a person on a project, or on every project. The person's open sessions end, so
/// the role applies at their next request.
#[utoipa::path(post, path = "/api/v1/admin/role-bindings", request_body = BindingInput, responses(
    (status = 201, body = AdminChangedView), (status = 401, body = super::super::ApiError),
    (status = 403, body = super::super::ApiError), (status = 404, body = super::super::ApiError),
    (status = 409, body = super::super::ApiError), (status = 422, body = super::super::ApiError)))]
pub(in crate::api) async fn add_binding(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let via = Via::of(&headers);
    let by = match administrator(&state, &headers, &body, via).await {
        Ok(by) => by,
        Err(r) => return *r,
    };
    let input: BindingInput = match decode(via, &body) {
        Ok(i) => i,
        Err(r) => return *r,
    };
    let role = match role_of(via, &input.role) {
        Ok(r) => r,
        Err(r) => return *r,
    };
    let project = match scope_of(&state, via, &input.project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    match state
        .store
        .add_role_binding(by, project, input.person, role)
        .await
    {
        Ok(id) => made(via, StatusCode::CREATED, id, ROLES),
        Err(e) => write_failed(via, e),
    }
}

/// Removes a role bound to a person. The last global administrator cannot be removed.
#[utoipa::path(delete, path = "/api/v1/admin/role-bindings/{id}",
    params(("id" = String, Path, description = "The binding's ID")), responses(
    (status = 204), (status = 401, body = super::super::ApiError),
    (status = 403, body = super::super::ApiError), (status = 404, body = super::super::ApiError),
    (status = 409, body = super::super::ApiError)))]
pub(in crate::api) async fn remove_binding(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    body: Bytes,
) -> Response {
    let via = Via::of(&headers);
    let by = match administrator(&state, &headers, &body, via).await {
        Ok(by) => by,
        Err(r) => return *r,
    };
    let id = match id_of(via, &raw, "role") {
        Ok(id) => id,
        Err(r) => return *r,
    };
    match state.store.remove_role_binding(by, id).await {
        Ok(()) => removed(via, ROLES),
        Err(e) => write_failed(via, e),
    }
}

// ---------------------------------------------------------------- group mappings

/// An identity-provider group to map to a role.
#[derive(Debug, Deserialize, ToSchema)]
pub(in crate::api) struct GroupInput {
    /// The project's slug, or blank for every project.
    #[serde(default)]
    project: String,
    /// The group claim's value, exactly as the identity provider sends it.
    #[serde(default)]
    group: String,
    /// `viewer`, `maintainer` or `admin`.
    role: String,
}

/// A group mapping's new role (a `PUT` on the mapping, or the form's `edit`).
#[derive(Debug, Deserialize, ToSchema)]
pub(in crate::api) struct GroupRoleInput {
    /// `viewer`, `maintainer` or `admin`.
    role: String,
}

/// Maps a group to a role. The sign-in issuer is the configured one.
#[utoipa::path(post, path = "/api/v1/admin/group-mappings", request_body = GroupInput, responses(
    (status = 201, body = AdminChangedView), (status = 401, body = super::super::ApiError),
    (status = 403, body = super::super::ApiError), (status = 404, body = super::super::ApiError),
    (status = 409, body = super::super::ApiError), (status = 422, body = super::super::ApiError)))]
pub(in crate::api) async fn add_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let via = Via::of(&headers);
    let by = match administrator(&state, &headers, &body, via).await {
        Ok(by) => by,
        Err(r) => return *r,
    };
    let input: GroupInput = match decode(via, &body) {
        Ok(i) => i,
        Err(r) => return *r,
    };
    let Some(login) = state.config.login.as_ref() else {
        return fail(
            via,
            StatusCode::UNPROCESSABLE_ENTITY,
            "sign-in is not configured, so there is no identity provider to map groups for",
        );
    };
    let role = match role_of(via, &input.role) {
        Ok(r) => r,
        Err(r) => return *r,
    };
    let project = match scope_of(&state, via, &input.project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    match state
        .store
        .add_group_mapping(by, &login.issuer, project, &input.group, role)
        .await
    {
        Ok(id) => made(via, StatusCode::CREATED, id, GROUPS),
        Err(e) => write_failed(via, e),
    }
}

/// Changes the role a dashboard group mapping grants. A configured mapping is locked.
#[utoipa::path(put, path = "/api/v1/admin/group-mappings/{id}", request_body = GroupRoleInput,
    params(("id" = String, Path, description = "The mapping's ID")), responses(
    (status = 200, body = AdminChangedView), (status = 401, body = super::super::ApiError),
    (status = 403, body = super::super::ApiError), (status = 404, body = super::super::ApiError),
    (status = 409, body = super::super::ApiError), (status = 422, body = super::super::ApiError)))]
pub(in crate::api) async fn edit_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    body: Bytes,
) -> Response {
    let via = Via::of(&headers);
    let by = match administrator(&state, &headers, &body, via).await {
        Ok(by) => by,
        Err(r) => return *r,
    };
    let id = match id_of(via, &raw, "group mapping") {
        Ok(id) => id,
        Err(r) => return *r,
    };
    let input: GroupRoleInput = match decode(via, &body) {
        Ok(i) => i,
        Err(r) => return *r,
    };
    let role = match role_of(via, &input.role) {
        Ok(r) => r,
        Err(r) => return *r,
    };
    match state.store.set_group_mapping_role(by, id, role).await {
        Ok(()) => made(via, StatusCode::OK, id, GROUPS),
        Err(e) => write_failed(via, e),
    }
}

/// Removes a group mapping. A configured mapping is locked.
#[utoipa::path(delete, path = "/api/v1/admin/group-mappings/{id}",
    params(("id" = String, Path, description = "The mapping's ID")), responses(
    (status = 204), (status = 401, body = super::super::ApiError),
    (status = 403, body = super::super::ApiError), (status = 404, body = super::super::ApiError),
    (status = 409, body = super::super::ApiError)))]
pub(in crate::api) async fn remove_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    body: Bytes,
) -> Response {
    let via = Via::of(&headers);
    let by = match administrator(&state, &headers, &body, via).await {
        Ok(by) => by,
        Err(r) => return *r,
    };
    let id = match id_of(via, &raw, "group mapping") {
        Ok(id) => id,
        Err(r) => return *r,
    };
    match state.store.remove_group_mapping(by, id).await {
        Ok(()) => removed(via, GROUPS),
        Err(e) => write_failed(via, e),
    }
}

// ---------------------------------------------------------------- trust policies

/// A publisher trust policy's fields. Lists are text, one item per line or comma. A blank optional
/// field means none; a blank `issuer`, `audience` or `pr_policy` takes the default.
#[derive(Debug, Deserialize, ToSchema)]
pub(in crate::api) struct PolicyInput {
    /// The project's slug (adding only).
    #[serde(default)]
    project: String,
    /// Unique per project.
    name: String,
    /// The token issuer. Blank: GitHub Actions. It must be one the server accepts.
    #[serde(default)]
    issuer: String,
    /// The audience the token must carry. Blank: `fleet`.
    #[serde(default)]
    audience: String,
    /// The immutable numeric repository ID.
    repository_id: String,
    /// The immutable numeric owner ID.
    repository_owner_id: String,
    /// Refs whose pushes count as trusted (globs). Blank: none.
    #[serde(default)]
    trusted_refs: String,
    /// Events accepted. Blank: the default set.
    #[serde(default)]
    allowed_events: String,
    /// Only tokens minted inside this reusable workflow. Blank: any.
    #[serde(default)]
    job_workflow_ref: String,
    /// Only jobs in this environment. Blank: any.
    #[serde(default)]
    environment: String,
    /// `restricted` (the default), `full` or `deny`.
    #[serde(default)]
    pr_policy: String,
}

/// The policy a form describes, checked the way the configuration checks a policy.
fn policy_of(via: Via, state: &AppState, input: &PolicyInput) -> Result<PolicyRow, Box<Response>> {
    let cfg = TrustPolicyConfig {
        name: input.name.trim().to_owned(),
        issuer: optional(&input.issuer).unwrap_or_else(|| crate::config::GITHUB_ISSUER.to_owned()),
        audience: optional(&input.audience)
            .unwrap_or_else(|| fleet_protocol::brand::OIDC_AUDIENCE.to_owned()),
        repository_id: input.repository_id.trim().to_owned(),
        repository_owner_id: input.repository_owner_id.trim().to_owned(),
        trusted_refs: list(&input.trusted_refs),
        allowed_events: list(&input.allowed_events),
        pr_policy: optional(&input.pr_policy).unwrap_or_else(|| "restricted".to_owned()),
        job_workflow_ref: optional(&input.job_workflow_ref),
        environment: optional(&input.environment),
    };
    cfg.check(&state.config.oidc).map_err(|e| {
        refuse(
            via,
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("the trust policy is not valid: {e:#}"),
        )
    })?;
    Ok(PolicyRow {
        name: cfg.name.clone(),
        issuer: cfg.issuer.clone(),
        audience: cfg.audience.clone(),
        policy: cfg.policy(),
    })
}

/// Adds a publisher trust policy to a project.
#[utoipa::path(post, path = "/api/v1/admin/trust-policies", request_body = PolicyInput, responses(
    (status = 201, body = AdminChangedView), (status = 401, body = super::super::ApiError),
    (status = 403, body = super::super::ApiError), (status = 404, body = super::super::ApiError),
    (status = 409, body = super::super::ApiError), (status = 422, body = super::super::ApiError)))]
pub(in crate::api) async fn add_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let via = Via::of(&headers);
    let by = match administrator(&state, &headers, &body, via).await {
        Ok(by) => by,
        Err(r) => return *r,
    };
    let input: PolicyInput = match decode(via, &body) {
        Ok(i) => i,
        Err(r) => return *r,
    };
    let project = match project_of(&state, via, input.project.trim()).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let row = match policy_of(via, &state, &input) {
        Ok(r) => r,
        Err(r) => return *r,
    };
    match state.store.add_trust_policy(by, project, &row).await {
        Ok(id) => made(via, StatusCode::CREATED, id, TRUST),
        Err(e) => write_failed(via, e),
    }
}

/// Replaces a dashboard trust policy's fields. A configured policy is locked.
#[utoipa::path(put, path = "/api/v1/admin/trust-policies/{id}", request_body = PolicyInput,
    params(("id" = String, Path, description = "The policy's ID")), responses(
    (status = 200, body = AdminChangedView), (status = 401, body = super::super::ApiError),
    (status = 403, body = super::super::ApiError), (status = 404, body = super::super::ApiError),
    (status = 409, body = super::super::ApiError), (status = 422, body = super::super::ApiError)))]
pub(in crate::api) async fn edit_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    body: Bytes,
) -> Response {
    let via = Via::of(&headers);
    let by = match administrator(&state, &headers, &body, via).await {
        Ok(by) => by,
        Err(r) => return *r,
    };
    let id = match id_of(via, &raw, "trust policy") {
        Ok(id) => id,
        Err(r) => return *r,
    };
    let input: PolicyInput = match decode(via, &body) {
        Ok(i) => i,
        Err(r) => return *r,
    };
    let row = match policy_of(via, &state, &input) {
        Ok(r) => r,
        Err(r) => return *r,
    };
    match state.store.edit_trust_policy(by, id, &row).await {
        Ok(()) => made(via, StatusCode::OK, id, TRUST),
        Err(e) => write_failed(via, e),
    }
}

/// Revokes a dashboard trust policy: it grants nothing new, and the tokens it minted end now.
#[utoipa::path(post, path = "/api/v1/admin/trust-policies/{id}/revoke",
    params(("id" = String, Path, description = "The policy's ID")), responses(
    (status = 200, body = AdminChangedView), (status = 401, body = super::super::ApiError),
    (status = 403, body = super::super::ApiError), (status = 404, body = super::super::ApiError),
    (status = 409, body = super::super::ApiError)))]
pub(in crate::api) async fn revoke_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    body: Bytes,
) -> Response {
    let via = Via::of(&headers);
    let by = match administrator(&state, &headers, &body, via).await {
        Ok(by) => by,
        Err(r) => return *r,
    };
    let id = match id_of(via, &raw, "trust policy") {
        Ok(id) => id,
        Err(r) => return *r,
    };
    match state.store.revoke_trust_policy(by, id).await {
        Ok(()) => made(via, StatusCode::OK, id, TRUST),
        Err(e) => write_failed(via, e),
    }
}

/// Removes a dashboard trust policy; the tokens it minted end first. A configured policy is locked.
#[utoipa::path(delete, path = "/api/v1/admin/trust-policies/{id}",
    params(("id" = String, Path, description = "The policy's ID")), responses(
    (status = 204), (status = 401, body = super::super::ApiError),
    (status = 403, body = super::super::ApiError), (status = 404, body = super::super::ApiError),
    (status = 409, body = super::super::ApiError)))]
pub(in crate::api) async fn remove_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    body: Bytes,
) -> Response {
    let via = Via::of(&headers);
    let by = match administrator(&state, &headers, &body, via).await {
        Ok(by) => by,
        Err(r) => return *r,
    };
    let id = match id_of(via, &raw, "trust policy") {
        Ok(id) => id,
        Err(r) => return *r,
    };
    match state.store.remove_trust_policy(by, id).await {
        Ok(()) => removed(via, TRUST),
        Err(e) => write_failed(via, e),
    }
}
