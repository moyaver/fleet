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

use std::{convert::Infallible, ops::Range};

use axum::{
    Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use fleet_blob::{BlobError, BlobKey, Sha256Digest};
use fleet_protocol::artifact::ArtifactClass;
use fleet_store::auth::AuditActor;
use time::OffsetDateTime;
use tower::ServiceExt as _;
use tower_http::set_header::SetResponseHeaderLayer;
use uuid::Uuid;

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
        (header::ACCEPT_RANGES, HeaderValue::from_static("bytes")),
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
    headers: HeaderMap,
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
    let size = u64::try_from(servable.size_bytes).unwrap_or(0);
    let wanted = match ask(
        headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
        headers.contains_key(header::IF_RANGE),
        size,
    ) {
        Ask::Whole => None,
        Ask::Part(range) => Some(range),
        Ask::Unsatisfiable => return unsatisfiable(size),
    };
    let partial = wanted.is_some();
    // Media and reports are audited (research 06 T7). The link names no user, so the actor is the
    // system and the link's artifact is the target. A refused audit write refuses the read: an
    // untracked read of a screenshot is not allowed.
    if is_audited(servable.class)
        && !audit_read(
            &state,
            granted.project,
            granted.artifact,
            servable.class,
            partial,
        )
        .await
    {
        return refuse(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
    }
    let blob = match blobs
        .get(&BlobKey::new(granted.project, digest), wanted)
        .await
    {
        Ok(b) => b,
        Err(BlobError::RangeNotSatisfiable { size }) => return unsatisfiable(size),
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
    let length = blob.range.end - blob.range.start;
    let mut response = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, length.to_string())
        .header(
            header::CONTENT_DISPOSITION,
            disposition(servable.class, &servable.content_type, &servable.name),
        );
    if partial {
        response = response.header(
            header::CONTENT_RANGE,
            format!(
                "bytes {}-{}/{}",
                blob.range.start,
                blob.range.end - 1,
                blob.size
            ),
        );
    }
    response
        .body(Body::from_stream(blob.body))
        .unwrap_or_else(|_| refuse(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))
}

/// A `416` for a range the blob cannot give, with the size it has.
fn unsatisfiable(size: u64) -> Response {
    (
        StatusCode::RANGE_NOT_SATISFIABLE,
        [(header::CONTENT_RANGE, format!("bytes */{size}"))],
    )
        .into_response()
}

/// What a request's `Range` header asks for, of a blob of a known size (RFC 9110 §14).
#[derive(Debug, PartialEq, Eq)]
enum Ask {
    /// The whole blob: no `Range`, or one this server ignores (another unit, several ranges, a
    /// malformed range, or an `If-Range`, which needs a validator this origin does not keep).
    Whole,
    /// One byte range, clamped to the blob's end.
    Part(Range<u64>),
    /// Not satisfiable: a `416`.
    Unsatisfiable,
}

/// Reads a single `bytes=` range. Anything this does not implement is ignored, which HTTP allows.
fn ask(range: Option<&str>, if_range: bool, size: u64) -> Ask {
    let Some(spec) = range.and_then(|r| r.trim().strip_prefix("bytes=")) else {
        return Ask::Whole;
    };
    if if_range || spec.contains(',') {
        return Ask::Whole;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return Ask::Whole;
    };
    let (first, last) = (first.trim(), last.trim());
    match (first.is_empty(), last.is_empty()) {
        // `-N`: the last N bytes.
        (true, false) => match last.parse::<u64>() {
            Ok(n) if n > 0 && size > 0 => Ask::Part(size.saturating_sub(n)..size),
            Ok(_) => Ask::Unsatisfiable,
            Err(_) => Ask::Whole,
        },
        // `N-` or `N-M`.
        (false, _) => {
            let Ok(start) = first.parse::<u64>() else {
                return Ask::Whole;
            };
            let end = if last.is_empty() {
                size
            } else {
                match last.parse::<u64>() {
                    // Syntactically invalid when the last byte is before the first.
                    Ok(end) if end < start => return Ask::Whole,
                    Ok(end) => end.saturating_add(1),
                    Err(_) => return Ask::Whole,
                }
            };
            if start >= size {
                Ask::Unsatisfiable
            } else {
                Ask::Part(start..end.min(size))
            }
        }
        _ => Ask::Whole,
    }
}

/// Writes the audit row for a read of an artifact through a signed link (research 06 T7). The
/// link names no user, so the actor is the system and the artifact is the target. `false` when the
/// row cannot be written: the read is then refused, so no untracked read is served.
async fn audit_read(
    state: &AppState,
    project: Uuid,
    artifact: Uuid,
    class: ArtifactClass,
    partial: bool,
) -> bool {
    let details = serde_json::json!({
        "via": "signed_link",
        "artifact_id": artifact,
        "class": class.as_str(),
        "partial": partial,
    });
    match state
        .store
        .audit(
            AuditActor::System,
            Some(project),
            "artifact.fetched",
            details,
        )
        .await
    {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(error = %e, "auditing an artifact read");
            false
        }
    }
}

/// Classes whose every read through a signed link is audited (research 06 T7): screenshots,
/// which may show PII, and the reports. Text evidence is redacted before it is exported.
const fn is_audited(class: ArtifactClass) -> bool {
    matches!(
        class,
        ArtifactClass::FailureScreenshot | ArtifactClass::Junit | ArtifactClass::HtmlReport
    )
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

#[cfg(test)]
mod tests {
    use super::{Ask, ask};

    #[test]
    fn a_single_range_is_read_and_clamped_to_the_blob() {
        assert_eq!(ask(Some("bytes=0-99"), false, 1000), Ask::Part(0..100));
        assert_eq!(ask(Some("bytes=500-"), false, 1000), Ask::Part(500..1000));
        assert_eq!(
            ask(Some("bytes=900-5000"), false, 1000),
            Ask::Part(900..1000)
        );
        assert_eq!(ask(Some("bytes=-100"), false, 1000), Ask::Part(900..1000));
        // A suffix longer than the blob is the whole blob.
        assert_eq!(ask(Some("bytes=-5000"), false, 1000), Ask::Part(0..1000));
        assert_eq!(ask(Some(" bytes=7-7 "), false, 1000), Ask::Part(7..8));
    }

    #[test]
    fn a_range_the_blob_cannot_give_is_416() {
        assert_eq!(ask(Some("bytes=1000-"), false, 1000), Ask::Unsatisfiable);
        assert_eq!(
            ask(Some("bytes=5000-6000"), false, 1000),
            Ask::Unsatisfiable
        );
        assert_eq!(ask(Some("bytes=-0"), false, 1000), Ask::Unsatisfiable);
        assert_eq!(ask(Some("bytes=0-0"), false, 0), Ask::Unsatisfiable);
        assert_eq!(ask(Some("bytes=-10"), false, 0), Ask::Unsatisfiable);
    }

    #[test]
    fn what_is_not_implemented_is_ignored_and_the_whole_blob_is_sent() {
        for header in [
            None,
            Some("items=0-1"),
            Some("bytes=0-1,5-6"),
            Some("bytes=-"),
            Some("bytes=a-b"),
            Some("bytes=9-3"),
            Some("bytes=3"),
        ] {
            assert_eq!(ask(header, false, 1000), Ask::Whole, "{header:?}");
        }
        // With If-Range there is no validator to compare, so the whole blob is the safe answer.
        assert_eq!(ask(Some("bytes=0-9"), true, 1000), Ask::Whole);
    }
}
