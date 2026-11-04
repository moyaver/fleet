// SPDX-License-Identifier: AGPL-3.0-only
//! The identity provider as the login flow sees it: discovery, the authorization-code exchange
//! and ID token verification.
//!
//! Discovery is cached for an hour. The provider's `issuer` must equal the configured one (RFC
//! 8414 §3.3), and redirects are never followed, so a hostile discovery document or token
//! endpoint can't bounce the server around (SSRF hygiene). ID tokens are verified like the CI
//! tokens of `oidc.rs`: RS256 only, keys from the issuer's JWKS, `iss`, `aud` and `exp` checked.

use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, Validation};
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::{
    config::Login,
    oidc::{Jwks, Refused},
};

const DISCOVERY_TTL: Duration = Duration::from_secs(3600);

/// The provider endpoints Fleet uses.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Endpoints {
    issuer: String,
    pub(crate) authorization_endpoint: String,
    token_endpoint: String,
}

/// The ID token's claims that sign-in reads.
#[derive(Debug, Deserialize)]
pub(crate) struct IdClaims {
    pub(crate) sub: String,
    #[serde(default)]
    pub(crate) nonce: Option<String>,
    #[serde(default)]
    pub(crate) email: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) preferred_username: Option<String>,
    #[serde(flatten)]
    other: Map<String, Value>,
}

impl IdClaims {
    /// The groups in the first-class claims named in `claim_names` (a string or a list of
    /// strings each); `None` when none of those claims is present at all.
    pub(crate) fn groups(&self, claim_names: &[String]) -> Option<Vec<String>> {
        let mut found = None;
        for name in claim_names {
            let Some(value) = self.other.get(name) else {
                continue;
            };
            let groups: &mut Vec<String> = found.get_or_insert_with(Vec::new);
            match value {
                Value::String(g) => groups.push(g.clone()),
                Value::Array(items) => {
                    groups.extend(items.iter().filter_map(Value::as_str).map(str::to_owned));
                }
                _ => {}
            }
        }
        found
    }

    /// Entra ID leaves `groups` out of tokens of users in too many groups and points at
    /// Microsoft Graph instead (`_claim_names`).
    pub(crate) fn groups_overage(&self) -> bool {
        self.other
            .get("_claim_names")
            .and_then(|n| n.get("groups"))
            .is_some()
    }
}

/// Talks to the configured provider.
#[derive(Debug)]
pub(crate) struct Idp {
    client: reqwest::Client,
    discovery: Mutex<Option<(Instant, Endpoints)>>,
}

impl Idp {
    pub(crate) fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            discovery: Mutex::new(None),
        })
    }

    /// The provider's endpoints from its discovery document.
    pub(crate) async fn endpoints(&self, issuer: &str) -> Result<Endpoints, Refused> {
        let mut cached = self.discovery.lock().await;
        if let Some((at, endpoints)) = cached.as_ref()
            && at.elapsed() < DISCOVERY_TTL
        {
            return Ok(endpoints.clone());
        }
        let unavailable = |e: reqwest::Error| Refused::Unavailable(e.to_string());
        let endpoints: Endpoints = self
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
        if endpoints.issuer != issuer {
            return Err(Refused::Invalid(format!(
                "the provider says it is {}, not {issuer}",
                endpoints.issuer
            )));
        }
        *cached = Some((Instant::now(), endpoints.clone()));
        Ok(endpoints)
    }
}

/// RFC 6749 §2.3.1: the client ID and secret are form-encoded before Basic auth.
fn form_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

impl Idp {
    /// Exchanges an authorization code (with its PKCE verifier) for the ID token.
    pub(crate) async fn exchange(
        &self,
        login: &Login,
        endpoints: &Endpoints,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<String, Refused> {
        #[derive(Deserialize)]
        struct Tokens {
            id_token: String,
        }
        let body = {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.append_pair("grant_type", "authorization_code")
                .append_pair("code", code)
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("code_verifier", verifier);
            if login.client_secret.is_none() {
                form.append_pair("client_id", &login.client_id);
            }
            form.finish()
        };
        let mut request = self.client.post(&endpoints.token_endpoint);
        if let Some(secret) = &login.client_secret {
            request = request.basic_auth(
                form_encode(&login.client_id),
                Some(form_encode(secret.expose())),
            );
        }
        let response = request
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|e| Refused::Unavailable(e.to_string()))?;
        if !response.status().is_success() {
            return Err(Refused::Invalid(format!(
                "the provider refused the code ({})",
                response.status()
            )));
        }
        let tokens: Tokens = response
            .json()
            .await
            .map_err(|e| Refused::Invalid(format!("the provider's token response: {e}")))?;
        Ok(tokens.id_token)
    }

    /// Verifies an ID token's signature, issuer, audience and expiry.
    pub(crate) async fn verify(
        &self,
        jwks: &Jwks,
        login: &Login,
        leeway_seconds: u64,
        id_token: &str,
    ) -> Result<IdClaims, Refused> {
        let invalid = |e: jsonwebtoken::errors::Error| Refused::Invalid(e.to_string());
        let header = jsonwebtoken::decode_header(id_token).map_err(invalid)?;
        if header.alg != Algorithm::RS256 {
            return Err(Refused::Invalid("only RS256 is accepted".into()));
        }
        let kid = header
            .kid
            .ok_or_else(|| Refused::Invalid("the token has no kid".into()))?;
        let key = jwks.key(&login.issuer, &kid).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&login.issuer]);
        validation.set_audience(&[&login.client_id]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.leeway = leeway_seconds;
        validation.validate_nbf = true;
        jsonwebtoken::decode::<IdClaims>(id_token, &key, &validation)
            .map(|data| data.claims)
            .map_err(invalid)
    }
}
