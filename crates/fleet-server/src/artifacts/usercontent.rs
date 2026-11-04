// SPDX-License-Identifier: AGPL-3.0-only
//! The usercontent origin (PLAN §7.6, invariant I7; research 06 §6.2, threats T9 and T15).
//!
//! Maestro HTML reports and raw artifacts are attacker-influenced files. They are served only
//! here, from a host that is not the dashboard's, and:
//!
//! - **only by signed link** (`links`): no session, no cookie is read or set; a link names one
//!   artifact of one project until one moment;
//! - **sandboxed**: `Content-Security-Policy: sandbox …` gives HTML an opaque origin with no
//!   scripts, `X-Content-Type-Options: nosniff` stops a browser reinterpreting a file, and the
//!   `Content-Type` is what the verified bytes are, not what the uploader claimed;
//! - **as a download unless harmless**: images and (sandboxed) HTML are shown inline, everything
//!   else is `Content-Disposition: attachment`.
//!
//! [`router`] is mounted by `Host` on the main listener ([`dispatch`]) and, with
//! `usercontent.listen`, on a listener of its own. Nothing else is routed here.

use std::convert::Infallible;

use axum::{
    Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use fleet_blob::{BlobError, BlobKey, Sha256Digest};
use fleet_protocol::artifact::ArtifactClass;
use time::OffsetDateTime;
use tower::ServiceExt as _;
use tower_http::set_header::SetResponseHeaderLayer;

use super::links::Refusal;
use crate::state::AppState;

/// `scheme://host[:port]` of the dashboard, from `api.public_url`.
fn app_origin(state: &AppState) -> Option<String> {
    let url = url::Url::parse(state.config.api.public_url.as_deref()?).ok()?;
    Some(url.origin().ascii_serialization())
}

/// The policy of every response from this origin, errors included.
fn csp(app_origin: Option<&str>) -> String {
    format!(
        "sandbox; default-src 'none'; img-src 'self' data:; media-src 'self'; \
         style-src 'self' 'unsafe-inline'; font-src 'self' data:; form-action 'none'; \
         frame-ancestors {}",
        app_origin.unwrap_or("'none'")
    )
}

/// Everything the origin serves is [`router`]; every response gets these headers.
pub(crate) fn router(state: &AppState) -> Router {
    let origin = app_origin(state);
    let mut headers: Vec<(HeaderName, HeaderValue)> = vec![
        (
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::try_from(csp(origin.as_deref()))
                .unwrap_or(HeaderValue::from_static("sandbox; default-src 'none'")),
        ),
        (
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ),
        // The dashboard embeds screenshots across sites, and the signed link is the access
        // control, so the resource is shareable (research 06 recommends same-site, which assumes
        // the dashboard and this origin share a registrable domain; they must not).
        (
            HeaderName::from_static("cross-origin-resource-policy"),
            HeaderValue::from_static("cross-origin"),
        ),
        (
            HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ),
        (
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ),
        (
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=300"),
        ),
        (header::VARY, HeaderValue::from_static("Origin")),
    ];
    // The dashboard's own scripts may fetch an artifact (the log viewer); nobody else's.
    if let Some(origin) = origin.and_then(|o| HeaderValue::try_from(o).ok()) {
        headers.push((header::ACCESS_CONTROL_ALLOW_ORIGIN, origin));
    }
    let router = Router::new()
        .route(
            "/u/{project}/{artifact}/{expires}/{signature}/{name}",
            get(serve),
        )
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(state.clone());
    headers.into_iter().fold(router, |router, (name, value)| {
        router.layer(SetResponseHeaderLayer::overriding(name, value))
    })
}

/// Sends requests for the usercontent host to [`router`] and everything else to `app`. A request
/// for the usercontent host never reaches the app's routes, and the app's routes never serve
/// artifact bytes.
pub(crate) fn dispatch(app: Router, usercontent: Router, hosts: Vec<String>) -> Router {
    Router::new().fallback(move |req: Request| {
        let (app, usercontent, hosts) = (app.clone(), usercontent.clone(), hosts.clone());
        async move {
            let host = req
                .headers()
                .get(header::HOST)
                .and_then(|h| h.to_str().ok())
                .map(str::to_ascii_lowercase);
            let to_usercontent = host.is_some_and(|h| hosts.contains(&h));
            let routed: Result<Response, Infallible> = if to_usercontent {
                usercontent.oneshot(req).await
            } else {
                app.oneshot(req).await
            };
            routed.unwrap_or_else(|never| match never {})
        }
    })
}

fn refuse(status: StatusCode, message: &'static str) -> Response {
    (status, message).into_response()
}

async fn serve(
    State(state): State<AppState>,
    Path((project, artifact, expires, signature, _name)): Path<(
        String,
        String,
        String,
        String,
        String,
    )>,
) -> Response {
    // No `Cookie` header is ever read: this origin has no session, and a link works the same
    // with or without one.
    let Some(signer) = &state.signer else {
        return refuse(StatusCode::NOT_FOUND, "not found");
    };
    let granted = match signer.verify(
        &project,
        &artifact,
        &expires,
        &signature,
        OffsetDateTime::now_utc(),
    ) {
        Ok(g) => g,
        Err(Refusal::BadSignature) => return refuse(StatusCode::FORBIDDEN, "invalid link"),
        Err(Refusal::Expired) => return refuse(StatusCode::GONE, "this link has expired"),
    };
    let servable = match state
        .store
        .servable(granted.project, granted.artifact)
        .await
    {
        Ok(Some(s)) => s,
        Ok(None) => return refuse(StatusCode::NOT_FOUND, "not found"),
        Err(e) => {
            tracing::error!(error = %e, "looking up an artifact to serve");
            return refuse(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    let (Some(digest), Ok(blobs)) = (
        Sha256Digest::from_hex(&servable.sha256),
        state.blobs().await,
    ) else {
        return refuse(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
    };
    let blob = match blobs
        .get(&BlobKey::new(granted.project, digest), None)
        .await
    {
        Ok(b) => b,
        Err(BlobError::NotFound) => return refuse(StatusCode::NOT_FOUND, "not found"),
        Err(e) => {
            tracing::error!(error = %e, "reading a blob");
            return refuse(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    let content_type = match servable.content_type.as_str() {
        t @ ("text/plain" | "text/html" | "application/xml") => format!("{t}; charset=utf-8"),
        t => t.to_owned(),
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_LENGTH, blob.size.to_string()),
            (
                header::CONTENT_DISPOSITION,
                disposition(servable.class, &servable.content_type, &servable.name),
            ),
        ],
        Body::from_stream(blob.body),
    )
        .into_response()
}

/// Images and HTML (which is sandboxed) are shown; everything else is saved.
fn disposition(class: ArtifactClass, content_type: &str, name: &str) -> String {
    let inline = content_type.starts_with("image/")
        || (class == ArtifactClass::HtmlReport && content_type == "text/html");
    let kind = if inline { "inline" } else { "attachment" };
    // An ASCII fallback, plus the exact name for browsers that read `filename*` (RFC 6266).
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && !matches!(c, '"' | '\\' | '%' | ';') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_') {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}
