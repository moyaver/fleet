// SPDX-License-Identifier: AGPL-3.0-only
//! The full stack in one process tree: the real server (in-process) on a fresh Postgres 18
//! database, the built `fleet-agent` binary, and the fake `maestro` that replays a captured
//! fixture (`crates/fleet-agent/tests/it/fake-maestro.sh`).

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
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
use fleet_server::config::{Config, OidcIssuer, ProjectConfig, Secret, TrustPolicyConfig};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) const VIEWER: &str = "e2e-viewer";

pub(crate) struct Stack {
    pub(crate) server: Option<fleet_server::Server>,
    pub(crate) config: Config,
    pub(crate) key: String,
}

impl Stack {
    pub(crate) async fn start(tweak: impl FnOnce(&mut Config)) -> Self {
        let (store, url) = fleet_store::testing::fresh_database().await.unwrap();
        let mut c = Config {
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            ..Config::default()
        };
        c.database.url = Some(Secret::from(url.as_str()));
        c.ingest.telemetry_flush_ms = 200;
        c.api.viewer_tokens = vec![Secret::from(VIEWER)];
        c.projects = vec![ProjectConfig {
            slug: "app".into(),
            name: "App".into(),
            trust_policies: Vec::new(),
        }];
        // The default blob store is under /var/lib/fleet, which the test host can't write: every
        // upload would fail and wait out its budget.
        c.storage.path =
            std::env::temp_dir().join(format!("fleet-e2e-blobs-{}", uuid::Uuid::now_v7()));
        c.artifacts.spool_dir =
            std::env::temp_dir().join(format!("fleet-e2e-spool-{}", uuid::Uuid::now_v7()));
        tweak(&mut c);
        let server = fleet_server::start(c.clone()).await.unwrap();
        let project = store.project_id("app").await.unwrap().unwrap();
        let (_, key) = store
            .create_api_key(
                project,
                "e2e",
                fleet_store::auth::TrustLevel::Trusted,
                time::OffsetDateTime::now_utc() + time::Duration::days(90),
            )
            .await
            .unwrap();
        Self {
            server: Some(server),
            config: c,
            key: key.expose().to_owned(),
        }
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.server.as_ref().unwrap().addr()
    }

    pub(crate) fn ws_url(&self) -> String {
        format!("ws://{}", self.addr())
    }

    /// Stops the server and starts a new one on the same port and database.
    pub(crate) async fn restart(&mut self) {
        let addr = self.addr();
        self.server.take().unwrap().stop().await.unwrap();
        self.config.listen = addr;
        self.server = Some(fleet_server::start(self.config.clone()).await.unwrap());
    }

    /// GET as a viewer; the JSON body.
    pub(crate) async fn get(&self, path: &str) -> Value {
        let (status, body) = http(self.addr(), "GET", path, Some(VIEWER), "").await;
        assert_eq!(status, 200, "GET {path}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    /// The snapshot of the project's newest attempt.
    pub(crate) async fn latest_snapshot(&self) -> Value {
        let attempts = self.get("/api/v1/projects/app/attempts").await;
        let id = attempts[0]["id"].as_str().unwrap().to_owned();
        self.get(&format!("/api/v1/projects/app/attempts/{id}"))
            .await
    }

    /// Waits (up to 30 s) until the job's agent run is `ended`, the state the dashboard shows as
    /// Completed, and returns the snapshot. The agent's exit doesn't show that the server stored
    /// the run end: its final flush can run out of budget, so a read right after the exit can
    /// see the run still active. This fails with that state if the run never ends.
    pub(crate) async fn await_run_ended(&self, job_key: &str) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let snap = self.latest_snapshot().await;
            let state = job_run_state(&snap, job_key);
            if state.as_deref() == Some("ended") {
                return snap;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{job_key}: its agent run is still {state:?} 30 s after the agent exited; \
                 not every event reached the server"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}

/// The state of the agent run of the job `job_key` in a snapshot, if both are there.
fn job_run_state(snap: &Value, job_key: &str) -> Option<String> {
    let job = snap["jobs"]
        .as_array()?
        .iter()
        .find(|j| j["job_key"] == job_key)?;
    let run = snap["agent_runs"]
        .as_array()?
        .iter()
        .find(|r| r["job_id"] == job["id"])?;
    run["state"].as_str().map(str::to_owned)
}

/// A minimal HTTP/1.1 request over a raw socket.
pub(crate) async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n{auth}\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    sock.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    let status = text.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (status, body)
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub(crate) fn fixture(name: &str) -> PathBuf {
    repo().join("fixtures/maestro/2.12.1").join(name)
}

/// The workspace's `fleet-agent` binary, built if this test run didn't build it.
pub(crate) fn agent_bin() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    // target/<profile>/deps/it-<hash> → target/<profile>/fleet-agent
    let bin = exe
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("fleet-agent");
    if !bin.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let ok = Command::new(cargo)
            .args(["build", "--locked", "-p", "fleet-agent"])
            .current_dir(repo())
            .status()
            .unwrap()
            .success();
        assert!(ok, "building fleet-agent");
    }
    bin
}

/// Runs `fleet-agent run -- maestro test flows/` with the fake Maestro replaying `fixture`, in a
/// cleared environment (CI sets `GITHUB_*`, which the agent would read) plus `env`.
pub(crate) fn run_agent(fixture_name: &str, env: &[(&str, &str)]) -> Output {
    run_agent_with(fixture_name, env, &[], None).0
}

/// Like [`run_agent`], with `flags` after `test` (before the suite). `{DIR}` in a flag is the
/// run's own scratch directory, so a test can point Maestro's output somewhere specific.
pub(crate) fn run_agent_with_flags(
    fixture_name: &str,
    env: &[(&str, &str)],
    flags: &[&str],
) -> Output {
    run_agent_with(fixture_name, env, flags, None).0
}

/// Like [`run_agent`], and also the run directory (`<run>`), where an offline run keeps its bundle.
pub(crate) fn run_agent_keeping_run_dir(
    fixture_name: &str,
    env: &[(&str, &str)],
) -> (Output, PathBuf) {
    run_agent_with(fixture_name, env, &[], None)
}

/// Like [`run_agent`], but the flows come from a Fleet plan (ADR-0013): `manifest` is written to
/// disk, every flow it names is created (empty) under the agent's working directory, and the
/// agent runs shard 0 with `--plan`. Flow paths in `manifest` are relative to that directory.
pub(crate) fn run_agent_planned(
    fixture_name: &str,
    env: &[(&str, &str)],
    manifest: &Value,
) -> Output {
    run_agent_with(fixture_name, env, &[], Some(manifest)).0
}

fn run_agent_with(
    fixture_name: &str,
    env: &[(&str, &str)],
    flags: &[&str],
    plan: Option<&Value>,
) -> (Output, PathBuf) {
    let dir = std::env::temp_dir().join(format!("fleet-e2e-{}", uuid::Uuid::now_v7()));
    let flags: Vec<String> = flags
        .iter()
        .map(|f| f.replace("{DIR}", &dir.to_string_lossy()))
        .collect();
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(
        repo().join("crates/fleet-agent/tests/it/fake-maestro.sh"),
        dir.join("maestro"),
    )
    .unwrap();
    // A real directory: a suite, so the agent adds the JUnit report (a missing path would not).
    std::fs::create_dir_all(dir.join("flows")).unwrap();
    let mut cmd = Command::new(agent_bin());
    cmd.env_clear();
    for keep in ["PATH", "HOME", "TMPDIR"] {
        if let Some(v) = std::env::var_os(keep) {
            cmd.env(keep, v);
        }
    }
    cmd.args(["run", "--finalize-timeout", "30", "--run-dir"])
        .arg(dir.join("run"));
    if let Some(manifest) = plan {
        // More than one flow file makes the agent add the JUnit report, as a directory does.
        for shard in manifest["shards"].as_array().unwrap() {
            for flow in shard["flows"].as_array().unwrap() {
                let file = dir.join(flow.as_str().unwrap());
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, "appId: e2e\n---\n").unwrap();
            }
        }
        std::fs::write(dir.join("plan.json"), manifest.to_string()).unwrap();
        cmd.args(["--plan", "plan.json", "--shard", "0"])
            .arg("--")
            .arg(dir.join("maestro"))
            .arg("test")
            .current_dir(&dir);
    } else {
        cmd.arg("--")
            .arg(dir.join("maestro"))
            .arg("test")
            .args(&flags)
            .arg(dir.join("flows"));
    }
    cmd.env("FLEET_FAKE_FIXTURE", fixture(fixture_name))
        .env("FLEET_FAKE_LINGER", "0.2")
        .env("FLEET_FLUSH_TIMEOUT", "15")
        .stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    (cmd.output().unwrap(), dir.join("run"))
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A fake GitHub Actions OIDC provider: discovery, JWKS, and the runner's token endpoint
/// (`ACTIONS_ID_TOKEN_REQUEST_URL`). The key is generated per run.
pub(crate) struct Issuer {
    pub(crate) url: String,
    pub(crate) requests: Arc<AtomicU32>,
}

impl Issuer {
    pub(crate) async fn start(claims: Value) -> Self {
        let key = Arc::new(RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let public = key.public_key();
        let jwks = json!({"keys": [{
            "kty": "RSA", "kid": "e2e", "use": "sig", "alg": "RS256",
            "n": b64(public.modulus().big_endian_without_leading_zero()),
            "e": b64(public.exponent().big_endian_without_leading_zero()),
        }]});
        let requests = Arc::new(AtomicU32::new(0));
        let (u1, u2, r) = (url.clone(), url.clone(), Arc::clone(&requests));
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                routing::get(move || {
                    let u = u1.clone();
                    async move { Json(json!({"issuer": u, "jwks_uri": format!("{u}/jwks")})) }
                }),
            )
            .route(
                "/jwks",
                routing::get(move || async move { Json(jwks.clone()) }),
            )
            .route(
                "/token",
                routing::get(move |headers: axum::http::HeaderMap| {
                    let (key, claims, iss, r) =
                        (Arc::clone(&key), claims.clone(), u2.clone(), Arc::clone(&r));
                    async move {
                        assert_eq!(
                            headers.get("authorization").and_then(|v| v.to_str().ok()),
                            Some("Bearer runner-request-token"),
                            "the runner's request token is sent"
                        );
                        r.fetch_add(1, Ordering::SeqCst);
                        Json(json!({ "value": sign(&key, &iss, &claims) }))
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, requests }
    }

    /// Server config: accept this issuer, and trust repository 123 owned by 9.
    pub(crate) fn configure(&self, c: &mut Config) {
        c.oidc.issuers.push(OidcIssuer {
            issuer: self.url.clone(),
            audience: "fleet".into(),
            jwks_hosts: Vec::new(),
        });
        c.projects[0].trust_policies = vec![TrustPolicyConfig {
            name: "acme".into(),
            issuer: self.url.clone(),
            audience: "fleet".into(),
            repository_id: "123".into(),
            repository_owner_id: "9".into(),
            trusted_refs: vec!["refs/heads/main".into()],
            allowed_events: vec![],
            pr_policy: "restricted".into(),
            job_workflow_ref: None,
            environment: None,
        }];
    }
}

fn sign(key: &RsaKeyPair, iss: &str, extra: &Value) -> String {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let mut c = json!({
        "iss": iss, "aud": "fleet", "iat": now, "nbf": now - 5, "exp": now + 300,
        "jti": uuid::Uuid::now_v7().to_string(),
        "repository": "acme/app", "repository_id": "123", "repository_owner_id": "9",
        "run_id": "9001", "run_attempt": "1", "check_run_id": "31337", "sha": "abc",
        "ref": "refs/heads/main", "event_name": "push",
    });
    for (k, v) in extra.as_object().into_iter().flatten() {
        c[k] = v.clone();
    }
    let header = b64(br#"{"alg":"RS256","typ":"JWT","kid":"e2e"}"#);
    let input = format!("{header}.{}", b64(c.to_string().as_bytes()));
    let mut sig = vec![0u8; key.public_modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        input.as_bytes(),
        &mut sig,
    )
    .unwrap();
    format!("{input}.{}", b64(&sig))
}

/// An HTTP CONNECT proxy that counts the tunnels it opened.
pub(crate) async fn connect_proxy() -> (SocketAddr, Arc<AtomicU32>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tunnels = Arc::new(AtomicU32::new(0));
    let t = Arc::clone(&tunnels);
    tokio::spawn(async move {
        loop {
            let Ok((mut c, _)) = listener.accept().await else {
                return;
            };
            let t = Arc::clone(&t);
            tokio::spawn(async move {
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let Ok(b) = c.read_u8().await else { return };
                    head.push(b);
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let Some(target) = head
                    .strip_prefix("CONNECT ")
                    .and_then(|r| r.split(' ').next())
                else {
                    let _ = c
                        .write_all(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
                        .await;
                    return;
                };
                let Ok(mut up) = tokio::net::TcpStream::connect(target).await else {
                    let _ = c.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    return;
                };
                t.fetch_add(1, Ordering::SeqCst);
                let _ = c
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await;
                let _ = tokio::io::copy_bidirectional(&mut c, &mut up).await;
            });
        }
    });
    (addr, tunnels)
}

/// The flows of a snapshot as (name, state, finality, source), sorted.
pub(crate) fn flows(snapshot: &Value) -> Vec<(String, String, String, String)> {
    let mut v: Vec<_> = snapshot["flows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["name"].as_str().unwrap_or_default().to_owned(),
                f["state"].as_str().unwrap_or_default().to_owned(),
                f["finality"].as_str().unwrap_or_default().to_owned(),
                f["outcome_source"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    v.sort();
    v
}
