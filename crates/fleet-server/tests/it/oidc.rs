// SPDX-License-Identifier: AGPL-3.0-only
//! The CI OIDC exchange against a fake issuer (discovery + JWKS), with tokens signed by a key
//! generated for each test run (no key material in the repository).

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use aws_lc_rs::{
    rand::SystemRandom,
    signature::{KeyPair as _, RSA_PKCS1_SHA256, RsaKeyPair},
};
use axum::{Json, Router, routing};
use base64::Engine as _;
use fleet_protocol::frame::ServerFrame;
use fleet_server::config::{OidcIssuer, TrustPolicyConfig};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::support::{Agent, Client, Harness, VIEWER, get, post_json};

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A minimal OIDC issuer: discovery, JWKS, and a signer.
struct Issuer {
    url: String,
    key: Arc<RsaKeyPair>,
    fetches: Arc<AtomicU32>,
}

impl Issuer {
    async fn start() -> Self {
        let key = Arc::new(RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let public = key.public_key();
        let jwks = json!({"keys": [{
            "kty": "RSA", "kid": "test-1", "use": "sig", "alg": "RS256",
            "n": b64(public.modulus().big_endian_without_leading_zero()),
            "e": b64(public.exponent().big_endian_without_leading_zero()),
        }]});
        let fetches = Arc::new(AtomicU32::new(0));
        let (u, f) = (url.clone(), Arc::clone(&fetches));
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                routing::get(move || {
                    let u = u.clone();
                    f.fetch_add(1, Ordering::SeqCst);
                    async move { Json(json!({"issuer": u, "jwks_uri": format!("{u}/jwks")})) }
                }),
            )
            .route(
                "/jwks",
                routing::get(move || async move { Json(jwks.clone()) }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, key, fetches }
    }

    /// A signed RS256 token; `claims` override the defaults of a trusted push.
    fn token(&self, claims: &Value) -> String {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let mut c = json!({
            "iss": self.url, "aud": "fleet", "iat": now, "nbf": now - 5, "exp": now + 300,
            "jti": uuid::Uuid::now_v7().to_string(),
            "repository": "acme/app", "repository_id": "123", "repository_owner_id": "9",
            "run_id": "777", "run_attempt": "2", "check_run_id": "4242", "sha": "abc",
            "ref": "refs/heads/main", "event_name": "push",
        });
        for (k, v) in claims.as_object().unwrap() {
            c[k] = v.clone();
        }
        let header = b64(br#"{"alg":"RS256","typ":"JWT","kid":"test-1"}"#);
        let payload = b64(c.to_string().as_bytes());
        let signing_input = format!("{header}.{payload}");
        let mut sig = vec![0u8; self.key.public_modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut sig,
            )
            .unwrap();
        format!("{signing_input}.{}", b64(&sig))
    }
}

async fn harness(issuer: &Issuer) -> Harness {
    let url = issuer.url.clone();
    Harness::start_with(move |c| {
        c.oidc.issuers.push(OidcIssuer {
            issuer: url.clone(),
            audience: "fleet".into(),
        });
        c.projects[0].trust_policies = vec![TrustPolicyConfig {
            name: "acme-app".into(),
            issuer: url,
            audience: "fleet".into(),
            repository_id: "123".into(),
            repository_owner_id: "9".into(),
            trusted_refs: vec!["refs/heads/main".into()],
            allowed_events: vec![],
            pr_policy: "restricted".into(),
            job_workflow_ref: None,
            environment: None,
        }];
    })
    .await
}

async fn exchange(h: &Harness, id_token: &str) -> (u16, Value) {
    let (status, body) = post_json(
        h.addr(),
        "/api/v1/agent/token",
        None,
        &json!({"id_token": id_token}).to_string(),
    )
    .await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_trusted_push_gets_a_token_and_verified_claims_name_the_job() {
    let issuer = Issuer::start().await;
    let h = harness(&issuer).await;
    let (status, body) = exchange(&h, &issuer.token(&json!({}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        (body["project"].as_str(), body["trust_level"].as_str()),
        (Some("app"), Some("trusted"))
    );
    let token = body["token"].as_str().unwrap();
    assert!(token.starts_with("mf_agent_"));

    // The agent claims a different job and run; the verified claims win.
    let mut agent = Agent::new("not-777", "agent-says-e2e");
    let mut c = Client::connect(h.addr(), Some(token), true).await;
    assert!(matches!(
        c.hello(&agent, Some(1)).await,
        ServerFrame::Welcome(_)
    ));
    let e = agent.suite(&[("login", true)]);
    c.events(e).await;
    let (_, attempts) = get(h.addr(), "/api/v1/projects/app/attempts", Some(VIEWER)).await;
    let attempts: Value = serde_json::from_str(&attempts).unwrap();
    assert_eq!(attempts[0]["run_key"], "acme/app:777");
    assert_eq!(attempts[0]["attempt"], 2);
    let id = attempts[0]["id"].as_str().unwrap();
    let (_, snap) = get(
        h.addr(),
        &format!("/api/v1/projects/app/attempts/{id}"),
        Some(VIEWER),
    )
    .await;
    let snap: Value = serde_json::from_str(&snap).unwrap();
    assert_eq!(
        snap["jobs"][0]["job_key"], "4242",
        "check_run_id, not the agent's job key"
    );
    assert_eq!(snap["agent_runs"][0]["trust_level"], "trusted");
}

#[tokio::test]
async fn pull_requests_are_pr_trust_and_dangerous_events_are_refused() {
    let issuer = Issuer::start().await;
    let h = harness(&issuer).await;
    let (status, body) = exchange(
        &h,
        &issuer.token(&json!({"event_name": "pull_request", "ref": "refs/pull/5/merge"})),
    )
    .await;
    assert_eq!((status, body["trust_level"].as_str()), (200, Some("pr")));
    let (status, _) = exchange(&h, &issuer.token(&json!({"ref": "refs/heads/feature"}))).await;
    assert_eq!(status, 200, "an untrusted ref still publishes, at pr trust");
    for bad in [
        json!({"event_name": "pull_request_target"}),
        json!({"event_name": "dynamic"}),
        json!({"repository_id": "124"}),
        json!({"repository_owner_id": "10"}),
    ] {
        let (status, body) = exchange(&h, &issuer.token(&bad)).await;
        assert_eq!(status, 403, "{bad} → {body}");
    }
}

#[tokio::test]
async fn forged_expired_replayed_and_foreign_tokens_are_refused() {
    let issuer = Issuer::start().await;
    let h = harness(&issuer).await;
    let ok = issuer.token(&json!({}));
    assert_eq!(exchange(&h, &ok).await.0, 200);
    assert_eq!(exchange(&h, &ok).await.0, 401, "a jti is accepted once");
    let now = OffsetDateTime::now_utc().unix_timestamp();
    for bad in [
        json!({"aud": "someone-else"}),
        json!({"exp": now - 3600, "iat": now - 4000, "nbf": now - 4000}),
        json!({"iat": now - 3600}),
    ] {
        assert_eq!(exchange(&h, &issuer.token(&bad)).await.0, 401, "{bad}");
    }
    // A signature from another key.
    let other = Issuer::start().await;
    let forged = other.token(&json!({"iss": issuer.url}));
    assert_eq!(exchange(&h, &forged).await.0, 401);
    // An issuer nobody configured is never even contacted.
    let stranger = Issuer::start().await;
    assert_eq!(exchange(&h, &stranger.token(&json!({}))).await.0, 401);
    assert_eq!(
        stranger.fetches.load(Ordering::SeqCst),
        0,
        "no fetch from an unlisted iss"
    );
    // Tampered payload.
    let mut parts: Vec<String> = ok.split('.').map(str::to_owned).collect();
    parts[1] = b64(json!({"iss": issuer.url, "repository_id": "123"})
        .to_string()
        .as_bytes());
    assert_eq!(exchange(&h, &parts.join(".")).await.0, 401);
}

#[tokio::test]
async fn an_api_key_is_exchanged_for_an_agent_token() {
    let issuer = Issuer::start().await;
    let h = harness(&issuer).await;
    let (status, body) = post_json(h.addr(), "/api/v1/agent/token", Some(&h.key), "{}").await;
    assert_eq!(status, 200, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    let token = body["token"].as_str().unwrap();
    let agent = Agent::new("1", "j");
    let mut c = Client::connect(h.addr(), Some(token), true).await;
    assert!(matches!(
        c.hello(&agent, None).await,
        ServerFrame::Welcome(_)
    ));
    assert_eq!(
        post_json(h.addr(), "/api/v1/agent/token", Some("mf_live_nope"), "{}")
            .await
            .0,
        401
    );
}
