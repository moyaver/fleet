// SPDX-License-Identifier: AGPL-3.0-only
//! A fake company identity provider for the browser (PLAN §7.5): what Okta, Entra ID or Keycloak
//! do for a person who clicks "Sign in", on wiremock. It serves discovery, the JWKS, an
//! `/authorize` endpoint that redirects straight back (the person is always "logged in at the
//! provider") and a `/token` endpoint that checks the PKCE verifier and hands out an ID token
//! signed with a key generated per run.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use aws_lc_rs::{
    digest,
    rand::SystemRandom,
    signature::{KeyPair as _, RSA_PKCS1_SHA256, RsaKeyPair},
};
use base64::Engine as _;
use serde_json::{Value, json};
use time::OffsetDateTime;
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path},
};

/// The client ID Fleet is registered under.
pub(crate) const CLIENT_ID: &str = "fleet-web";

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// What a code was issued for: the login's nonce and PKCE challenge.
#[derive(Debug, Clone)]
struct Issued {
    nonce: String,
    challenge: String,
}

#[derive(Debug)]
struct Shared {
    key: RsaKeyPair,
    issuer: String,
    /// The groups the person belongs to; tests change them between sign-ins.
    groups: Mutex<Vec<String>>,
    codes: Mutex<HashMap<String, Issued>>,
}

/// The provider, listening on a local port until dropped.
pub(crate) struct FakeIdp {
    server: MockServer,
    shared: Arc<Shared>,
}

struct Authorize(Arc<Shared>);

impl Respond for Authorize {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let q: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
        let param = |k: &str| q.get(k).cloned().unwrap_or_default();
        assert_eq!(param("response_type"), "code");
        assert_eq!(param("client_id"), CLIENT_ID);
        assert_eq!(param("code_challenge_method"), "S256", "PKCE is mandatory");
        let code = uuid::Uuid::now_v7().to_string();
        self.0.codes.lock().unwrap().insert(
            code.clone(),
            Issued {
                nonce: param("nonce"),
                challenge: param("code_challenge"),
            },
        );
        let mut back = url::Url::parse(&param("redirect_uri")).unwrap();
        back.query_pairs_mut()
            .append_pair("code", &code)
            .append_pair("state", &param("state"));
        ResponseTemplate::new(302).insert_header("location", back.as_str())
    }
}

struct Token(Arc<Shared>);

impl Respond for Token {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let form: HashMap<String, String> = url::form_urlencoded::parse(&request.body)
            .into_owned()
            .collect();
        let issued = form
            .get("code")
            .and_then(|c| self.0.codes.lock().unwrap().remove(c));
        let verifier = form.get("code_verifier").map_or("", String::as_str);
        let proven = issued.as_ref().is_some_and(|i| {
            b64(digest::digest(&digest::SHA256, verifier.as_bytes()).as_ref()) == i.challenge
        });
        match issued.filter(|_| proven) {
            Some(issued) => ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "at", "token_type": "Bearer", "expires_in": 60,
                "id_token": self.0.sign(&issued.nonce),
            })),
            None => ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})),
        }
    }
}

impl Shared {
    fn sign(&self, nonce: &str) -> String {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let claims = json!({
            "iss": self.issuer, "aud": CLIENT_ID, "sub": "ada",
            "iat": now, "exp": now + 300, "nonce": nonce,
            "email": "ada@example.com", "name": "Ada Lovelace",
            "groups": *self.groups.lock().unwrap(),
        });
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

impl FakeIdp {
    /// Starts the provider; the person belongs to `groups` until [`FakeIdp::set_groups`].
    pub(crate) async fn start(groups: &[&str]) -> Self {
        let server = MockServer::start().await;
        let issuer = server.uri();
        let key = RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        let public = key.public_key();
        let jwks = json!({"keys": [{
            "kty": "RSA", "kid": "idp-1", "use": "sig", "alg": "RS256",
            "n": b64(public.modulus().big_endian_without_leading_zero()),
            "e": b64(public.exponent().big_endian_without_leading_zero()),
        }]});
        let shared = Arc::new(Shared {
            key,
            issuer: issuer.clone(),
            groups: Mutex::new(groups.iter().map(|g| (*g).to_owned()).collect()),
            codes: Mutex::new(HashMap::new()),
        });
        let discovery: Value = json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
        });
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(discovery))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jwks))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/authorize"))
            .respond_with(Authorize(Arc::clone(&shared)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(Token(Arc::clone(&shared)))
            .mount(&server)
            .await;
        Self { server, shared }
    }

    /// The issuer URL to configure in Fleet.
    pub(crate) fn url(&self) -> String {
        self.server.uri()
    }

    /// Changes the groups in the ID tokens of the next sign-ins.
    pub(crate) fn set_groups(&self, groups: &[&str]) {
        *self.shared.groups.lock().unwrap() = groups.iter().map(|g| (*g).to_owned()).collect();
    }

    /// How many authorization codes were redeemed (successfully or not) at the token endpoint.
    pub(crate) async fn token_requests(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/token")
            .count()
    }
}
