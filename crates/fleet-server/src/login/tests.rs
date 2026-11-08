// SPDX-License-Identifier: AGPL-3.0-only
//! Browser sign-in over HTTP (seam S2): the real router and a fresh Postgres database, driven
//! in-process with axum-test, against a fake OIDC provider (wiremock) that signs ID tokens with
//! a key generated for each test run.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use aws_lc_rs::{
    rand::SystemRandom,
    signature::{KeyPair as _, RSA_PKCS1_SHA256, RsaKeyPair},
};
use axum_test::TestServer;
use base64::Engine as _;
use fleet_core::policy::Role;
use serde_json::{Value, json};
use time::OffsetDateTime;
use url::Url;
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path},
};

use crate::{
    app,
    config::{Config, GroupRole, Login, ProjectConfig, Secret},
};

const PUBLIC: &str = "https://fleet.test";
const CALLBACK: &str = "https://fleet.test/api/v1/auth/callback";
const SESSION_COOKIE: &str = "__Host-fleet_session";

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A fake identity provider: discovery, JWKS, and a token endpoint that hands out the ID token
/// a test registered for an authorization code.
struct Idp {
    server: MockServer,
    key: RsaKeyPair,
    codes: Arc<Mutex<HashMap<String, String>>>,
}

struct TokenEndpoint(Arc<Mutex<HashMap<String, String>>>);

impl Respond for TokenEndpoint {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let form: HashMap<String, String> = url::form_urlencoded::parse(&request.body)
            .into_owned()
            .collect();
        let token = form
            .get("code")
            .and_then(|code| self.0.lock().unwrap().get(code).cloned());
        match token {
            Some(id_token) => ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "at", "token_type": "Bearer", "expires_in": 60,
                "id_token": id_token,
            })),
            None => ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})),
        }
    }
}

impl Idp {
    async fn start() -> Self {
        let server = MockServer::start().await;
        let key = RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        let uri = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": uri,
                "authorization_endpoint": format!("{uri}/authorize"),
                "token_endpoint": format!("{uri}/token"),
                "jwks_uri": format!("{uri}/jwks"),
            })))
            .mount(&server)
            .await;
        let public = key.public_key();
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"keys": [{
                "kty": "RSA", "kid": "idp-1", "use": "sig", "alg": "RS256",
                "n": b64(public.modulus().big_endian_without_leading_zero()),
                "e": b64(public.exponent().big_endian_without_leading_zero()),
            }]})))
            .mount(&server)
            .await;
        let codes = Arc::new(Mutex::new(HashMap::new()));
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(TokenEndpoint(Arc::clone(&codes)))
            .mount(&server)
            .await;
        Self { server, key, codes }
    }

    fn url(&self) -> String {
        self.server.uri()
    }

    /// Registers the ID token the token endpoint returns for `code`.
    fn issue(&self, code: &str, id_token: String) {
        self.codes.lock().unwrap().insert(code.into(), id_token);
    }

    /// A valid ID token for the test client; `extra` adds or overrides claims.
    fn id_token(&self, nonce: &str, extra: &Value) -> String {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let mut claims = json!({
            "iss": self.url(), "aud": "fleet-web", "sub": "user-1",
            "iat": now, "exp": now + 300, "nonce": nonce,
            "email": "ada@example.com", "name": "Ada Lovelace",
        });
        for (k, v) in extra.as_object().unwrap() {
            claims[k] = v.clone();
        }
        self.sign(&claims)
    }

    /// The form fields of every request the token endpoint received.
    async fn token_requests(&self) -> Vec<(HashMap<String, String>, Option<String>)> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == "/token")
            .map(|r| {
                let form = url::form_urlencoded::parse(&r.body).into_owned().collect();
                let auth = r
                    .headers
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_owned());
                (form, auth)
            })
            .collect()
    }

    fn sign(&self, claims: &Value) -> String {
        let header = b64(br#"{"alg":"RS256","typ":"JWT","kid":"idp-1"}"#);
        let input = format!("{header}.{}", b64(claims.to_string().as_bytes()));
        let mut sig = vec![0u8; self.key.public_modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                input.as_bytes(),
                &mut sig,
            )
            .unwrap();
        format!("{input}.{}", b64(&sig))
    }
}

/// The application under test and its fake provider.
struct Rig {
    app: TestServer,
    store: fleet_store::Store,
    idp: Idp,
}

async fn rig() -> Rig {
    rig_with(|_| {}).await
}

async fn rig_with(tweak: impl FnOnce(&mut Config)) -> Rig {
    let idp = Idp::start().await;
    let (store, url) = fleet_store::testing::fresh_database().await.unwrap();
    let mut config = Config::default();
    config.database.url = Some(Secret::from(url.as_str()));
    config.api.public_url = Some(PUBLIC.into());
    config.projects = ["app", "other"]
        .map(|slug| ProjectConfig {
            slug: slug.into(),
            name: slug.into(),
            trust_policies: Vec::new(),
        })
        .into();
    let role = |group: &str, role, project: Option<&str>| GroupRole {
        group: group.into(),
        role,
        project: project.map(Into::into),
    };
    config.login = Some(Login {
        issuer: idp.url(),
        client_id: "fleet-web".into(),
        client_secret: Some(Secret::from("shh")),
        scopes: vec!["profile".into(), "email".into()],
        group_claims: vec!["groups".into(), "roles".into()],
        session_ttl_minutes: 60,
        group_roles: vec![
            role("qa", Role::Viewer, Some("app")),
            role("leads", Role::Maintainer, Some("app")),
            role("other-readers", Role::Viewer, Some("other")),
            role("ops", Role::Admin, None),
        ],
    });
    tweak(&mut config);
    let state = app::init(config).await.unwrap();
    // A real socket, so WebSocket upgrades work.
    let app = TestServer::builder()
        .http_transport()
        .build(app::router(&state).into_make_service_with_connect_info::<SocketAddr>());
    Rig { app, store, idp }
}

fn set_cookie(res: &axum_test::TestResponse) -> String {
    res.header("set-cookie").to_str().unwrap().to_owned()
}

#[tokio::test]
async fn login_redirects_to_the_provider_with_pkce_state_and_nonce() {
    let rig = rig().await;

    let res = rig.app.get("/api/v1/auth/login").await;

    assert_eq!(res.status_code(), 303);
    let location = Url::parse(res.header("location").to_str().unwrap()).unwrap();
    assert_eq!(
        format!(
            "{}{}",
            location.origin().ascii_serialization(),
            location.path()
        ),
        format!("{}/authorize", rig.idp.url())
    );
    let q: HashMap<String, String> = location.query_pairs().into_owned().collect();
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], "fleet-web");
    assert_eq!(q["redirect_uri"], CALLBACK);
    let scopes: Vec<&str> = q["scope"].split(' ').collect();
    assert_eq!(scopes, ["openid", "profile", "email"]);
    assert_eq!(q["code_challenge_method"], "S256");
    // RFC 7636: the challenge is base64url(SHA-256(verifier)), 43 characters.
    assert_eq!(q["code_challenge"].len(), 43);
    assert!(q["state"].len() >= 32 && q["nonce"].len() >= 32);
    assert_ne!(q["state"], q["nonce"]);

    // The transaction rides in an unguessable, HttpOnly, Secure, SameSite=Lax cookie.
    let cookie = set_cookie(&res);
    assert!(
        cookie.starts_with(&format!("{SESSION_COOKIE}=")),
        "{cookie}"
    );
    for attribute in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
        assert!(
            cookie.split("; ").any(|a| a == attribute),
            "{attribute} in {cookie}"
        );
    }
    assert!(
        !cookie.contains("Domain"),
        "a __Host- cookie has no Domain: {cookie}"
    );

    // Every login starts a fresh transaction.
    let again = rig.app.get("/api/v1/auth/login").await;
    let again = Url::parse(again.header("location").to_str().unwrap()).unwrap();
    let again: HashMap<String, String> = again.query_pairs().into_owned().collect();
    assert_ne!(again["state"], q["state"]);
    assert_ne!(again["nonce"], q["nonce"]);
}

/// One sign-in attempt in flight: what the provider was sent and the cookie that holds it.
struct Flow {
    state: String,
    nonce: String,
    challenge: String,
    cookie: String,
}

/// `name=value` of the response's `Set-Cookie`, as a browser would send it back.
fn cookie_pair(res: &axum_test::TestResponse) -> String {
    set_cookie(res).split(';').next().unwrap().to_owned()
}

async fn start_login(rig: &Rig, return_to: Option<&str>) -> Flow {
    let path = match return_to {
        Some(to) => format!("/api/v1/auth/login?return_to={}", urlencode(to)),
        None => "/api/v1/auth/login".into(),
    };
    let res = rig.app.get(&path).await;
    let location = Url::parse(res.header("location").to_str().unwrap()).unwrap();
    let q: HashMap<String, String> = location.query_pairs().into_owned().collect();
    Flow {
        state: q["state"].clone(),
        nonce: q["nonce"].clone(),
        challenge: q["code_challenge"].clone(),
        cookie: cookie_pair(&res),
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// The browser returning from the provider with `code`.
async fn callback(rig: &Rig, flow: &Flow, code: &str) -> axum_test::TestResponse {
    callback_with_state(rig, flow, code, &flow.state).await
}

async fn callback_with_state(
    rig: &Rig,
    flow: &Flow,
    code: &str,
    state: &str,
) -> axum_test::TestResponse {
    rig.app
        .get(&format!(
            "/api/v1/auth/callback?code={}&state={}",
            urlencode(code),
            urlencode(state)
        ))
        .add_header("cookie", &flow.cookie)
        .await
}

/// A whole successful sign-in with these ID token claims; returns the session cookie pair.
async fn sign_in(rig: &Rig, claims: &Value) -> String {
    let flow = start_login(rig, None).await;
    rig.idp.issue("code", rig.idp.id_token(&flow.nonce, claims));
    let res = callback(rig, &flow, "code").await;
    assert_eq!(res.status_code(), 303, "{}", res.text());
    cookie_pair(&res)
}

#[tokio::test]
async fn a_member_of_a_mapped_group_signs_in_and_gets_a_session() {
    let rig = rig().await;
    let flow = start_login(&rig, None).await;
    rig.idp.issue(
        "code-1",
        rig.idp
            .id_token(&flow.nonce, &json!({"groups": ["qa", "unrelated"]})),
    );

    let res = callback(&rig, &flow, "code-1").await;

    assert_eq!(res.status_code(), 303);
    assert_eq!(res.header("location"), "/");
    // A new session id replaces the sign-in transaction's (no session fixation).
    let session = set_cookie(&res);
    assert!(
        session.starts_with(&format!("{SESSION_COOKIE}=")),
        "{session}"
    );
    assert_ne!(cookie_pair(&res), flow.cookie);
    for attribute in [
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Path=/",
        "Max-Age=3600",
    ] {
        assert!(
            session.split("; ").any(|a| a == attribute),
            "{attribute} in {session}"
        );
    }

    // The provider got the PKCE verifier for the challenge it was sent, and the client secret.
    let requests = rig.idp.token_requests().await;
    assert_eq!(requests.len(), 1);
    let (form, authorization) = &requests[0];
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "code-1");
    assert_eq!(form["redirect_uri"], CALLBACK);
    let digest =
        aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, form["code_verifier"].as_bytes());
    assert_eq!(b64(digest.as_ref()), flow.challenge);
    assert_eq!(
        authorization.as_deref(),
        Some(&*format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("fleet-web:shh")
        ))
    );

    // The session says who signed in, what they may do, and carries the CSRF token.
    let me = rig
        .app
        .get("/api/v1/auth/session")
        .add_header("cookie", cookie_pair(&res))
        .await;
    assert_eq!(me.status_code(), 200);
    let me: Value = me.json();
    assert_eq!(me["user"]["email"], "ada@example.com");
    assert_eq!(me["user"]["name"], "Ada Lovelace");
    assert_eq!(me["roles"], json!([{"project": "app", "role": "viewer"}]));
    assert!(me["csrf_token"].as_str().unwrap().len() >= 32);
}

fn assert_signed_out(res: &axum_test::TestResponse, status: u16) {
    assert_eq!(res.status_code(), status, "{}", res.text());
    assert!(
        res.maybe_header("set-cookie").is_none(),
        "no session may start: {:?}",
        res.maybe_header("set-cookie")
    );
}

#[tokio::test]
async fn a_callback_with_a_forged_state_is_refused_and_spends_the_attempt() {
    let rig = rig().await;
    let flow = start_login(&rig, None).await;
    rig.idp.issue(
        "code",
        rig.idp.id_token(&flow.nonce, &json!({"groups": ["qa"]})),
    );

    let forged = callback_with_state(&rig, &flow, "code", "attacker-chosen").await;
    assert_signed_out(&forged, 400);

    // The attacker's attempt consumed the transaction: the honest callback can't ride on it.
    let honest = callback(&rig, &flow, "code").await;
    assert_signed_out(&honest, 400);
}

#[tokio::test]
async fn a_callback_with_a_signed_in_cookie_does_not_end_the_session() {
    let rig = rig().await;
    let cookie = sign_in(&rig, &json!({"groups": ["qa"]})).await;

    let response = rig
        .app
        .get("/api/v1/auth/callback?code=code&state=attacker-chosen")
        .add_header("cookie", &cookie)
        .await;
    assert_signed_out(&response, 400);

    let session = rig
        .app
        .get("/api/v1/auth/session")
        .add_header("cookie", &cookie)
        .await;
    assert_eq!(session.status_code(), 200);
    let session: Value = session.json();
    assert_eq!(session["user"]["email"], "ada@example.com");
}

#[tokio::test]
async fn a_callback_cannot_be_replayed() {
    let rig = rig().await;
    let flow = start_login(&rig, None).await;
    rig.idp.issue(
        "code",
        rig.idp.id_token(&flow.nonce, &json!({"groups": ["qa"]})),
    );
    assert_eq!(callback(&rig, &flow, "code").await.status_code(), 303);

    assert_signed_out(&callback(&rig, &flow, "code").await, 400);
}

#[tokio::test]
async fn a_callback_without_the_sign_in_cookie_is_refused() {
    let rig = rig().await;
    let flow = start_login(&rig, None).await;
    rig.idp.issue(
        "code",
        rig.idp.id_token(&flow.nonce, &json!({"groups": ["qa"]})),
    );

    let res = rig
        .app
        .get(&format!(
            "/api/v1/auth/callback?code=code&state={}",
            flow.state
        ))
        .await;

    assert_signed_out(&res, 400);
}

/// Builds an ID token for a flow.
type Forge<'a> = Box<dyn Fn(&Flow) -> String + 'a>;

#[tokio::test]
async fn id_tokens_that_fail_verification_are_refused() {
    let rig = rig().await;
    let foreign_key = Idp::start().await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let groups = json!(["qa"]);
    let cases: Vec<(&str, Forge<'_>)> = vec![
        (
            "a nonce from another login",
            Box::new(|_| {
                rig.idp
                    .id_token("someone-elses", &json!({"groups": groups}))
            }),
        ),
        (
            "no nonce at all",
            Box::new(|_| {
                rig.idp
                    .id_token("", &json!({"groups": groups, "nonce": null}))
            }),
        ),
        (
            "another audience",
            Box::new(|f| {
                rig.idp
                    .id_token(&f.nonce, &json!({"groups": groups, "aud": "other-app"}))
            }),
        ),
        (
            "another issuer",
            Box::new(|f| {
                rig.idp.id_token(
                    &f.nonce,
                    &json!({"groups": groups, "iss": "https://evil.example"}),
                )
            }),
        ),
        (
            "expired",
            Box::new(|f| {
                rig.idp
                    .id_token(&f.nonce, &json!({"groups": groups, "exp": now - 3600}))
            }),
        ),
        (
            "another signing key",
            Box::new(|f| {
                foreign_key.id_token(&f.nonce, &json!({"groups": groups, "iss": rig.idp.url()}))
            }),
        ),
    ];
    for (name, make) in cases {
        let flow = start_login(&rig, None).await;
        rig.idp.issue("code", make(&flow));
        let res = callback(&rig, &flow, "code").await;
        assert_eq!(res.status_code(), 401, "{name}: {}", res.text());
        assert!(res.maybe_header("set-cookie").is_none(), "{name}");
    }
}

#[tokio::test]
async fn a_code_the_provider_refuses_is_refused() {
    let rig = rig().await;
    let flow = start_login(&rig, None).await;

    let res = callback(&rig, &flow, "never-issued").await;

    assert_signed_out(&res, 401);
}

async fn try_sign_in(rig: &Rig, claims: &Value) -> axum_test::TestResponse {
    let flow = start_login(rig, None).await;
    rig.idp.issue("code", rig.idp.id_token(&flow.nonce, claims));
    callback(rig, &flow, "code").await
}

async fn roles_of(rig: &Rig, cookie: &str) -> Value {
    let me = rig
        .app
        .get("/api/v1/auth/session")
        .add_header("cookie", cookie)
        .await;
    me.json::<Value>()["roles"].clone()
}

#[tokio::test]
async fn without_a_group_that_maps_to_a_role_there_is_no_access() {
    let rig = rig().await;
    let cases = [
        ("no group claim at all", json!({})),
        ("an empty group list", json!({"groups": []})),
        (
            "groups that grant nothing",
            json!({"groups": ["marketing"]}),
        ),
        ("a claim that is not a list of groups", json!({"groups": 7})),
        // Entra ID drops `groups` for users in too many groups and sends this instead.
        (
            "Entra's groups overage",
            json!({"_claim_names": {"groups": "src1"}, "_claim_sources": {"src1": {}}}),
        ),
    ];
    for (name, claims) in cases {
        let res = try_sign_in(&rig, &claims).await;
        assert_eq!(res.status_code(), 403, "{name}: {}", res.text());
        assert!(res.maybe_header("set-cookie").is_none(), "{name}");
    }
}

#[tokio::test]
async fn entra_app_roles_map_like_groups_and_a_global_admin_group_reaches_every_project() {
    let rig = rig().await;

    let lead = sign_in(&rig, &json!({"roles": ["leads"]})).await;
    assert_eq!(
        roles_of(&rig, &lead).await,
        json!([{"project": "app", "role": "maintainer"}])
    );

    let both = sign_in(
        &rig,
        &json!({"groups": ["qa", "ops"], "roles": ["other-readers"]}),
    )
    .await;
    let roles = roles_of(&rig, &both).await;
    let roles = roles.as_array().unwrap();
    assert_eq!(roles.len(), 3, "{roles:?}");
    for expected in [
        json!({"project": "app", "role": "viewer"}),
        json!({"project": "other", "role": "viewer"}),
        json!({"project": null, "role": "admin"}),
    ] {
        assert!(roles.contains(&expected), "{expected} in {roles:?}");
    }
}

#[tokio::test]
async fn sign_in_returns_to_a_page_on_this_site_and_never_elsewhere() {
    let rig = rig().await;
    for (asked, lands) in [
        (
            "/p/app/pipelines/42?tab=flows#top",
            "/p/app/pipelines/42?tab=flows#top",
        ),
        ("//evil.example/x", "/"),
        ("https://evil.example/x", "/"),
        ("/\\evil.example", "/"),
        ("javascript:alert(1)", "/"),
        ("", "/"),
    ] {
        let flow = start_login(&rig, Some(asked)).await;
        rig.idp.issue(
            "code",
            rig.idp.id_token(&flow.nonce, &json!({"groups": ["qa"]})),
        );
        let res = callback(&rig, &flow, "code").await;
        assert_eq!(res.header("location"), lands, "asked for {asked:?}");
    }
}

/// A browser sent to sign in from a flow's page (`/api/v1/flows/{id}/page`) lands back on that page
/// once the provider has signed it in.
#[tokio::test]
async fn sign_in_from_a_flow_page_lands_back_on_that_page() {
    let rig = rig().await;
    let page = "/api/v1/flows/0196f2c0-0000-7000-8000-00000000000a/page";
    let flow = start_login(&rig, Some(page)).await;
    rig.idp.issue(
        "code",
        rig.idp.id_token(&flow.nonce, &json!({"groups": ["qa"]})),
    );
    let res = callback(&rig, &flow, "code").await;
    assert_eq!(res.header("location"), page);
}

async fn get_as(rig: &Rig, path: &str, cookie: &str) -> axum_test::TestResponse {
    rig.app.get(path).add_header("cookie", cookie).await
}

#[tokio::test]
async fn a_session_reads_only_the_projects_its_roles_cover() {
    let rig = rig().await;
    let qa = sign_in(&rig, &json!({"groups": ["qa"]})).await;
    let ops = sign_in(&rig, &json!({"groups": ["ops"]})).await;
    let slugs = |res: axum_test::TestResponse| -> Vec<String> {
        let mut slugs: Vec<String> = res
            .json::<Vec<Value>>()
            .iter()
            .map(|p| p["slug"].as_str().unwrap().to_owned())
            .collect();
        slugs.sort();
        slugs
    };

    assert_eq!(slugs(get_as(&rig, "/api/v1/projects", &qa).await), ["app"]);
    assert_eq!(
        slugs(get_as(&rig, "/api/v1/projects", &ops).await),
        ["app", "other"]
    );

    assert_eq!(
        get_as(&rig, "/api/v1/projects/app/attempts", &qa)
            .await
            .status_code(),
        200
    );
    // Another team's project looks exactly like a project that doesn't exist.
    let hidden = get_as(&rig, "/api/v1/projects/other/attempts", &qa).await;
    let missing = get_as(&rig, "/api/v1/projects/nope/attempts", &qa).await;
    assert_eq!(
        (hidden.status_code(), hidden.text()),
        (missing.status_code(), missing.text())
    );
    assert_eq!(hidden.status_code(), 404);
    let attempt = uuid::Uuid::now_v7();
    let hidden = get_as(
        &rig,
        &format!("/api/v1/projects/other/attempts/{attempt}"),
        &qa,
    )
    .await;
    assert_eq!(hidden.status_code(), 404);
    assert_eq!(
        get_as(&rig, "/api/v1/projects/other/attempts", &ops)
            .await
            .status_code(),
        200
    );
}

#[tokio::test]
async fn requests_without_a_live_session_are_unauthorized() {
    let rig = rig().await;
    assert_eq!(rig.app.get("/api/v1/projects").await.status_code(), 401);
    let garbage = format!("{SESSION_COOKIE}=not-a-session");
    assert_eq!(
        get_as(&rig, "/api/v1/projects", &garbage)
            .await
            .status_code(),
        401
    );
    assert_eq!(
        get_as(&rig, "/api/v1/auth/session", &garbage)
            .await
            .status_code(),
        401
    );
    // A half-finished sign-in is not a session.
    let flow = start_login(&rig, None).await;
    assert_eq!(
        get_as(&rig, "/api/v1/projects", &flow.cookie)
            .await
            .status_code(),
        401
    );
}

#[tokio::test]
async fn a_session_stops_working_when_it_expires() {
    let rig = rig().await;
    let cookie = sign_in(&rig, &json!({"groups": ["qa"]})).await;
    let record = rig
        .store
        .session(cookie.split_once('=').unwrap().1)
        .await
        .unwrap()
        .unwrap();
    let expired = rig
        .store
        .create_session(
            record.user_id,
            &record.data,
            OffsetDateTime::now_utc() - time::Duration::hours(1),
        )
        .await
        .unwrap();
    let expired_cookie = format!("{SESSION_COOKIE}={}", expired.expose());

    assert_eq!(
        get_as(&rig, "/api/v1/projects", &cookie)
            .await
            .status_code(),
        200
    );
    assert_eq!(
        get_as(&rig, "/api/v1/projects", &expired_cookie)
            .await
            .status_code(),
        401
    );
}

#[tokio::test]
async fn logout_needs_the_csrf_token_and_the_sites_own_origin_and_ends_the_session() {
    let rig = rig().await;
    let cookie = sign_in(&rig, &json!({"groups": ["qa"]})).await;
    let csrf = get_as(&rig, "/api/v1/auth/session", &cookie)
        .await
        .json::<Value>()["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let logout = |token: Option<&str>, origin: Option<&str>| {
        let mut req = rig
            .app
            .post("/api/v1/auth/logout")
            .add_header("cookie", &cookie);
        if let Some(t) = token {
            req = req.add_header("x-csrf-token", t);
        }
        if let Some(o) = origin {
            req = req.add_header("origin", o);
        }
        req
    };

    for (what, token, origin) in [
        ("no token", None, Some(PUBLIC)),
        ("a wrong token", Some("guess"), Some(PUBLIC)),
        (
            "another site's origin",
            Some(csrf.as_str()),
            Some("https://evil.example"),
        ),
    ] {
        assert_eq!(logout(token, origin).await.status_code(), 403, "{what}");
        assert_eq!(
            get_as(&rig, "/api/v1/projects", &cookie)
                .await
                .status_code(),
            200,
            "{what}"
        );
    }

    let res = logout(Some(&csrf), Some(PUBLIC)).await;
    assert_eq!(res.status_code(), 204);
    let cleared = set_cookie(&res);
    assert!(
        cleared.starts_with(&format!("{SESSION_COOKIE}=;")),
        "{cleared}"
    );
    assert!(cleared.split("; ").any(|a| a == "Max-Age=0"), "{cleared}");
    assert_eq!(
        get_as(&rig, "/api/v1/projects", &cookie)
            .await
            .status_code(),
        401
    );
}

#[tokio::test]
async fn logout_without_a_session_is_refused() {
    let rig = rig().await;
    assert_eq!(
        rig.app
            .post("/api/v1/auth/logout")
            .add_header("x-csrf-token", "x")
            .await
            .status_code(),
        401
    );
}

#[tokio::test]
async fn the_live_socket_follows_the_sessions_roles_and_refuses_foreign_origins() {
    let rig = rig().await;
    let qa = sign_in(&rig, &json!({"groups": ["qa"]})).await;
    let socket = |cookie: Option<&str>, origin: &str| {
        let mut req = rig
            .app
            .get_websocket("/api/v1/live")
            .add_header("origin", origin);
        if let Some(c) = cookie {
            req = req.add_header("cookie", c);
        }
        req
    };

    assert_eq!(socket(None, PUBLIC).await.status_code(), 401);
    // A page on another site can't ride the browser's cookie (cross-site WebSocket hijacking).
    assert_eq!(
        socket(Some(&qa), "https://evil.example")
            .await
            .status_code(),
        403
    );

    let mut live = socket(Some(&qa), PUBLIC).await.into_websocket().await;
    live.send_json(&json!({"op": "subscribe", "project": "app"}))
        .await;
    let frame: Value = live.receive_json().await;
    assert_eq!(frame["op"], "subscribed", "{frame}");

    // Another team's project answers like one that doesn't exist.
    let mut live = socket(Some(&qa), PUBLIC).await.into_websocket().await;
    live.send_json(&json!({"op": "subscribe", "project": "other"}))
        .await;
    let hidden: Value = live.receive_json().await;
    let mut live = socket(Some(&qa), PUBLIC).await.into_websocket().await;
    live.send_json(&json!({"op": "subscribe", "project": "nope"}))
        .await;
    let missing: Value = live.receive_json().await;
    assert_eq!(hidden["op"], "error");
    assert_eq!(
        hidden["message"].as_str().unwrap().replace("other", "nope"),
        missing["message"]
    );
}

#[tokio::test]
async fn a_live_socket_closes_when_its_sign_in_ends() {
    let rig = rig_with(|c| c.api.session_recheck_ms = 50).await;
    let qa = sign_in(&rig, &json!({"groups": ["qa"]})).await;
    let mut live = rig
        .app
        .get_websocket("/api/v1/live")
        .add_header("origin", PUBLIC)
        .add_header("cookie", &qa)
        .await
        .into_websocket()
        .await;
    live.send_json(&json!({"op": "subscribe", "project": "app"}))
        .await;
    let frame: Value = live.receive_json().await;
    assert_eq!(frame["op"], "subscribed", "{frame}");

    // Logout (or expiry) removes the session row; the open socket must not outlive it.
    let cookie = qa.split_once('=').unwrap().1;
    rig.store.delete_session(cookie).await.unwrap();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), live.receive_message())
        .await
        .expect("the socket closes once its session is gone");
    assert!(
        matches!(closed, axum_test::WsMessage::Close(_)),
        "{closed:?}"
    );
}

#[tokio::test]
async fn without_a_login_configuration_there_is_no_browser_sign_in() {
    let rig = rig_with(|c| c.login = None).await;

    assert_eq!(rig.app.get("/api/v1/auth/login").await.status_code(), 404);
    assert_eq!(
        rig.app
            .get("/api/v1/auth/callback?code=x&state=y")
            .await
            .status_code(),
        404
    );
}
