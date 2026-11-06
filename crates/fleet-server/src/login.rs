// SPDX-License-Identifier: AGPL-3.0-only
//! Browser sign-in: OIDC authorization code with PKCE, state and nonce (PLAN §7.5).
//!
//! - `GET /api/v1/auth/login` stores the transaction (state, nonce, PKCE verifier) in a
//!   short-lived session row and redirects to the provider.
//! - `GET /api/v1/auth/callback` consumes that row, exchanges the code, verifies the ID token,
//!   maps the user's groups to roles and starts the session.
//! - `GET /api/v1/auth/session` tells the SPA who is signed in, their roles and the CSRF token.
//! - `POST /api/v1/auth/logout` (CSRF-checked) ends the session.

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine as _;
use fleet_core::policy::Role;
use fleet_protocol::rest::{SessionRole, SessionUser, SessionView};
use fleet_store::auth::AuditActor;
use serde::Deserialize;
use serde_json::json;
use sha2::Digest as _;
use time::OffsetDateTime;
use url::Url;

use crate::{
    api::error,
    client_ip::ClientIp,
    config::Login,
    limits::too_many_requests,
    login::idp::IdClaims,
    oidc::Refused,
    session::{self, SignedIn, same_secret},
    state::AppState,
};

pub(crate) mod idp;

/// How long the provider has to send the user back.
const TRANSACTION_MINUTES: i64 = 10;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/auth/login", get(login))
        .route("/api/v1/auth/callback", get(callback))
        .route("/api/v1/auth/session", get(whoami))
        .route("/api/v1/auth/logout", post(logout))
}

fn internal(e: &dyn std::fmt::Display) -> Response {
    *Refusal::internal(e).0
}

#[derive(Debug, Deserialize)]
struct LoginQuery {
    /// Where to land afterwards: a path on this origin.
    return_to: Option<String>,
}

async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Query(q): Query<LoginQuery>,
) -> Response {
    if let Err(wait) = state.limits.ip.check(ip, std::time::Instant::now()) {
        return too_many_requests(wait);
    }
    let Some(config) = &state.config.login else {
        return error(StatusCode::NOT_FOUND, "browser sign-in is not configured");
    };
    let endpoints = match state.idp.endpoints(&config.issuer).await {
        Ok(e) => e,
        Err(Refused::Unavailable(m) | Refused::Invalid(m)) => {
            tracing::warn!(reason = %m, "the identity provider is unusable");
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "the identity provider is unavailable",
            );
        }
    };
    let (csrf_state, nonce, verifier) = (
        session::random_token(32),
        session::random_token(32),
        session::random_token(32),
    );
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    let Ok(mut to_idp) = Url::parse(&endpoints.authorization_endpoint) else {
        tracing::warn!("the provider's authorization endpoint is not a URL");
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the identity provider is misconfigured",
        );
    };
    let scope = std::iter::once("openid")
        .chain(
            config
                .scopes
                .iter()
                .map(String::as_str)
                .filter(|s| *s != "openid"),
        )
        .collect::<Vec<_>>()
        .join(" ");
    to_idp.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", &config.client_id),
        ("redirect_uri", &redirect_uri(&state)),
        ("scope", &scope),
        ("state", &csrf_state),
        ("nonce", &nonce),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ]);
    let transaction = json!({
        "state": csrf_state, "nonce": nonce, "verifier": verifier,
        "return_to": q.return_to,
    });
    let expires = OffsetDateTime::now_utc() + time::Duration::minutes(TRANSACTION_MINUTES);
    let cookie = match state
        .store
        .create_session(None, &transaction, expires)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "storing a sign-in transaction");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    (
        StatusCode::SEE_OTHER,
        [
            (
                header::LOCATION,
                HeaderValue::try_from(to_idp.as_str()).expect("a URL should be a header value"),
            ),
            (
                header::SET_COOKIE,
                session::set(cookie.expose(), TRANSACTION_MINUTES * 60),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// What a sign-in transaction stored.
#[derive(Debug, Deserialize)]
struct Transaction {
    state: String,
    nonce: String,
    verifier: String,
    return_to: Option<String>,
}

/// A refusal. The response is boxed: `Response` is large for an `Err`.
#[derive(Debug)]
struct Refusal(Box<Response>);

impl Refusal {
    fn new(status: StatusCode, message: &str) -> Self {
        Self(Box::new(error(status, message)))
    }

    fn internal(e: &dyn std::fmt::Display) -> Self {
        tracing::error!(error = %e, "sign-in request failed");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }

    fn unusable(r: &Refused) -> Self {
        match r {
            Refused::Unavailable(m) => {
                tracing::warn!(reason = %m, "the identity provider is unavailable");
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the identity provider is unavailable",
                )
            }
            Refused::Invalid(m) => {
                tracing::warn!(reason = %m, "sign-in refused");
                Self::new(
                    StatusCode::UNAUTHORIZED,
                    "the identity provider's answer was refused",
                )
            }
        }
    }
}

async fn callback(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    if let Err(wait) = state.limits.ip.check(ip, std::time::Instant::now()) {
        return too_many_requests(wait);
    }
    complete(&state, &headers, q).await.unwrap_or_else(|r| *r.0)
}

/// The whole callback; the first refusal wins.
async fn complete(
    state: &AppState,
    headers: &HeaderMap,
    q: CallbackQuery,
) -> Result<Response, Refusal> {
    let Some(config) = &state.config.login else {
        return Err(Refusal::new(
            StatusCode::NOT_FOUND,
            "browser sign-in is not configured",
        ));
    };
    let transaction = take_transaction(state, headers).await?;
    if let Some(reason) = q.error {
        tracing::warn!(%reason, "the identity provider refused the sign-in");
        return Err(Refusal::new(
            StatusCode::FORBIDDEN,
            "the identity provider refused the sign-in",
        ));
    }
    let (Some(code), Some(returned_state)) = (q.code, q.state) else {
        return Err(Refusal::new(
            StatusCode::BAD_REQUEST,
            "no authorization code or state",
        ));
    };
    // Binds the callback to the browser that started the login (login CSRF).
    if !same_secret(&returned_state, &transaction.state) {
        tracing::warn!("a sign-in callback carried a state this browser never got");
        return Err(Refusal::new(
            StatusCode::BAD_REQUEST,
            "the sign-in state does not match: sign in again",
        ));
    }
    let claims = verified_claims(state, config, &code, &transaction).await?;
    let Some(groups) = claims.groups(&config.group_claims) else {
        // Deny, don't guess: without group claims nothing says what this user may see.
        tracing::warn!(
            overage = claims.groups_overage(),
            "an ID token has none of the configured group claims"
        );
        return Err(Refusal::new(
            StatusCode::FORBIDDEN,
            "your sign-in carries no group claims: ask your identity-provider admin",
        ));
    };
    start_session(
        state,
        config,
        claims,
        &groups,
        transaction.return_to.as_deref(),
    )
    .await
}

/// Exchanges the code and verifies the ID token against this login.
async fn verified_claims(
    state: &AppState,
    config: &Login,
    code: &str,
    transaction: &Transaction,
) -> Result<IdClaims, Refusal> {
    let endpoints = state
        .idp
        .endpoints(&config.issuer)
        .await
        .map_err(|r| Refusal::unusable(&r))?;
    let id_token = state
        .idp
        .exchange(
            config,
            &endpoints,
            code,
            &transaction.verifier,
            &redirect_uri(state),
        )
        .await
        .map_err(|r| Refusal::unusable(&r))?;
    let claims = state
        .idp
        .verify(
            &state.jwks,
            config,
            state.config.oidc.leeway_seconds,
            &id_token,
        )
        .await
        .map_err(|r| Refusal::unusable(&r))?;
    // Binds the ID token to this login (replay of a token issued for another one).
    if !claims
        .nonce
        .as_deref()
        .is_some_and(|n| same_secret(n, &transaction.nonce))
    {
        tracing::warn!("an ID token's nonce does not match the login");
        return Err(Refusal::new(
            StatusCode::UNAUTHORIZED,
            "the ID token does not belong to this sign-in",
        ));
    }
    Ok(claims)
}

/// Resolves the user's roles and starts the session; denies a user who holds none.
async fn start_session(
    state: &AppState,
    config: &Login,
    claims: IdClaims,
    groups: &[String],
    return_to: Option<&str>,
) -> Result<Response, Refusal> {
    let name = claims
        .name
        .clone()
        .or_else(|| claims.preferred_username.clone());
    let user = state
        .store
        .upsert_user(
            &config.issuer,
            &claims.sub,
            claims.email.as_deref(),
            name.as_deref(),
        )
        .await
        .map_err(|e| Refusal::internal(&e))?;
    let grants = state
        .store
        .grants_for(user, &config.issuer, groups)
        .await
        .map_err(|e| Refusal::internal(&e))?;
    if grants.is_empty() {
        return Err(Refusal::new(
            StatusCode::FORBIDDEN,
            "your groups grant no access to Fleet",
        ));
    }
    let data = session::SessionData {
        grants,
        csrf: session::random_token(32),
        email: claims.email,
        name,
    };
    let ttl = i64::try_from(config.session_ttl_minutes).unwrap_or(480) * 60;
    let expires = OffsetDateTime::now_utc() + time::Duration::seconds(ttl);
    // A new session id at sign-in: nothing the browser held before carries over.
    let cookie = state
        .store
        .create_session(Some(user), &json!(data), expires)
        .await
        .map_err(|e| Refusal::internal(&e))?;
    if let Err(e) = state
        .store
        .audit(AuditActor::User(user), None, "auth.login", json!({}))
        .await
    {
        tracing::warn!(error = %e, "auditing a sign-in");
    }
    Ok((
        StatusCode::SEE_OTHER,
        [
            (
                header::LOCATION,
                HeaderValue::try_from(local_path(return_to))
                    .unwrap_or_else(|_| HeaderValue::from_static("/")),
            ),
            (header::SET_COOKIE, session::set(cookie.expose(), ttl)),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
    )
        .into_response())
}

/// The sign-in transaction the request's cookie names, consumed whatever happens next: a
/// callback can't be replayed.
async fn take_transaction(state: &AppState, headers: &HeaderMap) -> Result<Transaction, Refusal> {
    let record = match session::read(headers) {
        Some(cookie) => state
            .store
            .take_session(&cookie)
            .await
            .map_err(|e| Refusal::internal(&e))?,
        None => None,
    };
    record
        .filter(|r| r.user_id.is_none())
        .and_then(|r| serde_json::from_value::<Transaction>(r.data).ok())
        .ok_or_else(|| {
            Refusal::new(
                StatusCode::BAD_REQUEST,
                "the sign-in expired or was not started here: sign in again",
            )
        })
}

/// `return_to` if it is a path on this site, else `/`. A scheme, a host (`//host`) or a
/// backslash (browsers read `/\host` as `//host`) would make the callback an open redirect.
fn local_path(return_to: Option<&str>) -> &str {
    return_to
        .filter(|p| {
            p.starts_with('/')
                && !p.starts_with("//")
                && !p.contains('\\')
                && !p.chars().any(char::is_control)
        })
        .unwrap_or("/")
}

async fn whoami(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let active = match session::active(&state, &headers).await {
        Ok(Some(a)) => a,
        Ok(None) => return error(StatusCode::UNAUTHORIZED, "not signed in"),
        Err(e) => return internal(&e),
    };
    let slugs = match state.store.projects().await {
        Ok(p) => p,
        Err(e) => return internal(&e),
    };
    let roles = active
        .data
        .grants
        .iter()
        .filter_map(|g| match g.project {
            None => Some(SessionRole {
                project: None,
                role: role_name(g.role).to_owned(),
            }),
            Some(id) => slugs.iter().find(|p| p.id == id).map(|p| SessionRole {
                project: Some(p.slug.clone()),
                role: role_name(g.role).to_owned(),
            }),
        })
        .collect();
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(SessionView {
            user: SessionUser {
                id: active.user_id,
                email: active.data.email,
                name: active.data.name,
            },
            roles,
            csrf_token: active.data.csrf,
        }),
    )
        .into_response()
}

async fn logout(State(state): State<AppState>, SignedIn(active): SignedIn) -> Response {
    if let Err(e) = state.store.delete_session(&active.cookie).await {
        return internal(&e);
    }
    if let Err(e) = state
        .store
        .audit(
            AuditActor::User(active.user_id),
            None,
            "auth.logout",
            json!({}),
        )
        .await
    {
        tracing::warn!(error = %e, "auditing a sign-out");
    }
    (
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, session::clear())],
    )
        .into_response()
}

/// A role as the API spells it (the same words `fleet_core::policy::Role` serializes to).
fn role_name(role: Role) -> &'static str {
    match role {
        Role::Viewer => "viewer",
        Role::Maintainer => "maintainer",
        Role::Admin => "admin",
    }
}

/// Where the provider sends the user back: `<public_url>/api/v1/auth/callback`.
fn redirect_uri(state: &AppState) -> String {
    let base = state.config.api.public_url.as_deref().unwrap_or_default();
    format!("{}/api/v1/auth/callback", base.trim_end_matches('/'))
}

#[cfg(test)]
mod tests;
