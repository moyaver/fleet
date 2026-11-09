// SPDX-License-Identifier: AGPL-3.0-only
//! Server configuration (research 03 §11): defaults, then a TOML file, then `FLEET_*`
//! environment variables (`__` separates sections: `FLEET_DATABASE__URL`), then `FLEET_*_FILE`
//! variables whose value is a path to read the setting from (Docker and Kubernetes secrets).
//!
//! Errors name the source and key of a bad value. Secrets never print: `check-config` shows
//! `<redacted>`.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use figment::{
    Figment, Metadata, Profile, Provider,
    providers::{Format, Serialized, Toml},
    value::{Dict, Map, Value},
};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};

/// A secret setting: deserializes from a string, never prints.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// The value. Use only where the secret is needed.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("<redacted>")
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Everything the server reads at start-up.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Where to listen.
    pub listen: SocketAddr,
    /// Addresses of reverse proxies in front of Fleet, as CIDR blocks (`10.0.0.0/8`,
    /// `fd00::/8`). Empty: the socket peer is always the client, and `X-Forwarded-For` is
    /// ignored. A peer inside one of these blocks is a proxy, and its `X-Forwarded-For` names
    /// the client. See `docs/src/operator-guide.md`.
    pub trusted_proxies: Vec<IpNet>,
    /// Postgres.
    pub database: Database,
    /// The agent protocol.
    pub ingest: Ingest,
    /// Request and event-rate limits (`429 Too Many Requests` with `Retry-After`).
    pub rate_limits: RateLimits,
    /// Liveness thresholds (research 05 §6.8).
    pub liveness: Liveness,
    /// Read APIs (REST and the browser WebSocket).
    pub api: Api,
    /// CI OIDC token exchange (`POST /api/v1/agent/token`).
    pub oidc: Oidc,
    /// Browser sign-in with the company's identity provider (PLAN §7.5). Unset: no browser
    /// sessions, and reads need a viewer token.
    pub login: Option<Login>,
    /// Where artifacts are stored (PLAN §7.6, ADR-0007).
    pub storage: Storage,
    /// What agents may upload, and how much (PLAN §6.6, §7.6).
    pub artifacts: Artifacts,
    /// The second origin that serves artifact bytes (PLAN §7.6, invariant I7).
    pub usercontent: Usercontent,
    /// How long telemetry, the journal, the audit log and artifact blobs are kept (PLAN §9).
    pub retention: Retention,
    /// Projects to create at start-up (by slug). Screen G shows them; changing them is by this list.
    pub projects: Vec<ProjectConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
            trusted_proxies: Vec::new(),
            database: Database::default(),
            ingest: Ingest::default(),
            rate_limits: RateLimits::default(),
            liveness: Liveness::default(),
            api: Api::default(),
            oidc: Oidc::default(),
            login: None,
            storage: Storage::default(),
            artifacts: Artifacts::default(),
            usercontent: Usercontent::default(),
            retention: Retention::default(),
            projects: Vec::new(),
        }
    }
}

/// Which blob store holds artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageBackend {
    /// A directory on the server's disk (`storage.path`): small installs and tests.
    LocalFs,
    /// Any S3-compatible store (`storage.s3`): Garage in the reference deployment.
    S3,
}

/// Artifact storage (ADR-0007).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Storage {
    /// Which backend.
    pub backend: StorageBackend,
    /// Blob directory for `local_fs`. Must be on a persistent volume.
    pub path: PathBuf,
    /// Settings for `s3`.
    pub s3: S3Settings,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            backend: StorageBackend::LocalFs,
            path: PathBuf::from("/var/lib/fleet/blobs"),
            s3: S3Settings::default(),
        }
    }
}

/// How long data is kept, and how often the leader's retention jobs run (PLAN §7.7, §9).
///
/// Durations are whole days; **0 keeps the data for ever**. Artifact retention classes are
/// per project (`retention_classes`, seeded with the research 05 §12.3 defaults). Screen G shows
/// them read only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Retention {
    /// Raw 5 s telemetry samples.
    pub raw_samples_days: u32,
    /// One-minute rollups.
    pub rollup_1m_days: u32,
    /// One-hour rollups.
    pub rollup_1h_days: u32,
    /// The raw agent journal.
    pub journal_days: u32,
    /// Step executions.
    pub steps_days: u32,
    /// The audit log.
    pub audit_days: u32,
    /// How long a blob no artifact refers to is kept before its object is deleted.
    pub blob_grace_hours: u32,
    /// How long an expired artifact's row is kept as a tombstone for history.
    pub expired_artifact_days: u32,
    /// Temp files of crashed uploads older than this are removed from a local blob store.
    pub stale_upload_hours: u32,
    /// How often telemetry is rolled up.
    pub rollup_interval_seconds: u64,
    /// How often artifacts expire and blobs are collected.
    pub gc_interval_minutes: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            raw_samples_days: 7,
            rollup_1m_days: 90,
            rollup_1h_days: 365,
            journal_days: 14,
            steps_days: 14,
            audit_days: 365,
            blob_grace_hours: 24,
            expired_artifact_days: 30,
            stale_upload_hours: 24,
            rollup_interval_seconds: 60,
            gc_interval_minutes: 10,
        }
    }
}

fn days(d: u32) -> Option<std::time::Duration> {
    (d > 0).then(|| std::time::Duration::from_secs(u64::from(d) * 86_400))
}

impl Retention {
    /// The store's view: `None` keeps for ever.
    #[must_use]
    pub fn policy(&self) -> fleet_store::retention::Retention {
        fleet_store::retention::Retention {
            raw_samples: days(self.raw_samples_days),
            rollup_1m: days(self.rollup_1m_days),
            rollup_1h: days(self.rollup_1h_days),
            journal: days(self.journal_days),
            steps: days(self.steps_days),
            audit: days(self.audit_days),
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.rollup_interval_seconds > 0 && self.gc_interval_minutes > 0,
            "retention.rollup_interval_seconds and retention.gc_interval_minutes must be above 0"
        );
        Ok(())
    }
}

/// Artifact upload policy (PLAN §6.6; research 06 T11, T12).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Artifacts {
    /// Where uploaded bytes wait between the `PUT` and verification. A local disk with room for
    /// the largest upload times the uploads in flight; created on first use.
    pub spool_dir: PathBuf,
    /// How long an upload grant lasts. Grants are single-use and short-lived.
    pub grant_ttl_seconds: u64,
    /// Most bytes one agent run may store.
    pub max_run_bytes: u64,
    /// Most files one agent run may store.
    pub max_run_files: u32,
    /// Most bytes one project may declare in any 24 hours, across all its runs.
    pub max_project_bytes_per_day: u64,
    /// Most files one project may declare in any 24 hours, across all its runs.
    pub max_project_files_per_day: u32,
    /// Size limits per class, in bytes.
    pub max_bytes: ClassLimits,
}

impl Default for Artifacts {
    fn default() -> Self {
        Self {
            spool_dir: PathBuf::from("/var/lib/fleet/spool"),
            grant_ttl_seconds: 600,
            max_run_bytes: 2 * 1024 * 1024 * 1024,
            max_run_files: 2_000,
            max_project_bytes_per_day: 20 * 1024 * 1024 * 1024,
            max_project_files_per_day: 20_000,
            max_bytes: ClassLimits::default(),
        }
    }
}

/// The largest accepted file of each class, in bytes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct ClassLimits {
    /// `failure_screenshot`.
    pub failure_screenshot: u64,
    /// `failure_hierarchy`.
    pub failure_hierarchy: u64,
    /// `commands`.
    pub commands: u64,
    /// `junit`.
    pub junit: u64,
    /// `runner_log`.
    pub runner_log: u64,
    /// `html_report`.
    pub html_report: u64,
    /// `device_logcat`: the excerpt of a failed flow's device log. The agent cuts it to at most
    /// 512 KiB (issue #75), so this is the ceiling it can reach.
    pub device_logcat: u64,
    /// `user_screenshot`: a screenshot the flow took with `takeScreenshot` (issue #133).
    pub user_screenshot: u64,
    /// `user_recording`: a recording the flow made with `startRecording` (issue #133).
    pub user_recording: u64,
}

impl Default for ClassLimits {
    fn default() -> Self {
        const MIB: u64 = 1024 * 1024;
        Self {
            failure_screenshot: 20 * MIB,
            failure_hierarchy: 4 * MIB,
            commands: 16 * MIB,
            junit: 32 * MIB,
            runner_log: 64 * MIB,
            html_report: 128 * MIB,
            device_logcat: 512 * 1024,
            user_screenshot: 20 * MIB,
            user_recording: 100 * MIB,
        }
    }
}

impl ClassLimits {
    /// The limit for `class`.
    #[must_use]
    pub fn of(&self, class: fleet_protocol::artifact::ArtifactClass) -> u64 {
        use fleet_protocol::artifact::ArtifactClass as C;
        match class {
            C::FailureScreenshot => self.failure_screenshot,
            C::FailureHierarchy => self.failure_hierarchy,
            C::Commands => self.commands,
            C::Junit => self.junit,
            C::RunnerLog => self.runner_log,
            C::HtmlReport => self.html_report,
            C::DeviceLogcat => self.device_logcat,
            C::UserScreenshot => self.user_screenshot,
            C::UserRecording => self.user_recording,
        }
    }
}

/// The usercontent origin (PLAN §7.6, research 06 §6.2). Uploaded files, above all Maestro's HTML
/// reports, are hostile input: they are served only from here, a different site from the
/// dashboard, never with cookies, always sandboxed, and only through short-lived signed links.
/// Unset: artifacts are stored but cannot be opened.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Usercontent {
    /// The origin's public URL (`https://fleet-usercontent.example.net`): a different host from
    /// `api.public_url`, and ideally a different registrable domain. Requests whose `Host` is this
    /// one get artifacts and nothing else; every other `Host` never gets artifact bytes.
    pub url: Option<String>,
    /// Also listen here, for an origin published on its own port. Optional: a proxy that routes
    /// both hostnames to `listen` (the Compose file does) needs no second listener.
    pub listen: Option<SocketAddr>,
    /// The HMAC key that signs links, at least 32 bytes. Set it with
    /// `FLEET_USERCONTENT__SIGNING_KEY_FILE`. Changing it invalidates every link at once.
    pub signing_key: Option<Secret>,
    /// How long a link works. Research 06 T15: at most 5 minutes.
    pub link_ttl_seconds: u64,
}

impl Default for Usercontent {
    fn default() -> Self {
        Self {
            url: None,
            listen: None,
            signing_key: None,
            link_ttl_seconds: 300,
        }
    }
}

/// Shortest accepted signing key: 256 bits, the size of the HMAC-SHA256 output.
const MIN_SIGNING_KEY_BYTES: usize = 32;

/// `host[:port]` of a URL, lowercase, with the scheme's default port made explicit.
fn authority(url: &url::Url) -> Option<String> {
    Some(format!(
        "{}:{}",
        url.host_str()?.to_ascii_lowercase(),
        url.port_or_known_default()?
    ))
}

impl Usercontent {
    /// The `Host` header values that mean this origin: `host:port`, and the bare host when the
    /// port is the scheme's default. Empty when not configured.
    pub(crate) fn hosts(&self) -> Vec<String> {
        let Some(url) = self.url.as_deref().and_then(|u| url::Url::parse(u).ok()) else {
            return Vec::new();
        };
        let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
            return Vec::new();
        };
        let host = host.to_ascii_lowercase();
        let mut hosts = vec![format!("{host}:{port}")];
        if url.port().is_none() {
            hosts.push(host);
        }
        hosts
    }

    fn validate(&self, public_url: Option<&str>) -> anyhow::Result<()> {
        let Some(raw) = &self.url else {
            anyhow::ensure!(
                self.listen.is_none() && self.signing_key.is_none(),
                "usercontent.listen and usercontent.signing_key are set but usercontent.url is not"
            );
            return Ok(());
        };
        let url = url::Url::parse(raw)
            .with_context(|| format!("usercontent.url {raw:?} is not a URL"))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https") && url.path() == "/",
            "usercontent.url must be an http:// or https:// origin without a path"
        );
        let own = authority(&url).context("usercontent.url has no host")?;
        let app = public_url
            .context("usercontent.url needs api.public_url: it names the one origin allowed to embed and fetch artifacts")?;
        let app = url::Url::parse(app)
            .ok()
            .and_then(|u| authority(&u))
            .context("api.public_url is not a URL")?;
        anyhow::ensure!(
            own != app,
            "usercontent.url must be a different host from api.public_url: user content must \
             never share an origin with the dashboard (invariant I7)"
        );
        let key = self.signing_key.as_ref().map_or(0, |k| k.expose().len());
        anyhow::ensure!(
            key >= MIN_SIGNING_KEY_BYTES,
            "usercontent.signing_key must be at least {MIN_SIGNING_KEY_BYTES} bytes \
             (set FLEET_USERCONTENT__SIGNING_KEY_FILE; `openssl rand -base64 48` makes one)"
        );
        anyhow::ensure!(
            (1..=300).contains(&self.link_ttl_seconds),
            "usercontent.link_ttl_seconds must be between 1 and 300 (research 06 T15: links last at most five minutes)"
        );
        Ok(())
    }
}

/// An S3-compatible bucket, addressed path-style at a custom endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct S3Settings {
    /// The S3 API's base URL, for example `http://garage:3900`.
    pub endpoint: Option<String>,
    /// Signing region. Garage's default is `garage`.
    pub region: String,
    /// The base URL agents reach the bucket at, for direct uploads (PLAN §7.6). Presigned URLs
    /// name this host, so it must be one agents can reach: `endpoint` is often a name only the
    /// server's network resolves. Unset: presigned URLs use `endpoint`.
    pub public_endpoint: Option<String>,
    /// The bucket; it must exist.
    pub bucket: Option<String>,
    /// Access key id (`FLEET_STORAGE__S3__ACCESS_KEY_ID_FILE` can hold it).
    pub access_key_id: Option<Secret>,
    /// Secret access key (`FLEET_STORAGE__S3__SECRET_ACCESS_KEY_FILE` can hold it).
    pub secret_access_key: Option<Secret>,
}

impl Storage {
    fn validate(&self) -> anyhow::Result<()> {
        match self.backend {
            StorageBackend::LocalFs => anyhow::ensure!(
                !self.path.as_os_str().is_empty(),
                "storage.path is empty: set the blob directory"
            ),
            StorageBackend::S3 => {
                let s3 = &self.s3;
                let endpoint = s3.endpoint.as_deref().unwrap_or_default();
                let url = url::Url::parse(endpoint)
                    .with_context(|| format!("storage.s3.endpoint {endpoint:?} is not a URL"))?;
                anyhow::ensure!(
                    matches!(url.scheme(), "http" | "https"),
                    "storage.s3.endpoint must be an http:// or https:// URL"
                );
                if let Some(public) = s3.public_endpoint.as_deref() {
                    let url = url::Url::parse(public).with_context(|| {
                        format!("storage.s3.public_endpoint {public:?} is not a URL")
                    })?;
                    anyhow::ensure!(
                        matches!(url.scheme(), "http" | "https"),
                        "storage.s3.public_endpoint must be an http:// or https:// URL"
                    );
                }
                for (key, value) in [
                    ("bucket", s3.bucket.as_deref()),
                    ("region", Some(s3.region.as_str())),
                    (
                        "access_key_id",
                        s3.access_key_id.as_ref().map(Secret::expose),
                    ),
                    (
                        "secret_access_key",
                        s3.secret_access_key.as_ref().map(Secret::expose),
                    ),
                ] {
                    anyhow::ensure!(
                        value.is_some_and(|v| !v.trim().is_empty()),
                        "storage.backend is s3 but storage.s3.{key} is not set"
                    );
                }
            }
        }
        Ok(())
    }

    /// Opens the configured blob store. Makes no request to an S3 endpoint: a wrong endpoint or
    /// credential shows on first use.
    pub fn open(&self) -> anyhow::Result<Arc<dyn fleet_blob::BlobStore>> {
        Ok(match self.backend {
            StorageBackend::LocalFs => Arc::new(
                fleet_blob::LocalFs::new(&self.path)
                    .with_context(|| format!("opening blob directory {}", self.path.display()))?,
            ),
            StorageBackend::S3 => {
                let s3 = &self.s3;
                let get = |v: Option<&str>| v.unwrap_or_default().to_owned();
                Arc::new(
                    fleet_blob::S3::new(&fleet_blob::S3Config {
                        endpoint: get(s3.endpoint.as_deref()),
                        region: s3.region.clone(),
                        bucket: get(s3.bucket.as_deref()),
                        access_key_id: get(s3.access_key_id.as_ref().map(Secret::expose)),
                        secret_access_key: get(s3.secret_access_key.as_ref().map(Secret::expose)),
                        public_endpoint: s3
                            .public_endpoint
                            .clone()
                            .filter(|p| !p.trim().is_empty()),
                    })
                    .context("opening the S3 blob store")?,
                )
            }
        })
    }
}

impl Default for S3Settings {
    fn default() -> Self {
        Self {
            endpoint: None,
            region: "garage".into(),
            public_endpoint: None,
            bucket: None,
            access_key_id: None,
            secret_access_key: None,
        }
    }
}

/// When migrations run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrateMode {
    /// On start, under an advisory lock (the Compose default).
    Auto,
    /// Only by `fleet migrate`; `serve` refuses an outdated schema.
    Manual,
}

/// Postgres settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Database {
    /// `postgres://…` (set it with `FLEET_DATABASE__URL` or `FLEET_DATABASE__URL_FILE`).
    pub url: Option<Secret>,
    /// Pool size.
    pub max_connections: u32,
    /// When migrations run.
    pub migrate: MigrateMode,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            url: None,
            max_connections: 20,
            migrate: MigrateMode::Auto,
        }
    }
}

/// Agent protocol limits and timers (research 05 §3.5, §3.8, §3.9).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Ingest {
    /// How often agents send a heartbeat.
    pub heartbeat_interval_ms: u64,
    /// WebSocket Ping cadence: below common 60 s proxy idle cuts.
    pub ping_interval_ms: u64,
    /// Close a socket with no inbound frame for this long.
    pub idle_timeout_ms: u64,
    /// How long a new socket may take to say `hello`.
    pub hello_timeout_ms: u64,
    /// Largest frame accepted.
    pub max_frame_bytes: u32,
    /// Events in flight per agent.
    pub max_inflight_events: u32,
    /// Telemetry checkpoint: buffered samples are written and acked this often (10–30 s).
    pub telemetry_flush_ms: u64,
    /// On shutdown: how long to drain sockets before closing them anyway.
    pub shutdown_grace_ms: u64,
    /// `goaway` asks agents to reconnect after a random delay up to this.
    pub reconnect_jitter_ms: u64,
    /// How often agents sample host and process metrics while the host is calm, in milliseconds
    /// (1 000 to 300 000). Unset: 5 s. A project may override it on the dashboard.
    pub sample_interval_ms: Option<u64>,
    /// How often agents sample while the host is under pressure, in milliseconds (1 000 to
    /// 300 000). Unset: 15 s. A project may override it on the dashboard.
    pub pressure_sample_interval_ms: Option<u64>,
}

impl Default for Ingest {
    fn default() -> Self {
        Self {
            heartbeat_interval_ms: 10_000,
            ping_interval_ms: 20_000,
            idle_timeout_ms: 45_000,
            hello_timeout_ms: 10_000,
            max_frame_bytes: 256 * 1024,
            max_inflight_events: 512,
            telemetry_flush_ms: 15_000,
            shutdown_grace_ms: 15_000,
            reconnect_jitter_ms: 2_000,
            sample_interval_ms: None,
            pressure_sample_interval_ms: None,
        }
    }
}

/// Request and event-rate limits (research 03 §11). A refusal is `429 Too Many Requests` with
/// `Retry-After` in whole seconds, or `retry_after_ms` on the protocol's `reject` frame.
///
/// Each limit is a token bucket: `burst` requests may come at once, then tokens return at the
/// rate given. Keys that have refilled are forgotten, so memory stays bounded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimits {
    /// Pre-auth requests per client IP (`POST /api/v1/agent/token`, browser sign-in and its
    /// callback), per minute. The client IP comes from `trusted_proxies`.
    pub ip_requests_per_minute: u32,
    /// Pre-auth requests per client IP that may arrive at once.
    pub ip_burst: u32,
    /// Agent connections per credential (WebSocket upgrades and batch requests), per minute.
    pub credential_requests_per_minute: u32,
    /// Agent connections per credential that may arrive at once.
    pub credential_burst: u32,
    /// Events per second, on average, that one agent socket may send. Beyond it the server
    /// reads that socket more slowly, so the agent's own backpressure applies: events are never
    /// dropped (invariant I5).
    pub events_per_second: u32,
    /// Events one agent socket may send at once before it is slowed.
    pub event_burst: u32,
    /// Viewer requests per credential, per minute: REST reads and live socket upgrades. The
    /// credential is a configured viewer token or a browser session; a request without an
    /// accepted credential is not counted.
    pub viewer_requests_per_minute: u32,
    /// Viewer requests per credential that may arrive at once.
    pub viewer_burst: u32,
    /// Keys tracked by each limiter. A new key beyond this waits one second and is retried,
    /// until an idle key has aged out.
    pub max_tracked_keys: usize,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            ip_requests_per_minute: 120,
            ip_burst: 60,
            credential_requests_per_minute: 1_200,
            credential_burst: 300,
            events_per_second: 1_000,
            event_burst: 5_000,
            viewer_requests_per_minute: 3_000,
            viewer_burst: 600,
            max_tracked_keys: 100_000,
        }
    }
}

impl RateLimits {
    fn validate(&self) -> anyhow::Result<()> {
        for (key, value) in [
            ("ip_requests_per_minute", self.ip_requests_per_minute),
            ("ip_burst", self.ip_burst),
            (
                "credential_requests_per_minute",
                self.credential_requests_per_minute,
            ),
            ("credential_burst", self.credential_burst),
            ("events_per_second", self.events_per_second),
            ("event_burst", self.event_burst),
            (
                "viewer_requests_per_minute",
                self.viewer_requests_per_minute,
            ),
            ("viewer_burst", self.viewer_burst),
        ] {
            anyhow::ensure!(value > 0, "rate_limits.{key} must be above 0");
        }
        anyhow::ensure!(
            self.max_tracked_keys > 0,
            "rate_limits.max_tracked_keys must be above 0"
        );
        Ok(())
    }
}

/// Liveness thresholds (research 05 §6.8).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Liveness {
    /// A worker shows `stale` after this long without a frame (display only).
    pub stale_after_ms: u64,
    /// A detached run is abandoned after this long; its open flows become `unknown`.
    pub resume_grace_ms: u64,
    /// How often the leader sweeps.
    pub sweep_interval_ms: u64,
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            stale_after_ms: 25_000,
            resume_grace_ms: 15 * 60_000,
            sweep_interval_ms: 10_000,
        }
    }
}

/// Who may read run data until browser SSO arrives (M4). Agent credentials never can: they are
/// write-only (PLAN §7.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Api {
    /// Bearer tokens that may read every project. A single string is one token, so
    /// `FLEET_API__VIEWER_TOKENS_FILE` can hold it.
    #[serde(deserialize_with = "one_or_many")]
    pub viewer_tokens: Vec<Secret>,
    /// Let anyone read. Only for local development: run data is private (spec §10).
    pub anonymous_read: bool,
    /// The dashboard's public origin (`https://fleet.example.com`). Browser sockets from any
    /// other `Origin` are refused (cross-site WebSocket hijacking). Unset: the `Host` header.
    pub public_url: Option<String>,
    /// Live messages buffered per project for slow browsers; beyond that they `resync`.
    pub live_buffer: usize,
    /// How often an open browser socket checks that its sign-in is still live. Logout or expiry
    /// closes the socket within this time (research 06 T17, T21).
    pub session_recheck_ms: u64,
    /// Days until a new API key expires when `fleet-server api-key create` is not given
    /// `--expires-in-days` (research 06 T3). Above 0.
    pub api_key_ttl_days: u32,
}

impl Default for Api {
    fn default() -> Self {
        Self {
            viewer_tokens: Vec::new(),
            anonymous_read: false,
            public_url: None,
            live_buffer: 1024,
            session_recheck_ms: 30_000,
            api_key_ttl_days: 90,
        }
    }
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Secret>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Secret),
        Many(Vec<Secret>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

/// The GitHub Actions OIDC issuer.
pub const GITHUB_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// Which CI OIDC issuers are accepted (an allowlist: an unlisted `iss` is never fetched).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Oidc {
    /// Accepted issuers and the audience each must carry.
    pub issuers: Vec<OidcIssuer>,
    /// Lifetime of minted agent tokens (the GitHub-hosted job limit is 6 h).
    pub token_ttl_minutes: u64,
    /// Refuse ID tokens issued longer ago than this.
    pub max_token_age_seconds: u64,
    /// Clock leeway for `exp`, `nbf` and `iat`.
    pub leeway_seconds: u64,
}

impl Default for Oidc {
    fn default() -> Self {
        Self {
            issuers: vec![OidcIssuer {
                issuer: GITHUB_ISSUER.into(),
                audience: fleet_protocol::brand::OIDC_AUDIENCE.into(),
                jwks_hosts: Vec::new(),
            }],
            token_ttl_minutes: 360,
            max_token_age_seconds: 600,
            leeway_seconds: 60,
        }
    }
}

/// One accepted issuer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidcIssuer {
    /// `iss`, exactly.
    pub issuer: String,
    /// The audience the agent requests (`aud`).
    pub audience: String,
    /// Hosts, besides the issuer's own, that this issuer's `jwks_uri` may name. Most providers
    /// serve their keys on the issuer's host and need none (research 06 T14).
    #[serde(default)]
    pub jwks_hosts: Vec<String>,
}

/// Browser sign-in: OIDC authorization code with PKCE and nonce, against one identity provider
/// (PLAN §7.5). The redirect URI is `<api.public_url>/api/v1/auth/callback`: register it there.
/// The provider must sign ID tokens with RS256 (Entra ID, Okta, Keycloak and Google do by default).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Login {
    /// The provider's issuer URL; `/.well-known/openid-configuration` hangs off it.
    pub issuer: String,
    /// The client ID registered for Fleet.
    pub client_id: String,
    /// The client secret of a confidential client; omit for a public client (PKCE only).
    #[serde(default)]
    pub client_secret: Option<Secret>,
    /// Scopes to request; `openid` is always sent.
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// ID token claims that carry the user's groups, all of them read: `groups` for most
    /// providers, `roles` for Entra ID app roles.
    #[serde(default = "default_group_claims")]
    pub group_claims: Vec<String>,
    /// How long a session lasts. Roles are resolved at sign-in, so this also bounds how long a
    /// revoked group membership keeps working.
    #[serde(default = "default_session_minutes")]
    pub session_ttl_minutes: u64,
    /// How long a session may go unused before it ends, however much of `session_ttl_minutes`
    /// is left (research 06 T21). A request renews it; an open live socket does not.
    #[serde(default = "default_session_idle_minutes")]
    pub session_idle_minutes: u64,
    /// Other hosts this provider's `jwks_uri` may name, besides the issuer's own (research 06
    /// T14). Most providers need none.
    #[serde(default)]
    pub jwks_hosts: Vec<String>,
    /// Which group gets which role. A user with no matching group is denied.
    #[serde(default)]
    pub group_roles: Vec<GroupRole>,
}

fn default_scopes() -> Vec<String> {
    vec!["profile".into(), "email".into()]
}

fn default_group_claims() -> Vec<String> {
    vec!["groups".into(), "roles".into()]
}

fn default_session_minutes() -> u64 {
    8 * 60
}

/// An hour without a request ends a browser session. Research 06 suggests 8 h idle, but here the
/// absolute lifetime is 8 h, so that idle timeout would never fire.
fn default_session_idle_minutes() -> u64 {
    60
}

/// One identity-provider group and the role it grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRole {
    /// The group claim value (a name, an object ID, or an app role).
    pub group: String,
    /// `viewer`, `maintainer` or `admin`.
    pub role: fleet_core::policy::Role,
    /// The project slug, or unset for every project (an `admin` here is a global admin).
    #[serde(default)]
    pub project: Option<String>,
}

/// A project created at start-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// URL-safe identifier.
    pub slug: String,
    /// Display name.
    pub name: String,
    /// Who may publish here with a CI OIDC token (ADR-0008). Replaces the stored set at start-up.
    #[serde(default)]
    pub trust_policies: Vec<TrustPolicyConfig>,
}

/// A publisher trust policy (research 06 §2.5). IDs are GitHub's immutable numeric IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustPolicyConfig {
    /// Unique per project.
    pub name: String,
    /// Issuer; GitHub Actions by default.
    #[serde(default = "github_issuer")]
    pub issuer: String,
    /// Audience; `fleet` by default.
    #[serde(default = "default_audience")]
    pub audience: String,
    /// `repository_id` claim.
    #[serde(deserialize_with = "string_or_number")]
    pub repository_id: String,
    /// `repository_owner_id` claim.
    #[serde(deserialize_with = "string_or_number")]
    pub repository_owner_id: String,
    /// Refs whose pushes are trusted (`*` wildcards).
    #[serde(default)]
    pub trusted_refs: Vec<String>,
    /// Events accepted; empty means `push`, `schedule`, `workflow_dispatch`, `merge_group`
    /// and `pull_request`. `pull_request_target` and `dynamic` are always refused.
    #[serde(default)]
    pub allowed_events: Vec<String>,
    /// `restricted` (default), `full` or `deny`.
    #[serde(default = "restricted")]
    pub pr_policy: String,
    /// Only tokens minted inside this reusable workflow.
    #[serde(default)]
    pub job_workflow_ref: Option<String>,
    /// Only jobs in this environment.
    #[serde(default)]
    pub environment: Option<String>,
}

impl TrustPolicyConfig {
    /// Checks one policy the way the configuration does, and the dashboard checks a policy it adds
    /// or edits: a name, the IDs it matches, a known pull-request policy, and an issuer the server
    /// accepts (`oidc.issuers`).
    pub(crate) fn check(&self, oidc: &Oidc) -> anyhow::Result<()> {
        anyhow::ensure!(!self.name.trim().is_empty(), "a trust policy needs a name");
        anyhow::ensure!(
            !self.repository_id.trim().is_empty() && !self.repository_owner_id.trim().is_empty(),
            "the repository and owner IDs are required"
        );
        anyhow::ensure!(
            ["restricted", "full", "deny"].contains(&self.pr_policy.as_str()),
            "pr_policy must be restricted, full or deny"
        );
        anyhow::ensure!(
            oidc.issuers.iter().any(|i| i.issuer == self.issuer),
            "issuer {} is not in oidc.issuers",
            self.issuer
        );
        Ok(())
    }

    /// The matching rule this policy stores.
    #[must_use]
    pub(crate) fn policy(&self) -> fleet_core::trust::Policy {
        use fleet_core::trust::{Policy, PrPolicy};
        Policy {
            repository_id: self.repository_id.clone(),
            repository_owner_id: self.repository_owner_id.clone(),
            trusted_refs: self.trusted_refs.clone(),
            allowed_events: self.allowed_events.clone(),
            pr_policy: match self.pr_policy.as_str() {
                "full" => PrPolicy::Full,
                "deny" => PrPolicy::Deny,
                _ => PrPolicy::Restricted,
            },
            job_workflow_ref: self.job_workflow_ref.clone(),
            environment: self.environment.clone(),
        }
    }
}

fn github_issuer() -> String {
    GITHUB_ISSUER.into()
}

fn default_audience() -> String {
    fleet_protocol::brand::OIDC_AUDIENCE.into()
}

fn restricted() -> String {
    "restricted".into()
}

fn string_or_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Id {
        S(String),
        N(u64),
    }
    Ok(match Id::deserialize(d)? {
        Id::S(s) => s,
        Id::N(n) => n.to_string(),
    })
}

impl Config {
    /// Loads the configuration from an optional TOML file and the given environment.
    pub fn load(file: Option<&Path>, env: &[(String, String)]) -> anyhow::Result<Self> {
        let mut figment = Figment::new().merge(Serialized::defaults(Self::default()));
        if let Some(path) = file {
            anyhow::ensure!(path.exists(), "config file {} not found", path.display());
            figment = figment.merge(Toml::file_exact(path));
        }
        let config: Self = figment
            .merge(EnvProvider::new(env)?)
            .extract()
            .context("reading the configuration")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.api.api_key_ttl_days > 0,
            "api.api_key_ttl_days must be above 0"
        );
        if let Some(login) = &self.login {
            anyhow::ensure!(
                login.session_idle_minutes > 0,
                "login.session_idle_minutes must be above 0"
            );
        }
        let i = &self.ingest;
        for (key, value) in [
            ("sample_interval_ms", i.sample_interval_ms),
            ("pressure_sample_interval_ms", i.pressure_sample_interval_ms),
        ] {
            anyhow::ensure!(
                value.is_none_or(|v| (1_000..=300_000).contains(&v)),
                "ingest.{key} must be between 1000 and 300000 milliseconds"
            );
        }
        anyhow::ensure!(
            i.telemetry_flush_ms >= 100,
            "ingest.telemetry_flush_ms must be at least 100"
        );
        anyhow::ensure!(
            i.ping_interval_ms > 0 && i.ping_interval_ms < i.idle_timeout_ms,
            "ingest.ping_interval_ms must be positive and less than ingest.idle_timeout_ms"
        );
        if !(10_000..=30_000).contains(&i.telemetry_flush_ms) {
            tracing::warn!(
                telemetry_flush_ms = i.telemetry_flush_ms,
                "telemetry checkpoints outside 10–30 s: more database load or later acks"
            );
        }
        anyhow::ensure!(
            i.idle_timeout_ms > i.heartbeat_interval_ms,
            "ingest.idle_timeout_ms must exceed ingest.heartbeat_interval_ms"
        );
        if let Some(login) = &self.login {
            anyhow::ensure!(
                self.api.public_url.is_some(),
                "login needs api.public_url: the identity provider redirects back to it"
            );
            anyhow::ensure!(
                !login.group_roles.is_empty(),
                "login.group_roles is empty: nobody could sign in"
            );
            for g in &login.group_roles {
                anyhow::ensure!(
                    g.project
                        .as_ref()
                        .is_none_or(|slug| self.projects.iter().any(|p| &p.slug == slug)),
                    "login.group_roles: group {} names a project that isn't in `projects`",
                    g.group
                );
            }
        }
        for proxy in &self.trusted_proxies {
            anyhow::ensure!(
                proxy.prefix_len() > 0,
                "trusted_proxies: {proxy} trusts every address, so any client could forge its IP"
            );
        }
        self.rate_limits.validate()?;
        self.storage.validate()?;
        self.retention.validate()?;
        self.usercontent.validate(self.api.public_url.as_deref())?;
        for p in &self.projects {
            for t in &p.trust_policies {
                t.check(&self.oidc)
                    .with_context(|| format!("projects.{}.trust_policies.{}", p.slug, t.name))?;
            }
            anyhow::ensure!(
                fleet_store::admin::write::valid_slug(&p.slug),
                "project slug {:?} must be 2–63 of a-z, 0-9 and -",
                p.slug
            );
        }
        Ok(())
    }

    /// The effective configuration as JSON, secrets redacted (`check-config`).
    pub fn redacted(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

/// `FLEET_*` variables from an explicit list, so loading is testable without touching the
/// process environment. `FLEET_X_FILE=/path` sets `x` to the file's contents.
struct EnvProvider {
    dict: Dict,
}

/// Variables that configure the binary, not the server.
const NOT_SETTINGS: &[&str] = &["FLEET_CONFIG", "FLEET_LOG"];

impl EnvProvider {
    fn new(env: &[(String, String)]) -> anyhow::Result<Self> {
        let mut dict = Dict::new();
        for (key, value) in env {
            let Some(name) = key.strip_prefix("FLEET_") else {
                continue;
            };
            if NOT_SETTINGS.contains(&key.as_str()) || name.is_empty() {
                continue;
            }
            let name = name.to_ascii_lowercase();
            let (path, value) = if let Some(stem) = name.strip_suffix("_file") {
                let contents = std::fs::read_to_string(value)
                    .with_context(|| format!("reading {key}={value}"))?;
                // Secrets stay strings, whatever they look like.
                (
                    stem.to_owned(),
                    Value::from(contents.trim_end_matches(['\n', '\r']).to_owned()),
                )
            } else {
                (
                    name,
                    value
                        .parse::<Value>()
                        .unwrap_or_else(|_| Value::from(value.clone())),
                )
            };
            insert(&mut dict, &path.split("__").collect::<Vec<_>>(), value);
        }
        Ok(Self { dict })
    }
}

fn insert(dict: &mut Dict, path: &[&str], value: Value) {
    match path {
        [] => {}
        [last] => {
            dict.insert((*last).to_owned(), value);
        }
        [first, rest @ ..] => {
            let entry = dict
                .entry((*first).to_owned())
                .or_insert_with(|| Value::from(Dict::new()));
            if entry.as_dict().is_none() {
                *entry = Value::from(Dict::new());
            }
            if let Value::Dict(_, inner) = entry {
                insert(inner, rest, value);
            }
        }
    }
}

impl Provider for EnvProvider {
    fn metadata(&self) -> Metadata {
        Metadata::named("FLEET_* environment variable")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        Ok(Profile::Default.collect(self.dict.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn layers_merge_in_order_and_secrets_come_from_files() {
        let dir = std::env::temp_dir().join(format!("fleet-config-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let toml = dir.join("fleet.toml");
        std::fs::write(
            &toml,
            "listen = \"0.0.0.0:9000\"\n[ingest]\nmax_inflight_events = 64\n\
             [[projects]]\nslug = \"app\"\nname = \"App\"\n",
        )
        .unwrap();
        let secret = dir.join("db");
        std::fs::write(&secret, "postgres://u:p@db/fleet\n").unwrap();
        let c = Config::load(
            Some(&toml),
            &env(&[
                ("FLEET_INGEST__MAX_INFLIGHT_EVENTS", "128"),
                ("FLEET_DATABASE__URL_FILE", secret.to_str().unwrap()),
                ("FLEET_LOG", "debug"),
                ("FLEET_JOB_KEY", "ignored by unknown-field tolerance"),
                ("PATH", "/bin"),
            ]),
        )
        .unwrap();
        assert_eq!(c.listen.port(), 9000, "from the file");
        assert_eq!(c.ingest.max_inflight_events, 128, "env beats the file");
        assert_eq!(c.ingest.idle_timeout_ms, 45_000, "default");
        assert_eq!(
            c.database.url.as_ref().map(Secret::expose),
            Some("postgres://u:p@db/fleet")
        );
        assert_eq!(c.projects.len(), 1);
        let shown = c.redacted().unwrap();
        assert!(
            shown.contains("<redacted>") && !shown.contains("u:p@db"),
            "{shown}"
        );
        assert!(!format!("{c:?}").contains("u:p@db"));
    }

    #[test]
    fn ping_interval_must_be_positive_and_below_the_idle_timeout() {
        for ping in ["0", "45000", "45001"] {
            let result = Config::load(None, &env(&[("FLEET_INGEST__PING_INTERVAL_MS", ping)]));
            assert!(result.is_err(), "ping interval {ping} must be rejected");
        }
        assert!(Config::load(None, &env(&[("FLEET_INGEST__PING_INTERVAL_MS", "44999")])).is_ok());
    }

    #[test]
    fn the_sampling_intervals_are_between_1_and_300_seconds() {
        for key in [
            "FLEET_INGEST__SAMPLE_INTERVAL_MS",
            "FLEET_INGEST__PRESSURE_SAMPLE_INTERVAL_MS",
        ] {
            for bad in ["500", "300001"] {
                let err = Config::load(None, &env(&[(key, bad)])).unwrap_err();
                assert!(
                    format!("{err:#}").contains("1000 and 300000"),
                    "{key} {bad}"
                );
            }
            for ok in ["1000", "300000"] {
                assert!(Config::load(None, &env(&[(key, ok)])).is_ok(), "{key} {ok}");
            }
        }
    }

    #[test]
    fn bad_values_name_their_source() {
        let err =
            Config::load(None, &env(&[("FLEET_INGEST__MAX_FRAME_BYTES", "lots")])).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("max_frame_bytes") && msg.contains("FLEET_"),
            "{msg}"
        );
        assert!(Config::load(Some(Path::new("/nonexistent/fleet.toml")), &[]).is_err());
        let bad_slug = Config::load(
            None,
            &env(&[("FLEET_PROJECTS", "[{slug=\"Bad Slug\",name=\"x\"}]")]),
        );
        assert!(bad_slug.is_err());
    }

    #[test]
    fn retention_defaults_follow_the_plan_and_zero_keeps_for_ever() {
        use std::time::Duration;
        let days = |d: u64| Some(Duration::from_secs(d * 86_400));

        // PLAN §9: 7 d of raw samples, 90 d of minutes, 1 y of hours; the journal 14 d.
        let defaults = Config::load(None, &[]).unwrap().retention.policy();
        assert_eq!(defaults.raw_samples, days(7));
        assert_eq!(defaults.rollup_1m, days(90));
        assert_eq!(defaults.rollup_1h, days(365));
        assert_eq!((defaults.journal, defaults.steps), (days(14), days(14)));

        // Everything is configurable; 0 means keep.
        let tuned = Config::load(
            None,
            &env(&[
                ("FLEET_RETENTION__RAW_SAMPLES_DAYS", "3"),
                ("FLEET_RETENTION__ROLLUP_1H_DAYS", "0"),
            ]),
        )
        .unwrap()
        .retention
        .policy();
        assert_eq!(tuned.raw_samples, days(3));
        assert_eq!(tuned.rollup_1h, None);

        let err = Config::load(
            None,
            &env(&[("FLEET_RETENTION__ROLLUP_INTERVAL_SECONDS", "0")]),
        );
        assert!(err.is_err(), "a job that never runs is a mistake");
    }

    #[test]
    fn trusted_proxies_are_cidr_blocks_and_refuse_everything() {
        let c = Config::load(
            None,
            &env(&[("FLEET_TRUSTED_PROXIES", r#"["10.0.0.0/8", "fd00::/8"]"#)]),
        )
        .unwrap();
        assert_eq!(
            c.trusted_proxies,
            [
                "10.0.0.0/8".parse::<IpNet>().unwrap(),
                "fd00::/8".parse().unwrap()
            ]
        );
        assert_eq!(Config::load(None, &[]).unwrap().trusted_proxies.len(), 0);

        // A bare address is not a block: the error names the key.
        let bare =
            Config::load(None, &env(&[("FLEET_TRUSTED_PROXIES", r#"["10.0.0.1"]"#)])).unwrap_err();
        assert!(format!("{bare:#}").contains("trusted_proxies"), "{bare:#}");

        let all =
            Config::load(None, &env(&[("FLEET_TRUSTED_PROXIES", r#"["0.0.0.0/0"]"#)])).unwrap_err();
        assert!(format!("{all:#}").contains("forge"), "{all:#}");
    }

    #[test]
    fn api_keys_expire_in_90_days_unless_configured_otherwise() {
        assert_eq!(Config::load(None, &[]).unwrap().api.api_key_ttl_days, 90);
        let tuned = Config::load(None, &env(&[("FLEET_API__API_KEY_TTL_DAYS", "30")])).unwrap();
        assert_eq!(tuned.api.api_key_ttl_days, 30);
        let zero = Config::load(None, &env(&[("FLEET_API__API_KEY_TTL_DAYS", "0")])).unwrap_err();
        assert!(
            format!("{zero:#}").contains("api.api_key_ttl_days"),
            "{zero:#}"
        );
    }

    #[test]
    fn frames_are_capped_at_256_kib_and_a_project_has_a_daily_quota() {
        // Research 06 T12: the agent batches under this cap, and one project's day is bounded.
        let c = Config::load(None, &[]).unwrap();
        assert_eq!(c.ingest.max_frame_bytes, 256 * 1024);
        assert_eq!(
            c.artifacts.max_project_bytes_per_day,
            20 * 1024 * 1024 * 1024
        );
        assert_eq!(c.artifacts.max_project_files_per_day, 20_000);
    }

    #[test]
    fn rate_limits_default_generously_and_refuse_zero() {
        let d = Config::load(None, &[]).unwrap().rate_limits;
        assert_eq!((d.ip_requests_per_minute, d.ip_burst), (120, 60));
        assert_eq!((d.events_per_second, d.event_burst), (1_000, 5_000));
        assert_eq!((d.viewer_requests_per_minute, d.viewer_burst), (3_000, 600));
        let viewer = Config::load(None, &env(&[("FLEET_RATE_LIMITS__VIEWER_BURST", "7")])).unwrap();
        assert_eq!(viewer.rate_limits.viewer_burst, 7);
        assert_eq!(viewer.rate_limits.viewer_requests_per_minute, 3_000);
        let viewer_zero = Config::load(
            None,
            &env(&[("FLEET_RATE_LIMITS__VIEWER_REQUESTS_PER_MINUTE", "0")]),
        )
        .unwrap_err();
        assert!(
            format!("{viewer_zero:#}").contains("rate_limits.viewer_requests_per_minute"),
            "{viewer_zero:#}"
        );
        let tuned = Config::load(None, &env(&[("FLEET_RATE_LIMITS__IP_BURST", "3")])).unwrap();
        assert_eq!(tuned.rate_limits.ip_burst, 3);
        let zero =
            Config::load(None, &env(&[("FLEET_RATE_LIMITS__EVENTS_PER_SECOND", "0")])).unwrap_err();
        assert!(
            format!("{zero:#}").contains("rate_limits.events_per_second"),
            "{zero:#}"
        );
    }

    #[test]
    fn storage_defaults_to_a_local_directory() {
        let c = Config::load(None, &[]).unwrap();
        assert_eq!(c.storage.backend, StorageBackend::LocalFs);
        assert_eq!(c.storage.path, Path::new("/var/lib/fleet/blobs"));
    }

    #[test]
    fn s3_settings_layer_like_the_rest_and_credentials_stay_secret() {
        let dir = std::env::temp_dir().join(format!("fleet-config-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let toml = dir.join("fleet.toml");
        std::fs::write(
            &toml,
            "[storage]\nbackend = \"s3\"\n[storage.s3]\nendpoint = \"http://garage:3900\"\n\
             bucket = \"fleet-artifacts\"\naccess_key_id = \"GKexample\"\n",
        )
        .unwrap();
        let secret = dir.join("secret_key");
        std::fs::write(&secret, "s3-secret-value\n").unwrap();
        let c = Config::load(
            Some(&toml),
            &env(&[
                (
                    "FLEET_STORAGE__S3__SECRET_ACCESS_KEY_FILE",
                    secret.to_str().unwrap(),
                ),
                ("FLEET_STORAGE__S3__REGION", "eu-1"),
            ]),
        )
        .unwrap();
        let s3 = &c.storage.s3;
        assert_eq!(c.storage.backend, StorageBackend::S3);
        assert_eq!(s3.endpoint.as_deref(), Some("http://garage:3900"));
        assert_eq!(s3.bucket.as_deref(), Some("fleet-artifacts"));
        assert_eq!(s3.region, "eu-1", "env beats the default");
        assert_eq!(
            s3.secret_access_key.as_ref().map(Secret::expose),
            Some("s3-secret-value")
        );
        let shown = format!("{}{c:?}", c.redacted().unwrap());
        assert!(
            !shown.contains("s3-secret-value") && !shown.contains("GKexample"),
            "{shown}"
        );
        c.storage.open().expect("a complete S3 configuration opens");
    }

    #[test]
    fn a_public_endpoint_is_loaded_for_presigned_uploads_and_must_be_a_url() {
        let base = [
            ("FLEET_STORAGE__BACKEND", "s3"),
            ("FLEET_STORAGE__S3__ENDPOINT", "http://garage:3900"),
            ("FLEET_STORAGE__S3__BUCKET", "b"),
            ("FLEET_STORAGE__S3__ACCESS_KEY_ID", "k"),
            ("FLEET_STORAGE__S3__SECRET_ACCESS_KEY", "s"),
        ];
        let mut with = base.to_vec();
        with.push((
            "FLEET_STORAGE__S3__PUBLIC_ENDPOINT",
            "https://files.example.org",
        ));
        let c = Config::load(None, &env(&with)).unwrap();
        assert_eq!(
            c.storage.s3.public_endpoint.as_deref(),
            Some("https://files.example.org")
        );
        c.storage.open().expect("the presigning client opens");

        let mut bad = base.to_vec();
        bad.push((
            "FLEET_STORAGE__S3__PUBLIC_ENDPOINT",
            "ftp://files.example.org",
        ));
        let err = Config::load(None, &env(&bad)).unwrap_err();
        assert!(format!("{err:#}").contains("public_endpoint"), "{err:#}");
    }

    #[test]
    fn an_incomplete_s3_configuration_names_what_is_missing() {
        for (missing, pairs) in [
            ("endpoint", vec![("FLEET_STORAGE__S3__BUCKET", "b")]),
            (
                "bucket",
                vec![("FLEET_STORAGE__S3__ENDPOINT", "http://s3:9000")],
            ),
            (
                "secret_access_key",
                vec![
                    ("FLEET_STORAGE__S3__ENDPOINT", "http://s3:9000"),
                    ("FLEET_STORAGE__S3__BUCKET", "b"),
                    ("FLEET_STORAGE__S3__ACCESS_KEY_ID", "k"),
                ],
            ),
        ] {
            let mut vars = vec![("FLEET_STORAGE__BACKEND", "s3")];
            vars.extend(pairs);
            let err = Config::load(None, &env(&vars)).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains(missing), "{missing}: {msg}");
        }
        let bad_scheme = Config::load(
            None,
            &env(&[
                ("FLEET_STORAGE__BACKEND", "s3"),
                ("FLEET_STORAGE__S3__ENDPOINT", "ftp://s3"),
            ]),
        );
        assert!(format!("{:#}", bad_scheme.unwrap_err()).contains("http"));
    }

    #[test]
    fn a_signed_link_lives_at_most_five_minutes() {
        let key = Secret::from("k".repeat(40).as_str());
        let with_ttl = |link_ttl_seconds| Usercontent {
            url: Some("https://fleet-usercontent.example.net".into()),
            signing_key: Some(key.clone()),
            link_ttl_seconds,
            ..Usercontent::default()
        };
        let public = Some("https://fleet.example.com");
        assert!(with_ttl(300).validate(public).is_ok());
        assert!(with_ttl(301).validate(public).is_err());
        assert!(with_ttl(0).validate(public).is_err());
    }

    #[test]
    fn the_usercontent_origin_needs_its_own_host_and_a_strong_key_from_a_file() {
        let dir = std::env::temp_dir().join(format!("fleet-uc-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key");
        std::fs::write(&key, format!("{}\n", "k".repeat(40))).unwrap();
        let key = key.to_str().unwrap();
        let ok = [
            ("FLEET_API__PUBLIC_URL", "https://fleet.example.com"),
            (
                "FLEET_USERCONTENT__URL",
                "https://fleet-usercontent.example.net",
            ),
            ("FLEET_USERCONTENT__SIGNING_KEY_FILE", key),
        ];
        let c = Config::load(None, &env(&ok)).unwrap();
        assert_eq!(
            c.usercontent.signing_key.as_ref().map(Secret::expose),
            Some("k".repeat(40).as_str()),
            "read from the file, newline trimmed"
        );
        assert_eq!(
            c.usercontent.link_ttl_seconds, 300,
            "research 06 T15: at most 5 minutes"
        );
        assert_eq!(
            c.usercontent.hosts(),
            [
                "fleet-usercontent.example.net:443",
                "fleet-usercontent.example.net"
            ],
            "a Host header may omit the scheme's default port"
        );
        assert!(!c.redacted().unwrap().contains(&"k".repeat(40)));

        let with = |url: &'static str| {
            let mut v = ok.to_vec();
            v.retain(|(k, _)| *k != "FLEET_USERCONTENT__URL");
            v.push(("FLEET_USERCONTENT__URL", url));
            Config::load(None, &env(&v))
        };
        let same = with("https://fleet.example.com").unwrap_err();
        assert!(format!("{same:#}").contains("different host"), "{same:#}");
        let same_with_port = with("https://FLEET.example.com:443").unwrap_err();
        assert!(format!("{same_with_port:#}").contains("different host"));
        let no_key = Config::load(None, &env(&ok[..2])).unwrap_err();
        assert!(format!("{no_key:#}").contains("signing_key"), "{no_key:#}");
        let short = Config::load(
            None,
            &env(&[ok[0], ok[1], ("FLEET_USERCONTENT__SIGNING_KEY", "short")]),
        )
        .unwrap_err();
        assert!(format!("{short:#}").contains("at least 32"), "{short:#}");
        let no_app = Config::load(None, &env(&ok[1..])).unwrap_err();
        assert!(
            format!("{no_app:#}").contains("api.public_url"),
            "{no_app:#}"
        );
    }

    #[tokio::test]
    async fn the_configured_local_store_stores_and_serves_blobs() {
        use fleet_blob::{BlobKey, Sha256Digest};
        use futures_util::StreamExt;

        let dir = std::env::temp_dir().join(format!("fleet-blobs-{}", uuid::Uuid::now_v7()));
        let c = Config::load(
            None,
            &env(&[("FLEET_STORAGE__PATH", dir.to_str().unwrap())]),
        )
        .unwrap();
        let store = c.storage.open().unwrap();
        let key = BlobKey::new(uuid::Uuid::now_v7(), Sha256Digest::of(b"bytes"));
        let body = futures_util::stream::iter([Ok(bytes_of(b"bytes"))]).boxed();
        store.put(&key, 5, body).await.unwrap();
        assert_eq!(store.head(&key).await.unwrap().unwrap().size, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn bytes_of(b: &'static [u8]) -> axum::body::Bytes {
        axum::body::Bytes::from_static(b)
    }
}
