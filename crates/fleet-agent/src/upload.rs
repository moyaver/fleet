// SPDX-License-Identifier: AGPL-3.0-only
//! Artifact uploads (PLAN §6.6; protocol in `fleet_protocol::artifact`).
//!
//! A failure screenshot, a flow's `commands.json`, the JUnit report and `maestro.log` are the
//! classes the agent uploads. Each upload is `upload_request` → grant → `PUT` → `upload_complete`
//! over HTTPS, with:
//!
//! - **at most two uploads at a time**, and a byte-rate cap shared by both ([`Config::rate`]);
//! - the **SHA-256 computed while streaming** the file, never from a copy held in memory;
//! - **fail-open (I2)**: a refused, failed or timed-out upload is reported in the journal and the
//!   log and changes nothing else. Nothing here can touch Maestro's exit status or output;
//! - **no unredacted secret leaves (I6)**: text artifacts are redacted before they are hashed
//!   ([`prepare_text`]), and a text that can't be redacted reliably is not uploaded at all.

use std::{
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use fleet_core::redact::Redactor;
use fleet_protocol::artifact::{ArtifactClass, UploadGrant, UploadRequest, UploadResult};
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncReadExt as _,
    sync::{Notify, Semaphore},
    time::Instant,
};
use uuid::Uuid;

/// Uploads in flight at once (PLAN §6.6).
pub(crate) const CONCURRENCY: usize = 2;
/// Default byte-rate cap across all uploads: 4 MiB/s.
pub(crate) const DEFAULT_RATE: u64 = 4 * 1024 * 1024;
/// Chunk size of a streamed body.
const CHUNK: usize = 64 * 1024;
/// Tries per upload for retryable answers (run not known yet, server busy, network).
const ATTEMPTS: u32 = 8;
/// Longest text artifact the agent will redact in memory (two can be in flight at once, I10).
pub(crate) const MAX_TEXT: u64 = 16 * 1024 * 1024;

/// Where an upload goes and who it speaks as.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    /// The server's `http(s)://` base URL.
    pub(crate) base: String,
    /// The bearer credential, shared with the transport (which may still be exchanging an OIDC
    /// token when the first file appears).
    pub(crate) credential: Arc<Mutex<Option<String>>>,
    /// This agent run.
    pub(crate) agent_run_id: Uuid,
    /// Byte-rate cap, bytes per second.
    pub(crate) rate: u64,
    /// Pause before retrying a retryable answer.
    pub(crate) retry_delay: Duration,
    /// TLS roots.
    pub(crate) tls: Arc<rustls::ClientConfig>,
}

/// What to upload.
#[derive(Debug)]
pub(crate) struct Item {
    pub(crate) class: ArtifactClass,
    /// The path label, relative to the run's output directory (already redacted).
    pub(crate) path: String,
    pub(crate) content_type: &'static str,
    /// The flow it is evidence for.
    pub(crate) flow: Option<Uuid>,
    /// The flow step that produced it, when Maestro names one.
    pub(crate) step: Option<u32>,
    pub(crate) source: Source,
}

/// The bytes.
#[derive(Debug, Clone)]
pub(crate) enum Source {
    /// A binary file (a screenshot), streamed from disk. Binary media is never redacted, only
    /// access-controlled (research 06 T7).
    File(PathBuf),
    /// Bytes already in memory (an already redacted text).
    Bytes(Arc<[u8]>),
    /// A text file: read and redacted when its upload starts, not before, so queued uploads hold
    /// no text in memory. It is uploaded only if it can be redacted reliably ([`prepare_text`]).
    Text(PathBuf, Arc<Redactor>),
}

/// How an upload ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The server has it.
    Uploaded { artifact_id: Uuid, size: u64 },
    /// The server refused it (a `RejectReason` code).
    Rejected(String),
    /// It could not be uploaded (network, server, timeout).
    Failed(String),
}

/// A finished upload and what it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Done {
    pub(crate) class: ArtifactClass,
    pub(crate) path: String,
    pub(crate) flow: Option<Uuid>,
    pub(crate) outcome: Outcome,
}

/// Uploads files in the background; see the module docs.
#[derive(Debug)]
pub(crate) struct Uploader {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    cfg: Config,
    client: reqwest::Client,
    permits: Semaphore,
    limiter: Arc<Limiter>,
    in_flight: AtomicUsize,
    idle: Notify,
    done: Mutex<Vec<Done>>,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

/// A byte-rate cap shared by every upload: each chunk reserves its share of the timeline.
#[derive(Debug)]
struct Limiter {
    rate: u64,
    next: Mutex<Instant>,
}

impl Limiter {
    async fn take(&self, bytes: usize) {
        let wait = {
            let Ok(mut next) = self.next.lock() else {
                return;
            };
            let now = Instant::now();
            let start = (*next).max(now);
            let micros = u64::try_from(bytes)
                .unwrap_or(u64::MAX)
                .saturating_mul(1_000_000)
                / self.rate.max(1);
            *next = start + Duration::from_micros(micros);
            start.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

impl Uploader {
    /// `None` if the HTTP client can't be built (then nothing is uploaded).
    pub(crate) fn new(cfg: Config) -> Option<Self> {
        let direct = crate::net::direct_loopback(&cfg.base);
        let mut builder = reqwest::Client::builder()
            .redirect(crate::net::credential_redirects(direct))
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .tls_backend_preconfigured((*cfg.tls).clone());
        if direct {
            builder = builder.no_proxy();
        }
        let client = builder.build().ok()?;
        Some(Self {
            inner: Arc::new(Inner {
                limiter: Arc::new(Limiter {
                    rate: cfg.rate,
                    next: Mutex::new(Instant::now()),
                }),
                cfg,
                client,
                permits: Semaphore::new(CONCURRENCY),
                in_flight: AtomicUsize::new(0),
                idle: Notify::new(),
                done: Mutex::new(Vec::new()),
                tasks: Mutex::new(Vec::new()),
            }),
        })
    }

    /// Queues an upload; it starts when one of the two slots is free. Needs a running tokio
    /// runtime.
    pub(crate) fn submit(&self, item: Item) {
        self.inner.in_flight.fetch_add(1, Ordering::SeqCst);
        let inner = Arc::clone(&self.inner);
        let task = tokio::spawn(async move {
            let outcome = match inner.permits.acquire().await {
                Ok(_slot) => inner.upload(&item).await,
                Err(_) => Outcome::Failed("the uploader was shut down".into()),
            };
            if let Ok(mut done) = inner.done.lock() {
                done.push(Done {
                    class: item.class,
                    path: item.path,
                    flow: item.flow,
                    outcome,
                });
            }
            if inner.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
                inner.idle.notify_waiters();
            }
        });
        if let Ok(mut tasks) = self.inner.tasks.lock() {
            tasks.retain(|t| !t.is_finished());
            tasks.push(task.abort_handle());
        }
    }

    /// Uploads that finished since the last call.
    pub(crate) fn take_done(&self) -> Vec<Done> {
        self.inner
            .done
            .lock()
            .map(|mut d| std::mem::take(&mut *d))
            .unwrap_or_default()
    }

    /// Waits up to `budget` for every queued upload, then returns everything finished.
    /// Uploads still running after the budget are abandoned.
    pub(crate) async fn finish(&self, budget: Duration) -> Vec<Done> {
        let deadline = Instant::now() + budget;
        loop {
            let idle = self.inner.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self.inner.in_flight.load(Ordering::SeqCst) == 0 {
                break;
            }
            if tokio::time::timeout_at(deadline, idle).await.is_err() {
                tracing::warn!(
                    pending = self.inner.in_flight.load(Ordering::SeqCst),
                    "uploads did not finish in time; abandoning them"
                );
                if let Ok(tasks) = self.inner.tasks.lock() {
                    tasks.iter().for_each(tokio::task::AbortHandle::abort);
                }
                break;
            }
        }
        self.take_done()
    }
}

/// What one request/response round decided.
enum Step {
    Done(Outcome),
    /// Try the whole upload again after a pause.
    Retry(String),
}

impl Inner {
    async fn upload(&self, item: &Item) -> Outcome {
        let source = match &item.source {
            Source::Text(path, redactor) => match read_text(path, redactor).await {
                Ok(bytes) => Source::Bytes(bytes.into()),
                Err(outcome) => return outcome,
            },
            other => other.clone(),
        };
        let source = if item.class == ArtifactClass::Commands {
            let Source::Bytes(bytes) = &source else {
                return Outcome::Rejected(NOT_REDACTABLE.into());
            };
            match redact_commands(bytes) {
                Ok(bytes) => Source::Bytes(bytes.into()),
                Err(reason) => return Outcome::Rejected(reason.into()),
            }
        } else {
            source
        };
        let item = &Item {
            class: item.class,
            path: item.path.clone(),
            content_type: item.content_type,
            flow: item.flow,
            step: item.step,
            source,
        };
        let (size, sha256) = match digest(&item.source).await {
            Ok(d) => d,
            Err(e) => return Outcome::Failed(format!("reading the file: {e}")),
        };
        let request = UploadRequest {
            agent_run_id: self.cfg.agent_run_id,
            flow_execution_id: item.flow,
            step: item.step,
            class: item.class.as_str().to_owned(),
            path: item.path.clone(),
            content_type: item.content_type.to_owned(),
            size,
            sha256,
        };
        let mut why = String::new();
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(self.cfg.retry_delay * 2_u32.pow(attempt.min(4))).await;
            }
            match self.attempt(item, &request).await {
                Step::Done(outcome) => return outcome,
                Step::Retry(reason) => why = reason,
            }
        }
        Outcome::Failed(format!("gave up after {ATTEMPTS} tries: {why}"))
    }

    fn url(&self, relative: &str) -> Option<String> {
        // Only a path on the server we already talk to: a grant must never send the credential
        // somewhere else.
        relative
            .starts_with('/')
            .then(|| format!("{}{relative}", self.cfg.base.trim_end_matches('/')))
    }

    fn token(&self) -> Option<String> {
        self.cfg.credential.lock().ok()?.clone()
    }

    async fn attempt(&self, item: &Item, request: &UploadRequest) -> Step {
        let Some(token) = self.token() else {
            return Step::Retry("no credential yet".into());
        };
        let Some(base) = self.url("/api/v1/agent/artifacts") else {
            return Step::Done(Outcome::Failed("no server URL".into()));
        };
        if let Err(reason) = crate::net::credential_url(&base) {
            return Step::Done(Outcome::Failed(reason.into()));
        }
        let res = match self
            .client
            .post(base)
            .bearer_auth(&token)
            .json(request)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Step::Retry(format!("requesting an upload: {e}")),
        };
        let grant = match classify(res.status()) {
            Reply::Ok => match res.json::<UploadGrant>().await {
                Ok(g) => g,
                Err(e) => return Step::Done(Outcome::Failed(format!("unreadable grant: {e}"))),
            },
            other => return other.step("requesting an upload", res).await,
        };
        match grant {
            UploadGrant::Exists { artifact_id } => Step::Done(Outcome::Uploaded {
                artifact_id,
                size: request.size,
            }),
            UploadGrant::Rejected { reason, .. } => {
                Step::Done(Outcome::Rejected(reason.as_str().to_owned()))
            }
            UploadGrant::Upload {
                artifact_id,
                url,
                complete_url,
                ..
            } => {
                let (Some(put), Some(complete)) = (self.url(&url), self.url(&complete_url)) else {
                    return Step::Done(Outcome::Failed("the grant names another server".into()));
                };
                self.send_and_complete(item, request, artifact_id, &token, &put, &complete)
                    .await
            }
        }
    }

    async fn send_and_complete(
        &self,
        item: &Item,
        request: &UploadRequest,
        artifact_id: Uuid,
        token: &str,
        put: &str,
        complete: &str,
    ) -> Step {
        let res = match self
            .client
            .put(put)
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_LENGTH, request.size)
            .body(body(&item.source, Arc::clone(&self.limiter)))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Step::Retry(format!("sending the bytes: {e}")),
        };
        // 409: the bytes are already there (an earlier try got that far); just complete.
        if res.status() != reqwest::StatusCode::CONFLICT {
            match classify(res.status()) {
                Reply::Ok => {}
                // The grant lapsed: ask for a new one.
                Reply::Gone => return Step::Retry("the grant expired".into()),
                other => return other.step("sending the bytes", res).await,
            }
        }
        let res = match self.client.post(complete).bearer_auth(token).send().await {
            Ok(r) => r,
            Err(e) => return Step::Retry(format!("completing the upload: {e}")),
        };
        match classify(res.status()) {
            Reply::Ok => match res.json::<UploadResult>().await {
                Ok(UploadResult::Available { .. }) => Step::Done(Outcome::Uploaded {
                    artifact_id,
                    size: request.size,
                }),
                Ok(UploadResult::Rejected { reason, .. }) => {
                    Step::Done(Outcome::Rejected(reason.as_str().to_owned()))
                }
                Err(e) => Step::Done(Outcome::Failed(format!("unreadable result: {e}"))),
            },
            other => other.step("completing the upload", res).await,
        }
    }
}

/// The request body: the source in chunks, each paid for at the shared rate.
fn body(source: &Source, limiter: Arc<Limiter>) -> reqwest::Body {
    enum Cursor {
        File(PathBuf, Option<tokio::fs::File>),
        Bytes(Arc<[u8]>, usize),
    }
    let start = match source {
        Source::File(path) | Source::Text(path, _) => Cursor::File(path.clone(), None),
        Source::Bytes(bytes) => Cursor::Bytes(Arc::clone(bytes), 0),
    };
    let stream = futures_util::stream::try_unfold(start, move |mut cursor| {
        let limiter = Arc::clone(&limiter);
        async move {
            let chunk = match &mut cursor {
                Cursor::File(path, file) => {
                    if file.is_none() {
                        *file = Some(tokio::fs::File::open(&*path).await?);
                    }
                    let Some(file) = file.as_mut() else {
                        return Ok::<_, io::Error>(None);
                    };
                    let mut buf = vec![0_u8; CHUNK];
                    let n = file.read(&mut buf).await?;
                    buf.truncate(n);
                    buf
                }
                Cursor::Bytes(bytes, at) => {
                    let end = (*at + CHUNK).min(bytes.len());
                    let chunk = bytes.get(*at..end).unwrap_or_default().to_vec();
                    *at = end;
                    chunk
                }
            };
            if chunk.is_empty() {
                return Ok(None);
            }
            limiter.take(chunk.len()).await;
            Ok(Some((chunk, cursor)))
        }
    });
    reqwest::Body::wrap_stream(stream)
}

/// How the server answered, by status.
enum Reply {
    Ok,
    /// 410: the grant lapsed.
    Gone,
    /// 401 and 403: the credential isn't accepted. Retrying won't help.
    Refused,
    /// 408, 409, 425, 429 and 5xx: not now.
    Later,
    /// Any other status.
    Other,
}

fn classify(status: reqwest::StatusCode) -> Reply {
    match status.as_u16() {
        200..=299 => Reply::Ok,
        410 => Reply::Gone,
        401 | 403 => Reply::Refused,
        408 | 409 | 425 | 429 | 500..=599 => Reply::Later,
        _ => Reply::Other,
    }
}

impl Reply {
    async fn step(self, what: &str, res: reqwest::Response) -> Step {
        let status = res.status();
        match self {
            Self::Later | Self::Gone => Step::Retry(format!("{what}: {status}")),
            Self::Refused => Step::Done(Outcome::Failed(format!(
                "{what}: the server did not accept the credential ({status})"
            ))),
            Self::Ok | Self::Other => {
                let body = res.text().await.unwrap_or_default();
                let body: String = body.chars().take(200).collect();
                Step::Done(Outcome::Failed(format!("{what}: {status} {body}")))
            }
        }
    }
}

/// Lowercase hex, as the protocol writes digests.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Size and SHA-256 of the source, computed in one streaming pass.
async fn digest(source: &Source) -> io::Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;
    match source {
        Source::Bytes(b) => {
            hasher.update(b);
            size = b.len() as u64;
        }
        Source::File(path) | Source::Text(path, _) => {
            let mut file = tokio::fs::File::open(path).await?;
            let mut buf = vec![0_u8; CHUNK];
            loop {
                let n = file.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                hasher.update(buf.get(..n).unwrap_or_default());
                size += n as u64;
            }
        }
    }
    Ok((size, hex(&hasher.finalize())))
}

/// Reads a text file and redacts it, or the outcome that says it isn't uploaded.
async fn read_text(path: &PathBuf, redactor: &Redactor) -> Result<Vec<u8>, Outcome> {
    let too_big = |len: u64| len > MAX_TEXT;
    let bytes = match tokio::fs::metadata(path).await {
        Ok(m) if too_big(m.len()) => None,
        Ok(_) => Some(tokio::fs::read(path).await),
        Err(e) => return Err(Outcome::Failed(format!("reading the file: {e}"))),
    };
    let bytes = match bytes {
        Some(Ok(b)) => b,
        Some(Err(e)) => return Err(Outcome::Failed(format!("reading the file: {e}"))),
        None => return Err(Outcome::Rejected(NOT_REDACTABLE.into())),
    };
    prepare_text(&bytes, redactor).map_err(|code| Outcome::Rejected(code.into()))
}

// Keep the captured command metadata useful without exporting resolved values from a flow.
fn redact_commands(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut commands: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| NOT_REDACTABLE)?;
    if !commands.is_array() {
        return Err(NOT_REDACTABLE);
    }
    hide_command_values(&mut commands);
    serde_json::to_vec(&commands).map_err(|_| NOT_REDACTABLE)
}

fn hide_command_values(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(
                    key.as_str(),
                    "evaluatedCommand" | "env" | "inputTextCommand"
                ) {
                    *value = fleet_core::redact::MARKER.into();
                } else {
                    hide_command_values(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                hide_command_values(value);
            }
        }
        _ => {}
    }
}

/// The reason code for a text the agent will not upload.
pub(crate) const NOT_REDACTABLE: &str = "not_redactable";

/// Redacts a text artifact for upload, or says why it can't be uploaded. A text is uploaded only
/// if it is valid UTF-8 without NUL bytes and small enough to hold in memory: anything else
/// can't be redacted *reliably* (an encoding the redactor doesn't match, or a size it can't
/// scan), and an unredacted secret must never leave the runner (I6).
pub(crate) fn prepare_text(bytes: &[u8], redactor: &Redactor) -> Result<Vec<u8>, &'static str> {
    if bytes.len() as u64 > MAX_TEXT || bytes.contains(&0) {
        return Err(NOT_REDACTABLE);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| NOT_REDACTABLE)?;
    Ok(redactor.redact(text).into_owned().into_bytes())
}

#[cfg(test)]
mod tests {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;

    /// SHA-256 of `hello` (FIPS 180-4 / RFC 6234 test vector).
    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    const RUN: Uuid = Uuid::from_u128(0x0192_0000_0000_7000_8000_0000_0000_0001);

    fn config(server: &MockServer) -> Config {
        config_for(server.uri())
    }

    fn config_for(base: String) -> Config {
        Config {
            base,
            credential: Arc::new(Mutex::new(Some("mf_agent_secret".into()))),
            agent_run_id: RUN,
            rate: 1 << 30,
            retry_delay: Duration::from_millis(10),
            tls: crate::net::tls(None),
        }
    }

    fn item(bytes: &[u8]) -> Item {
        Item {
            class: ArtifactClass::MaestroLog,
            path: "maestro.log".into(),
            content_type: "text/plain",
            flow: None,
            step: None,
            source: Source::Bytes(bytes.to_vec().into()),
        }
    }

    fn grant(artifact: Uuid) -> serde_json::Value {
        serde_json::json!({
            "disposition": "upload",
            "artifact_id": artifact,
            "url": format!("/api/v1/agent/artifacts/{artifact}/content"),
            "complete_url": format!("/api/v1/agent/artifacts/{artifact}/complete"),
            "expires_at": "2030-01-01T00:00:00Z",
            "size": 5,
        })
    }

    async fn run(up: &Uploader) -> Vec<Done> {
        up.finish(Duration::from_secs(10)).await
    }

    #[tokio::test]
    async fn remote_cleartext_artifact_upload_is_refused() {
        let up = Uploader::new(config_for("http://192.0.2.1".into())).unwrap();
        up.submit(item(b"hello"));
        let done = run(&up).await;
        assert!(
            matches!(&done[0].outcome, Outcome::Failed(reason) if reason.contains("credentials require")),
            "{done:?}"
        );
    }

    #[tokio::test]
    async fn a_file_is_requested_with_its_digest_put_whole_and_completed() {
        let server = MockServer::start().await;
        let artifact = Uuid::now_v7();
        Mock::given(method("POST"))
            .and(path("/api/v1/agent/artifacts"))
            .respond_with(ResponseTemplate::new(201).set_body_json(grant(artifact)))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("/api/v1/agent/artifacts/{artifact}/content")))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/api/v1/agent/artifacts/{artifact}/complete")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"status": "available", "artifact_id": artifact}),
                ),
            )
            .mount(&server)
            .await;
        let up = Uploader::new(config(&server)).unwrap();

        up.submit(item(b"hello"));
        let done = run(&up).await;

        assert_eq!(
            done,
            [Done {
                class: ArtifactClass::MaestroLog,
                path: "maestro.log".into(),
                flow: None,
                outcome: Outcome::Uploaded {
                    artifact_id: artifact,
                    size: 5
                },
            }]
        );
        let seen = server.received_requests().await.unwrap();
        let request: UploadRequest = serde_json::from_slice(&seen[0].body).unwrap();
        assert_eq!(request.sha256, HELLO_SHA);
        assert_eq!(request.size, 5);
        assert_eq!(request.class, "maestro_log");
        assert_eq!(request.path, "maestro.log");
        assert_eq!(request.content_type, "text/plain");
        assert_eq!(request.agent_run_id, RUN);
        assert_eq!(seen[1].body, b"hello", "the bytes, whole");
        for r in &seen {
            assert_eq!(
                r.headers.get("authorization").map(|v| v.to_str().unwrap()),
                Some("Bearer mf_agent_secret")
            );
        }
        assert_eq!(seen[1].headers.get("content-length").unwrap(), "5");
        assert_eq!(seen.len(), 3);
    }

    #[tokio::test]
    async fn a_file_the_server_already_has_is_not_sent() {
        let server = MockServer::start().await;
        let artifact = Uuid::now_v7();
        Mock::given(method("POST"))
            .and(path("/api/v1/agent/artifacts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"disposition": "exists", "artifact_id": artifact}),
            ))
            .mount(&server)
            .await;
        let up = Uploader::new(config(&server)).unwrap();

        up.submit(item(b"hello"));
        let done = run(&up).await;

        assert_eq!(
            done[0].outcome,
            Outcome::Uploaded {
                artifact_id: artifact,
                size: 5
            }
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1, "no PUT");
    }

    #[tokio::test]
    async fn a_refusal_is_reported_with_its_reason_and_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/agent/artifacts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "disposition": "rejected", "reason": "too_large", "message": "too big"
            })))
            .mount(&server)
            .await;
        let up = Uploader::new(config(&server)).unwrap();

        up.submit(item(b"hello"));
        let done = run(&up).await;

        assert_eq!(done[0].outcome, Outcome::Rejected("too_large".into()));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn verification_that_fails_after_the_bytes_were_sent_is_a_rejection() {
        let server = MockServer::start().await;
        let artifact = Uuid::now_v7();
        Mock::given(method("POST"))
            .and(path("/api/v1/agent/artifacts"))
            .respond_with(ResponseTemplate::new(201).set_body_json(grant(artifact)))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/api/v1/agent/artifacts/{artifact}/complete")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "rejected", "artifact_id": artifact,
                "reason": "type_not_allowed", "message": "no"
            })))
            .mount(&server)
            .await;
        let up = Uploader::new(config(&server)).unwrap();

        up.submit(item(b"hello"));

        assert_eq!(
            run(&up).await[0].outcome,
            Outcome::Rejected("type_not_allowed".into())
        );
    }

    /// A minimal HTTP/1.1 server whose handler counts requests in flight, so the count
    /// covers exactly each request's lifetime as the server sees it.
    async fn serve_slow(
        now: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        artifact: Uuid,
    ) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (now, peak) = (Arc::clone(&now), Arc::clone(&peak));
                tokio::spawn(async move {
                    // Read the head and the whole body so the client never sees a reset.
                    let mut buf = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    let (head_len, body_len) = loop {
                        let n = stream.read(&mut chunk).await.unwrap();
                        assert!(n > 0, "client closed before finishing the request");
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                            break (end + 4, len);
                        }
                    };
                    while buf.len() < head_len + body_len {
                        let n = stream.read(&mut chunk).await.unwrap();
                        assert!(n > 0, "client closed mid-body");
                        buf.extend_from_slice(&chunk[..n]);
                    }

                    let in_flight = now.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(in_flight, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    now.fetch_sub(1, Ordering::SeqCst);

                    let body = serde_json::json!({
                        "disposition": "exists", "artifact_id": artifact
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn at_most_two_uploads_run_at_a_time() {
        let (now, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let addr = serve_slow(Arc::clone(&now), Arc::clone(&peak), Uuid::now_v7()).await;
        let up = Uploader::new(config_for(format!("http://{addr}"))).unwrap();

        for n in 0..6_u8 {
            up.submit(item(&[n; 8]));
        }
        let done = run(&up).await;

        assert_eq!(done.len(), 6);
        assert!(
            done.iter()
                .all(|d| matches!(d.outcome, Outcome::Uploaded { .. }))
        );
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "never more than two at once"
        );
    }

    #[tokio::test]
    async fn the_rate_cap_slows_the_bytes_down() {
        let server = MockServer::start().await;
        let artifact = Uuid::now_v7();
        Mock::given(method("POST"))
            .and(path("/api/v1/agent/artifacts"))
            .respond_with(ResponseTemplate::new(201).set_body_json(grant(artifact)))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/api/v1/agent/artifacts/{artifact}/complete")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"status": "available", "artifact_id": artifact}),
                ),
            )
            .mount(&server)
            .await;
        // 200 KiB at 100 KiB/s cannot finish in under 1.5 s (the first chunk is free).
        let mut cfg = config(&server);
        cfg.rate = 100 * 1024;
        let up = Uploader::new(cfg).unwrap();

        let started = Instant::now();
        up.submit(item(&vec![b'x'; 200 * 1024]));
        let done = run(&up).await;

        assert!(
            matches!(done[0].outcome, Outcome::Uploaded { .. }),
            "{done:?}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_server_that_is_down_fails_the_upload_without_a_panic() {
        // Nothing listens here: connection refused, every time.
        let server = MockServer::start().await;
        let mut cfg = config(&server);
        cfg.base = "http://127.0.0.1:1".into();
        let up = Uploader::new(cfg).unwrap();

        up.submit(item(b"hello"));
        let done = up.finish(Duration::from_secs(30)).await;

        assert!(matches!(done[0].outcome, Outcome::Failed(_)), "{done:?}");
    }

    #[tokio::test]
    async fn a_run_the_server_does_not_know_yet_is_retried_until_it_does() {
        let server = MockServer::start().await;
        let artifact = Uuid::now_v7();
        // The first two answers: the run has not connected yet.
        Mock::given(method("POST"))
            .and(path("/api/v1/agent/artifacts"))
            .respond_with(ResponseTemplate::new(409).set_body_json(
                serde_json::json!({"error": "unknown_run: the run has not connected yet"}),
            ))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/agent/artifacts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"disposition": "exists", "artifact_id": artifact}),
            ))
            .mount(&server)
            .await;
        let up = Uploader::new(config(&server)).unwrap();

        up.submit(item(b"hello"));

        assert!(matches!(
            run(&up).await[0].outcome,
            Outcome::Uploaded { .. }
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_file_is_streamed_from_disk_and_hashed_on_the_way() {
        let server = MockServer::start().await;
        let artifact = Uuid::now_v7();
        Mock::given(method("POST"))
            .and(path("/api/v1/agent/artifacts"))
            .respond_with(ResponseTemplate::new(201).set_body_json(grant(artifact)))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/api/v1/agent/artifacts/{artifact}/complete")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"status": "available", "artifact_id": artifact}),
                ),
            )
            .mount(&server)
            .await;
        let dir = std::env::temp_dir().join(format!("fleet-upload-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("shot.png");
        // Bigger than one chunk, so the hash really is computed across several reads.
        let bytes: Vec<u8> = (0..300_000_u32).map(|n| (n % 251) as u8).collect();
        std::fs::write(&file, &bytes).unwrap();
        let up = Uploader::new(config(&server)).unwrap();

        up.submit(Item {
            class: ArtifactClass::FailureScreenshot,
            path: "login/screenshots/shot.png".into(),
            content_type: "image/png",
            flow: Some(Uuid::now_v7()),
            step: Some(2),
            source: Source::File(file),
        });
        let done = run(&up).await;

        assert!(
            matches!(done[0].outcome, Outcome::Uploaded { size: 300_000, .. }),
            "{done:?}"
        );
        let seen = server.received_requests().await.unwrap();
        let request: UploadRequest = serde_json::from_slice(&seen[0].body).unwrap();
        assert_eq!(request.sha256, hex(&Sha256::digest(&bytes)));
        assert_eq!(seen[1].body, bytes);
        assert!(request.flow_execution_id.is_some());
        assert_eq!(request.step, Some(2), "the step rides on the request");
    }

    /// A server that accepts everything and records what it was sent.
    async fn accepting() -> (MockServer, Uuid) {
        let server = MockServer::start().await;
        let artifact = Uuid::now_v7();
        Mock::given(method("POST"))
            .and(path("/api/v1/agent/artifacts"))
            .respond_with(ResponseTemplate::new(201).set_body_json(grant(artifact)))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/api/v1/agent/artifacts/{artifact}/complete")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"status": "available", "artifact_id": artifact}),
                ),
            )
            .mount(&server)
            .await;
        (server, artifact)
    }

    fn text_file(bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fleet-upload-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("maestro.log");
        std::fs::write(&file, bytes).unwrap();
        file
    }

    fn text_item(file: PathBuf, redactor: Redactor) -> Item {
        Item {
            source: Source::Text(file, Arc::new(redactor)),
            ..item(b"")
        }
    }

    #[tokio::test]
    async fn a_text_is_redacted_before_it_is_hashed_and_sent_so_no_secret_leaves() {
        let (server, _) = accepting().await;
        let log = "ok\nlogin with hunter2-secret\n{\"password\":\"hunter2-secret\"}\n";
        let file = text_file(log.as_bytes());
        let up = Uploader::new(config(&server)).unwrap();

        up.submit(text_item(
            file,
            Redactor::new(["hunter2-secret".to_owned()]),
        ));
        let done = run(&up).await;

        assert!(
            matches!(done[0].outcome, Outcome::Uploaded { .. }),
            "{done:?}"
        );
        let seen = server.received_requests().await.unwrap();
        let sent = String::from_utf8(seen[1].body.clone()).unwrap();
        assert_eq!(
            sent, "ok\nlogin with [REDACTED]\n{\"password\":\"[REDACTED]\"}\n",
            "a planted secret, plain and JSON-escaped, never leaves the runner (I6)"
        );
        // The request describes what was sent: the redacted bytes, not the file on disk.
        let request: UploadRequest = serde_json::from_slice(&seen[0].body).unwrap();
        assert_eq!(request.size, sent.len() as u64);
        assert_eq!(request.sha256, hex(&Sha256::digest(sent.as_bytes())));
        for r in &seen {
            assert!(!String::from_utf8_lossy(&r.body).contains("hunter2-secret"));
        }
    }

    #[tokio::test]
    async fn captured_commands_hide_resolved_values_without_knowing_their_secrets() {
        let (server, _) = accepting().await;
        let fixture = include_bytes!(
            "../../../fixtures/maestro/2.12.1/features/run/debug/features/commands.json"
        );
        let up = Uploader::new(config(&server)).unwrap();
        let mut item = text_item(text_file(fixture), Redactor::new([]));
        item.class = ArtifactClass::Commands;
        item.path = "commands.json".into();
        item.content_type = "application/json";
        up.submit(item);
        let done = run(&up).await;
        assert!(matches!(done[0].outcome, Outcome::Uploaded { .. }));
        let requests = server.received_requests().await.unwrap();
        let sent = String::from_utf8(requests[1].body.clone()).unwrap();
        assert!(!sent.contains("fleet-planted-e-secret-7f3a"));
        assert!(!sent.contains("fleet-planted-maestro-secret-91c2"));
        let rows: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(rows[0]["metadata"]["status"], "COMPLETED");
        assert_eq!(rows[0]["metadata"]["sequenceNumber"], 0);
    }

    #[tokio::test]
    async fn a_text_that_cannot_be_redacted_reliably_is_never_sent() {
        let (server, _) = accepting().await;
        let up = Uploader::new(config(&server)).unwrap();

        // Not UTF-8 (a secret in another encoding would slip past), and NUL bytes (binary).
        for bytes in [&[0xff, 0xfe, b'a'][..], b"a\0b"] {
            up.submit(text_item(
                text_file(bytes),
                Redactor::new(["hunter2-secret".to_owned()]),
            ));
        }
        let done = run(&up).await;

        assert_eq!(done.len(), 2);
        assert!(
            done.iter()
                .all(|d| d.outcome == Outcome::Rejected(NOT_REDACTABLE.into()))
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            0,
            "not even asked about"
        );
    }
}
