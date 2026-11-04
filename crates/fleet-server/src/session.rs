// SPDX-License-Identifier: AGPL-3.0-only
//! Who is calling (PLAN §7.5): the session cookie, CSRF protection, and the [`Caller`] extractor
//! that turns a request into a `fleet_core::policy::Principal`.
//!
//! The session cookie is `__Host-fleet_session`, `HttpOnly`, `Secure`, `SameSite=Lax`.
//!
//! The `__Host-` prefix makes browsers refuse it unless it is Secure, has `Path=/` and no
//! `Domain`, so a sibling subdomain can't plant a cookie of the same name. `Lax` still sends it
//! on the top-level redirect back from the identity provider.

use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, HeaderValue, Method, StatusCode, header, request::Parts},
    response::Response,
};
use base64::Engine as _;
use fleet_core::policy::{Grant, Principal, Role};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::{api::error, ingest::bearer, state::AppState};

/// The cookie's name.
pub(crate) const COOKIE: &str = "__Host-fleet_session";

/// `n` random bytes, base64url without padding (43 characters for 32 bytes).
pub(crate) fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut buf)
        .expect("the OS random source should work");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// The session cookie's value in a request, if any.
pub(crate) fn read(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(name, value)| (name == COOKIE).then(|| value.to_owned()))
}

/// A `Set-Cookie` value that sets the session cookie for `max_age_seconds`.
pub(crate) fn set(value: &str, max_age_seconds: i64) -> HeaderValue {
    HeaderValue::try_from(format!(
        "{COOKIE}={value}; Path=/; Max-Age={max_age_seconds}; HttpOnly; Secure; SameSite=Lax"
    ))
    .expect("a base64url cookie value should be a valid header value")
}

/// A `Set-Cookie` value that deletes the session cookie.
pub(crate) fn clear() -> HeaderValue {
    set("", 0)
}

/// What a signed-in session keeps: who, what they may do (resolved at sign-in), and the CSRF
/// token the SPA echoes on mutating calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionData {
    pub(crate) grants: Vec<Grant>,
    pub(crate) csrf: String,
    pub(crate) email: Option<String>,
    pub(crate) name: Option<String>,
}

/// A live, signed-in session.
#[derive(Debug, Clone)]
pub(crate) struct Active {
    pub(crate) user_id: Uuid,
    pub(crate) data: SessionData,
    /// The cookie value, to end the session.
    pub(crate) cookie: String,
}

/// The signed-in session a request's cookie names. `None` when there is no cookie, it is
/// unknown or expired, or it is only a sign-in transaction.
pub(crate) async fn active(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<Active>, fleet_store::Error> {
    let Some(cookie) = read(headers) else {
        return Ok(None);
    };
    let Some(record) = state.store.session(&cookie).await? else {
        return Ok(None);
    };
    let Some(user_id) = record.user_id else {
        return Ok(None);
    };
    match serde_json::from_value(record.data) {
        Ok(data) => Ok(Some(Active {
            user_id,
            data,
            cookie,
        })),
        Err(e) => {
            tracing::warn!(error = %e, "a session has data this build can't read");
            Ok(None)
        }
    }
}

/// Whether the request's `Origin` is the dashboard's own. A request without one isn't from a
/// browser page, so it carries no ambient credential to abuse.
pub(crate) fn same_origin(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) else {
        return true;
    };
    let origin = origin.trim_end_matches('/');
    match &state.config.api.public_url {
        Some(public) => origin.eq_ignore_ascii_case(public.trim_end_matches('/')),
        None => headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|host| {
                origin
                    .split_once("://")
                    .is_some_and(|(_, rest)| rest.eq_ignore_ascii_case(host))
            }),
    }
}

/// The CSRF header the SPA echoes on mutating requests (from `GET /api/v1/auth/session`).
const CSRF_HEADER: &str = "x-csrf-token";

fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

/// Equality of two secrets without an early exit on the first differing byte (compares
/// fixed-size digests).
pub(crate) fn same_secret(a: &str, b: &str) -> bool {
    digest(a) == digest(b)
}

/// The signed-in session of a request, with CSRF protection when the method can change state.
///
/// A mutating request on a cookie must come from the dashboard's own origin and echo the
/// session's CSRF token; both defeat a forged cross-site form or fetch. Bearer credentials are
/// not ambient, so they never go through here. `Ok(None)`: no session.
pub(crate) async fn checked_session(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
) -> Result<Option<Active>, Box<Response>> {
    let active = match active(state, headers).await {
        Ok(Some(a)) => a,
        Ok(None) => return Ok(None),
        Err(e) => {
            tracing::error!(error = %e, "reading a session");
            return Err(Box::new(error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
            )));
        }
    };
    if method.is_safe() {
        return Ok(Some(active));
    }
    let given = headers
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let token_ok = same_secret(given, &active.data.csrf);
    if !token_ok || !same_origin(state, headers) {
        tracing::warn!(token_ok, "a mutating request failed the CSRF check");
        return Err(Box::new(error(
            StatusCode::FORBIDDEN,
            "missing or wrong CSRF token, or a foreign origin",
        )));
    }
    Ok(Some(active))
}

/// A signed-in session (for logout), CSRF-checked.
#[derive(Debug)]
pub(crate) struct SignedIn(pub(crate) Active);

impl FromRequestParts<AppState> for SignedIn {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        match checked_session(state, &parts.method, &parts.headers)
            .await
            .map_err(|r| *r)?
        {
            Some(active) => Ok(Self(active)),
            None => Err(error(StatusCode::UNAUTHORIZED, "not signed in")),
        }
    }
}

fn viewer() -> Principal {
    Principal::User {
        grants: vec![Grant {
            project: None,
            role: Role::Viewer,
        }],
    }
}

/// Who a request is, or the refusal. In order: a configured viewer token (reads everything), a
/// session cookie (the roles resolved at sign-in), then anonymous reads if the operator turned
/// them on for development.
pub(crate) async fn principal(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
) -> Result<Principal, Box<Response>> {
    if let Some(token) = bearer(headers) {
        let given = digest(&token);
        // Compare digests: equal length, and no early exit on the secret's bytes.
        let known = state
            .config
            .api
            .viewer_tokens
            .iter()
            .fold(false, |ok, t| ok | (digest(t.expose()) == given));
        if known {
            return Ok(viewer());
        }
    }
    if let Some(active) = checked_session(state, method, headers).await? {
        return Ok(Principal::User {
            grants: active.data.grants,
        });
    }
    if state.config.api.anonymous_read {
        return Ok(viewer());
    }
    Err(Box::new(error(
        StatusCode::UNAUTHORIZED,
        "sign in, or send a viewer token",
    )))
}

/// The caller of a request, as a policy principal.
#[derive(Debug)]
pub(crate) struct Caller(pub(crate) Principal);

impl FromRequestParts<AppState> for Caller {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        principal(state, &parts.method, &parts.headers)
            .await
            .map(Self)
            .map_err(|r| *r)
    }
}
