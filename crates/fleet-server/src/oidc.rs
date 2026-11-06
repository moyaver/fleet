// SPDX-License-Identifier: AGPL-3.0-only
//! `POST /api/v1/agent/token`: CI OIDC token → short-lived Fleet agent token (PLAN §7.5,
//! ADR-0008, research 06 §2.5).
//!
//! Verification: the issuer must be configured (an unlisted `iss` is never fetched, no SSRF);
//! RS256 only; keys from the issuer's discovery document, cached for an hour and refetched (at
//! most once a minute) on an unknown `kid`; `aud`, `exp`, `nbf` checked with leeway; `iat` no
//! older than 10 minutes; each `jti` accepted once. Then trust policies match the immutable
//! numeric repository claims and decide the trust level (`fleet_core::trust`).
//!
//! The same endpoint exchanges an API key (as the bearer) for an agent token, so downstream
//! code has one credential path.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::Engine as _;
use fleet_core::trust::{self, Decision, Trust};
use fleet_store::auth::{CiClaims, Credential, TokenSource, TrustLevel};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, jwk::JwkSet};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::Mutex;

use crate::{
    api::error, client_ip::ClientIp, config::Oidc, ingest::bearer, limits::too_many_requests,
    state::AppState,
};

mod rs256;

/// Keys are refreshed after this long.
const JWKS_TTL: Duration = Duration::from_secs(3600);
/// An unknown `kid` refetches at most this often.
const REFETCH_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct Cached {
    keys: JwkSet,
    fetched: Instant,
}

/// Per-issuer JWKS cache.
#[derive(Debug)]
pub(crate) struct Jwks {
    client: reqwest::Client,
    cache: Mutex<HashMap<String, Cached>>,
}

/// Why a token was refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Refused {
    #[error("invalid token: {0}")]
    Invalid(String),
    #[error("issuer unavailable: {0}")]
    Unavailable(String),
}

impl Jwks {
    pub(crate) fn new() -> anyhow::Result<Self> {
        // TLS: rustls with ring and the OS trust store, so a corporate CA in the image works.
        let _ = rustls::crypto::ring::default_provider().install_default();
        rs256::install();
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            cache: Mutex::new(HashMap::new()),
        })
    }

    async fn fetch(&self, issuer: &str) -> Result<JwkSet, Refused> {
        #[derive(Deserialize)]
        struct Discovery {
            jwks_uri: String,
        }
        let unavailable = |e: reqwest::Error| Refused::Unavailable(e.to_string());
        let discovery: Discovery = self
            .client
            .get(format!(
                "{}/.well-known/openid-configuration",
                issuer.trim_end_matches('/')
            ))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(unavailable)?
            .json()
            .await
            .map_err(unavailable)?;
        self.client
            .get(&discovery.jwks_uri)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(unavailable)?
            .json()
            .await
            .map_err(unavailable)
    }

    /// The key for `kid`, refetching once when it is unknown or the cache is old.
    pub(crate) async fn key(&self, issuer: &str, kid: &str) -> Result<DecodingKey, Refused> {
        let mut cache = self.cache.lock().await;
        let usable = cache
            .get(issuer)
            .filter(|c| c.fetched.elapsed() < JWKS_TTL)
            .and_then(|c| c.keys.find(kid).cloned());
        let jwk = if let Some(jwk) = usable {
            jwk
        } else {
            {
                let may_fetch = cache
                    .get(issuer)
                    .is_none_or(|c| c.fetched.elapsed() >= REFETCH_EVERY);
                if !may_fetch {
                    return Err(Refused::Invalid(format!("unknown key {kid}")));
                }
                let keys = self.fetch(issuer).await?;
                let jwk = keys.find(kid).cloned();
                cache.insert(
                    issuer.to_owned(),
                    Cached {
                        keys,
                        fetched: Instant::now(),
                    },
                );
                jwk.ok_or_else(|| Refused::Invalid(format!("unknown key {kid}")))?
            }
        };
        DecodingKey::from_jwk(&jwk).map_err(|e| Refused::Invalid(e.to_string()))
    }
}

/// The claims Fleet reads from a CI OIDC token (GitHub's names).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OidcClaims {
    iss: String,
    #[serde(default)]
    jti: Option<String>,
    iat: i64,
    exp: i64,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    repository_id: Option<String>,
    #[serde(default)]
    repository_owner_id: Option<String>,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    run_attempt: Option<String>,
    #[serde(default)]
    check_run_id: Option<String>,
    #[serde(default)]
    sha: Option<String>,
    #[serde(rename = "ref", default)]
    git_ref: Option<String>,
    #[serde(default)]
    event_name: Option<String>,
    #[serde(default)]
    job_workflow_ref: Option<String>,
    #[serde(default)]
    environment: Option<String>,
}

/// The `iss` of an unverified token, only to pick the configured issuer to verify against.
fn peek_issuer(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("iss")?.as_str().map(str::to_owned)
}

/// Verifies a CI OIDC token; returns its claims and the audience it was checked against.
pub(crate) async fn verify(
    jwks: &Jwks,
    cfg: &Oidc,
    token: &str,
) -> Result<(OidcClaims, String), Refused> {
    let header = jsonwebtoken::decode_header(token).map_err(|e| Refused::Invalid(e.to_string()))?;
    if header.alg != Algorithm::RS256 {
        return Err(Refused::Invalid("only RS256 is accepted".into()));
    }
    let kid = header
        .kid
        .ok_or_else(|| Refused::Invalid("the token has no kid".into()))?;
    let iss = peek_issuer(token).ok_or_else(|| Refused::Invalid("no issuer".into()))?;
    let issuer = cfg
        .issuers
        .iter()
        .find(|i| i.issuer == iss)
        .ok_or_else(|| Refused::Invalid(format!("issuer {iss} is not accepted")))?;
    let key = jwks.key(&issuer.issuer, &kid).await?;
    let mut v = Validation::new(Algorithm::RS256);
    v.set_issuer(&[&issuer.issuer]);
    v.set_audience(&[&issuer.audience]);
    v.set_required_spec_claims(&["exp", "iat", "iss", "aud"]);
    v.leeway = cfg.leeway_seconds;
    v.validate_nbf = true;
    let data = jsonwebtoken::decode::<OidcClaims>(token, &key, &v)
        .map_err(|e| Refused::Invalid(e.to_string()))?;
    let age = OffsetDateTime::now_utc().unix_timestamp() - data.claims.iat;
    let max_age = i64::try_from(cfg.max_token_age_seconds.saturating_add(cfg.leeway_seconds))
        .unwrap_or(i64::MAX);
    if age > max_age {
        return Err(Refused::Invalid("the token was issued too long ago".into()));
    }
    Ok((data.claims, issuer.audience.clone()))
}

/// The request.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct TokenRequest {
    /// A CI OIDC ID token. Without it, the bearer API key is exchanged.
    #[serde(default)]
    id_token: Option<String>,
    /// The project to publish to, when a repository's policies grant several.
    #[serde(default)]
    project: Option<String>,
}

/// The minted token.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct TokenResponse {
    /// Bearer secret for `/api/v1/agent/ws` and `/api/v1/agent/batch`.
    pub(crate) token: String,
    /// When it stops working.
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) expires_at: OffsetDateTime,
    /// The project it publishes to.
    pub(crate) project: String,
    /// `trusted` or `pr`.
    pub(crate) trust_level: TrustLevel,
}

fn level(t: Trust) -> TrustLevel {
    match t {
        Trust::Trusted => TrustLevel::Trusted,
        Trust::Pr => TrustLevel::Pr,
    }
}

pub(crate) async fn token(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    body: Option<Json<TokenRequest>>,
) -> Response {
    if let Err(wait) = state.limits.ip.check(ip, Instant::now()) {
        return too_many_requests(wait);
    }
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let ttl =
        time::Duration::minutes(i64::try_from(state.config.oidc.token_ttl_minutes).unwrap_or(360));
    let Some(id_token) = req.id_token else {
        return exchange_api_key(&state, &headers, ttl).await;
    };
    let (claims, audience) = match verify(&state.jwks, &state.config.oidc, &id_token).await {
        Ok(ok) => ok,
        Err(Refused::Unavailable(m)) => {
            tracing::warn!(error = %m, "OIDC issuer unavailable");
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "the issuer's keys are unavailable",
            );
        }
        Err(Refused::Invalid(m)) => {
            tracing::warn!(reason = %m, "OIDC token refused");
            return error(StatusCode::UNAUTHORIZED, m);
        }
    };
    let (policy, trust) = match decide(&state, &claims, &audience, req.project.as_deref()).await {
        Ok(ok) => ok,
        Err(r) => return *r,
    };
    if let Some(jti) = &claims.jti {
        let exp = OffsetDateTime::from_unix_timestamp(claims.exp)
            .unwrap_or_else(|_| OffsetDateTime::now_utc());
        match state.store.claim_jti(&claims.iss, jti, exp).await {
            Ok(true) => {}
            Ok(false) => return error(StatusCode::UNAUTHORIZED, "this token was already used"),
            Err(e) => {
                tracing::error!(error = %e, "recording a jti");
                return error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
            }
        }
    }
    let ci = CiClaims {
        repository: claims.repository.clone(),
        repository_id: claims.repository_id.clone(),
        run_id: claims.run_id.clone(),
        run_attempt: claims.run_attempt.as_deref().and_then(|a| a.parse().ok()),
        check_run_id: claims.check_run_id.clone(),
        sha: claims.sha.clone(),
        git_ref: claims.git_ref.clone(),
        event_name: claims.event_name.clone(),
    };
    let expires_at = OffsetDateTime::now_utc() + ttl;
    match state
        .store
        .mint_agent_token(
            policy.project_id,
            TokenSource::TrustPolicy(policy.policy_id),
            level(trust),
            &ci,
            expires_at,
        )
        .await
    {
        Ok((_, secret)) => {
            tracing::info!(project = %policy.project_slug, repository = ?claims.repository,
                trust = ?trust, "agent token issued for an OIDC identity");
            Json(TokenResponse {
                token: secret.expose().to_owned(),
                expires_at,
                project: policy.project_slug,
                trust_level: level(trust),
            })
            .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "minting an agent token");
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// Which trust policy accepts these verified claims, and at what trust.
async fn decide(
    state: &AppState,
    claims: &OidcClaims,
    audience: &str,
    project: Option<&str>,
) -> Result<(fleet_store::auth::Candidate, Trust), Box<Response>> {
    let fail = |status, message: String| Box::new(error(status, message));
    let (Some(repo_id), Some(owner_id)) = (&claims.repository_id, &claims.repository_owner_id)
    else {
        return Err(fail(
            StatusCode::FORBIDDEN,
            "the token has no repository_id/repository_owner_id".into(),
        ));
    };
    let candidates = state
        .store
        .trust_candidates(&claims.iss, audience, repo_id, owner_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "loading trust policies");
            fail(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
        })?;
    let facts = trust::Claims {
        repository_id: claims.repository_id.clone(),
        repository_owner_id: claims.repository_owner_id.clone(),
        git_ref: claims.git_ref.clone(),
        event_name: claims.event_name.clone(),
        job_workflow_ref: claims.job_workflow_ref.clone(),
        environment: claims.environment.clone(),
    };
    let mut reasons = Vec::new();
    let mut allowed = Vec::new();
    for c in candidates
        .into_iter()
        .filter(|c| project.is_none_or(|p| p == c.project_slug))
    {
        match trust::evaluate(&c.policy, &facts) {
            Decision::Allow(t) => allowed.push((c, t)),
            Decision::Deny(r) => reasons.push(r),
        }
    }
    let mut projects: Vec<&str> = allowed
        .iter()
        .map(|(c, _)| c.project_slug.as_str())
        .collect();
    projects.dedup();
    if projects.len() > 1 {
        return Err(fail(
            StatusCode::CONFLICT,
            format!(
                "several projects accept this repository ({}): pass \"project\"",
                projects.join(", ")
            ),
        ));
    }
    allowed.into_iter().next().ok_or_else(|| {
        tracing::warn!(repository = ?claims.repository, ?reasons, "no trust policy accepts the token");
        let why = if reasons.is_empty() {
            "no policy for this repository".to_owned()
        } else {
            reasons.join("; ")
        };
        fail(
            StatusCode::FORBIDDEN,
            format!("no trust policy accepts this token: {why}"),
        )
    })
}

async fn exchange_api_key(state: &AppState, headers: &HeaderMap, ttl: time::Duration) -> Response {
    let Some(secret) = bearer(headers) else {
        return error(
            StatusCode::UNAUTHORIZED,
            "send id_token, or an API key as the bearer",
        );
    };
    let publisher = match state.store.authenticate(&secret).await {
        Ok(Some(p)) => p,
        Ok(None) => return error(StatusCode::UNAUTHORIZED, "unknown credential"),
        Err(e) => {
            tracing::error!(error = %e, "authenticating an API key");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    let Credential::ApiKey(key) = publisher.credential else {
        return error(StatusCode::BAD_REQUEST, "only an API key can be exchanged");
    };
    let slug = match state.store.projects().await {
        Ok(p) => p
            .into_iter()
            .find(|p| p.id == publisher.project_id)
            .map(|p| p.slug),
        Err(_) => None,
    }
    .unwrap_or_default();
    let expires_at = OffsetDateTime::now_utc() + ttl;
    match state
        .store
        .mint_agent_token(
            publisher.project_id,
            TokenSource::ApiKey(key),
            publisher.trust,
            &CiClaims::default(),
            expires_at,
        )
        .await
    {
        Ok((_, t)) => Json(TokenResponse {
            token: t.expose().to_owned(),
            expires_at,
            project: slug,
            trust_level: publisher.trust,
        })
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "minting an agent token");
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}
