// SPDX-License-Identifier: AGPL-3.0-only
//! The agent ↔ server connection (research 05 §3.3–§3.8; invariants I2, I5).
//!
//! `hello` → `welcome{committed_through}` → drop what's committed, resend the rest in order, then
//! stream contiguous batches and drop events as cumulative acks arrive. The server decides where
//! a resumed run continues. Any failure only reconnects (full-jitter backoff) or, if the server
//! refuses the run for good, stops telemetry: nothing here can affect Maestro.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use fleet_protocol::{
    PROTOCOL_VERSION,
    brand::WS_SUBPROTOCOL,
    frame::{
        AgentInfo, BatchRequest, BatchResponse, CiContext, ClientFrame, ErrorCode, Hello, Limits,
        ProtocolRange, ResumeState, RunOrigin, ServerFrame, Telemetry, Welcome,
    },
    journal::ClockSample,
};
use futures_util::{SinkExt, StreamExt};
use time::OffsetDateTime;
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};
use uuid::Uuid;

use crate::{
    metrics::{Cadence, clamp_interval_ms},
    outbox::Outbox,
};

/// Where and how to connect.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    /// `ws://` or `wss://` URL of the agent endpoint.
    pub(crate) url: String,
    /// Bearer token (`FLEET_TOKEN`).
    pub(crate) token: Option<String>,
    /// Without a token: the GitHub OIDC token to exchange for one.
    pub(crate) oidc: Option<crate::net::OidcRequest>,
    /// TLS roots (Mozilla, OS, `FLEET_CA_FILE`).
    pub(crate) tls: Arc<rustls::ClientConfig>,
    /// The proxy for `url`, if any.
    pub(crate) proxy: Option<crate::net::Proxy>,
    /// This agent run.
    pub(crate) agent_run_id: Uuid,
    /// CI identity.
    pub(crate) ci: CiContext,
    /// Where the run's evidence came from: [`RunOrigin::Live`] unless this is a publish.
    pub(crate) origin: RunOrigin,
    /// When the run started, as its recorded events say (a publish only; see `Hello::started_at`).
    pub(crate) started_at: Option<OffsetDateTime>,
    /// How long the server may send nothing before the connection is given up for a new one.
    pub(crate) silence_timeout: Duration,
    /// The workspace's flows, found before the run (issue #85); sent in `hello`.
    pub(crate) catalog: fleet_protocol::frame::FlowCatalog,
}

impl Config {
    /// From the environment: `FLEET_TOKEN`, GitHub's OIDC variables, `FLEET_CA_FILE` and the
    /// proxy variables.
    pub(crate) fn from_env(
        url: String,
        agent_run_id: Uuid,
        ci: CiContext,
        env: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let proxy = crate::net::Target::parse(&url).and_then(|t| crate::net::proxy_for(&t, &env));
        if let Some(p) = &proxy {
            tracing::info!(proxy = ?p, "connecting through a proxy");
        }
        Self {
            token: env("FLEET_TOKEN").filter(|t| !t.is_empty()),
            oidc: crate::net::OidcRequest::from_env(&env),
            tls: crate::net::tls(env("FLEET_CA_FILE").as_deref()),
            proxy,
            url,
            agent_run_id,
            ci,
            origin: RunOrigin::Live,
            started_at: None,
            silence_timeout: SILENCE_TIMEOUT,
            catalog: fleet_protocol::frame::FlowCatalog::default(),
        }
    }
}

/// Shared between the observer and the transport task.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    pub(crate) outbox: Mutex<Outbox>,
    /// New events were pushed.
    pub(crate) wake: Notify,
    /// The run is over: flush what's left, then say `bye`.
    pub(crate) finishing: std::sync::atomic::AtomicBool,
    /// The server answered during this run: a WebSocket upgrade or a batch response, even if its
    /// welcome is still to come.
    pub(crate) connected: std::sync::atomic::AtomicBool,
    /// The bearer credential in use (`FLEET_TOKEN`, or the token the OIDC exchange returned),
    /// for the artifact uploader, which speaks as the same agent.
    pub(crate) credential: Arc<Mutex<Option<String>>>,
    /// The server's calm sampling interval in ms, clamped; 0 when the welcome did not set one.
    pub(crate) sample_interval_ms: std::sync::atomic::AtomicU64,
    /// The server's sampling interval under pressure in ms, clamped; 0 when absent.
    pub(crate) pressure_sample_interval_ms: std::sync::atomic::AtomicU64,
    /// Busy refusals in a row: each one lengthens the next wait, and an ack ends the streak.
    pub(crate) overloads: std::sync::atomic::AtomicU32,
}

impl Shared {
    /// The sampling intervals in force: the server's, or the defaults while it has not set them.
    pub(crate) fn cadence(&self) -> Cadence {
        Cadence::from_stored(
            self.sample_interval_ms
                .load(std::sync::atomic::Ordering::SeqCst),
            self.pressure_sample_interval_ms
                .load(std::sync::atomic::Ordering::SeqCst),
        )
    }
}

/// Stores the sampling intervals of a welcome for the observer. A value outside [1 s, 300 s] is
/// clamped and logged once per welcome.
fn share_intervals(shared: &Shared, limits: &Limits) {
    store_interval(
        &shared.sample_interval_ms,
        "sample_interval_ms",
        limits.sample_interval_ms,
    );
    store_interval(
        &shared.pressure_sample_interval_ms,
        "pressure_sample_interval_ms",
        limits.pressure_sample_interval_ms,
    );
}

fn store_interval(slot: &std::sync::atomic::AtomicU64, setting: &'static str, ms: Option<u64>) {
    let stored = ms.map_or(0, |ms| {
        let clamped = clamp_interval_ms(ms);
        if clamped != ms {
            tracing::warn!(
                setting,
                clamped_to_ms = clamped,
                "server sampling interval clamped"
            );
        }
        clamped
    });
    slot.store(stored, std::sync::atomic::Ordering::SeqCst);
}

/// How the transport ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum End {
    /// Everything was acked and `bye` sent.
    Flushed,
    /// The server refused the run for good; telemetry is off.
    Refused(String),
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const SILENCE_TIMEOUT: Duration = Duration::from_secs(30);
const PING_INTERVAL: Duration = Duration::from_secs(10);
const BATCH: usize = 64;
/// Room kept in every frame for its envelope (`op`, the array brackets and separators): the
/// events fill the rest of the server's `max_frame_bytes`.
const FRAME_RESERVE: usize = 1024;
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// The wait after a busy refusal: 200 ms, doubling, to 5 s (see [`overload_ceiling`]).
const OVERLOAD_BASE: Duration = Duration::from_millis(200);
const OVERLOAD_CAP: Duration = Duration::from_secs(5);

/// Serialized event bytes one frame may carry under the server's `max_frame_bytes`, less the
/// batch's other parts (`fixed`, such as the hello and telemetry of a batch request).
fn event_budget(max_frame_bytes: u32, fixed: usize) -> usize {
    usize::try_from(max_frame_bytes)
        .unwrap_or(usize::MAX)
        .saturating_sub(FRAME_RESERVE + fixed)
}

/// Runs until everything is flushed after `finishing`, or the server refuses the run.
/// The caller bounds it with a deadline.
pub(crate) async fn run(
    cfg: Config,
    shared: Arc<Shared>,
    mut telemetry: mpsc::Receiver<Telemetry>,
    clock: impl Fn() -> ClockSample,
) -> End {
    let mut attempt: u32 = 0;
    let mut upgrade_failures = 0_u32;
    let mut cfg = cfg;
    if (cfg.token.is_some() || cfg.oidc.is_some())
        && let Err(reason) = crate::net::credential_url(&cfg.url)
    {
        tracing::warn!(
            reason,
            "telemetry disabled; configure a secure FLEET_SERVER URL"
        );
        return End::Refused(reason.into());
    }
    if cfg.token.is_none()
        && let Some(req) = &cfg.oidc
        && let Err(reason) = crate::net::credential_url(&req.url)
    {
        tracing::warn!(reason, "telemetry disabled; configure a secure OIDC URL");
        return End::Refused(reason.into());
    }
    if cfg.token.is_none() && cfg.oidc.is_none() {
        // Never an unauthenticated connection: the hello would carry the run's CI facts to a
        // server that refuses them anyway (research 06 T1).
        tracing::warn!(
            "no FLEET_TOKEN and no OIDC token request: running offline, nothing is sent"
        );
        return End::Refused("no credential".into());
    }
    share(&shared, cfg.token.as_ref());
    loop {
        if cfg.token.is_none()
            && let Some(req) = &cfg.oidc
        {
            match crate::net::exchange(req, &cfg.url, Arc::clone(&cfg.tls)).await {
                Ok(t) => {
                    tracing::info!("exchanged the job's OIDC token for an agent token");
                    cfg.token = Some(t);
                    share(&shared, cfg.token.as_ref());
                }
                Err(e) => {
                    tracing::warn!(error = %e, attempt, "OIDC token exchange failed");
                    attempt = attempt.saturating_add(1);
                    tokio::time::sleep(pause(attempt, retry_after_of(e.as_ref()))).await;
                    continue;
                }
            }
        }
        let server_wait = match connection(&cfg, &shared, &mut telemetry, &clock).await {
            Ok(end) => return end,
            Err(e) => {
                if upgrade_refused(e.as_ref()) {
                    upgrade_failures += 1;
                    if upgrade_failures >= 3 {
                        tracing::warn!("websocket upgrades refused; switching to batch transport");
                        match batch(&cfg, &shared, &mut telemetry, &clock).await {
                            Ok(end) => return end,
                            Err(()) => {
                                upgrade_failures = 0;
                            }
                        }
                    }
                } else {
                    upgrade_failures = 0;
                }
                tracing::warn!(error = %e, attempt, "agent connection lost");
                retry_after_of(e.as_ref())
            }
        };
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(pause(attempt, server_wait)).await;
    }
}

/// How long to wait before trying again: the jittered backoff, or the server's `Retry-After`
/// when that is longer.
fn pause(attempt: u32, server: Option<Duration>) -> Duration {
    backoff(attempt).max(server.unwrap_or_default())
}

/// The `Retry-After` of a refusal: from the token exchange, or from an upgrade the server
/// answered with `429`.
fn retry_after_of(error: &(dyn std::error::Error + Send + Sync + 'static)) -> Option<Duration> {
    if let Some(limited) = error.downcast_ref::<crate::net::RateLimited>() {
        return Some(limited.retry_after);
    }
    match error.downcast_ref::<tokio_tungstenite::tungstenite::Error>() {
        Some(tokio_tungstenite::tungstenite::Error::Http(response)) if response.status() == 429 => {
            Some(crate::net::retry_after(response.headers()))
        }
        _ => None,
    }
}

/// Only explicit upgrade-incompatible HTTP responses trigger fallback. Authentication failures
/// and network errors continue on the normal WebSocket path.
fn upgrade_refused(error: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    matches!(error.downcast_ref::<tokio_tungstenite::tungstenite::Error>(),
        Some(tokio_tungstenite::tungstenite::Error::Http(response))
            if matches!(response.status().as_u16(), 400 | 404 | 405 | 426 | 501))
}

#[expect(
    clippy::too_many_lines,
    reason = "the batch loop keeps retry and response state together"
)]
async fn batch(
    cfg: &Config,
    shared: &Shared,
    telemetry: &mut mpsc::Receiver<Telemetry>,
    clock: &impl Fn() -> ClockSample,
) -> Result<End, ()> {
    let url = format!("{}/api/v1/agent/batch", crate::net::http_base(&cfg.url));
    if cfg.token.is_some()
        && let Err(reason) = crate::net::credential_url(&url)
    {
        return Ok(End::Refused(reason.into()));
    }
    let direct = crate::net::direct_loopback(&url);
    let mut builder = reqwest::Client::builder()
        .timeout(CONNECT_TIMEOUT)
        .redirect(crate::net::credential_redirects(direct))
        .tls_backend_preconfigured((*cfg.tls).clone());
    if direct {
        builder = builder.no_proxy();
    }
    let Ok(client) = builder.build() else {
        return Ok(End::Refused("cannot build batch client".into()));
    };
    let mut cursor: Option<u64> = None;
    let mut max_events = BATCH;
    let mut max_frame_bytes = 0_u32;
    let mut failures = 0_u32;
    let mut samples: Vec<Telemetry> = Vec::new();
    loop {
        if failures >= 5 {
            return Err(());
        }
        let hello = match hello_frame(cfg, shared, clock) {
            Ok(ClientFrame::Hello(hello)) => *hello,
            _ => return Ok(End::Refused("cannot build batch hello".into())),
        };
        while samples.len() < BATCH {
            match telemetry.try_recv() {
                Ok(sample) => samples.push(sample),
                Err(_) => break,
            }
        }
        let events = if let Some(after) = cursor {
            // The events fill what the hello and telemetry leave of the frame (research 06 T12).
            let fixed = serde_json::to_vec(&hello).map_or(0, |v| v.len())
                + samples
                    .iter()
                    .map(|s| serde_json::to_vec(s).map_or(0, |v| v.len() + 1))
                    .sum::<usize>();
            let budget = event_budget(max_frame_bytes, fixed);
            match shared.outbox.lock() {
                Ok(mut o) => o.after(after, max_events, budget),
                Err(_) => return Ok(End::Refused("outbox lock".into())),
            }
        } else {
            Vec::new()
        };
        let sent_events = !events.is_empty();
        let before_ack = shared.outbox.lock().map_or(0, |o| o.acked_through());
        let mut post = client.post(&url).json(&BatchRequest {
            hello,
            events,
            telemetry: samples.clone(),
        });
        if let Some(token) = &cfg.token {
            post = post.bearer_auth(token);
        }
        let response = match post.send().await {
            Ok(response)
                if response.status() == reqwest::StatusCode::UNAUTHORIZED
                    || response.status() == reqwest::StatusCode::FORBIDDEN =>
            {
                return Ok(End::Refused("batch authentication refused".into()));
            }
            Ok(response) if response.status().is_success() => {
                shared
                    .connected
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                response.json::<BatchResponse>().await
            }
            // Rate limited: wait as the server asks and send again. Not a failure of the link,
            // so it does not count towards giving up on batch.
            Ok(response) if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                let wait = pause(1, Some(crate::net::retry_after(response.headers())));
                tracing::warn!(wait_ms = wait.as_millis(), "batch rate limited; waiting");
                tokio::time::sleep(wait).await;
                continue;
            }
            Ok(_) | Err(_) => {
                failures = failures.saturating_add(1);
                tokio::time::sleep(backoff(failures)).await;
                continue;
            }
        };
        let Ok(response) = response else {
            failures = failures.saturating_add(1);
            tokio::time::sleep(backoff(failures)).await;
            continue;
        };
        let mut seen_welcome = false;
        let mut sent = cursor.unwrap_or(0);
        let mut failed = false;
        let mut overloaded = false;
        for frame in response.frames {
            match frame {
                ServerFrame::Welcome(welcome) => {
                    let acked = shared.outbox.lock().map_or(0, |o| o.acked_through());
                    if let Err(reason) = validate_welcome(&welcome, acked) {
                        return Ok(End::Refused(reason.into()));
                    }
                    seen_welcome = true;
                    shared
                        .connected
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    max_events =
                        BATCH.min(usize::try_from(welcome.limits.max_inflight_events).unwrap_or(0));
                    max_frame_bytes = welcome.limits.max_frame_bytes;
                    share_intervals(shared, &welcome.limits);
                    sent = welcome.committed_through;
                    if let Ok(mut o) = shared.outbox.lock() {
                        o.ack(sent);
                    }
                }
                ServerFrame::Reject {
                    fatal: true,
                    code,
                    message,
                    ..
                } => return Ok(End::Refused(format!("{code:?}: {message}"))),
                ServerFrame::Reject { .. } => failed = true,
                ServerFrame::TelemetryAck { through } => {
                    samples.retain(|sample| sample.tseq_to > through);
                }
                other => {
                    // Busy is the server's answer, not a broken link: it is not counted towards
                    // giving up on the batch transport.
                    overloaded |= matches!(
                        other,
                        ServerFrame::Error {
                            code: ErrorCode::Overloaded,
                            fatal: false,
                            ..
                        }
                    );
                    match on_frame(Some(other), shared, &mut sent).await {
                        Ok(Some(end)) => return Ok(end),
                        Err(_) => failed = true,
                        Ok(None) => {}
                    }
                }
            }
        }
        if failed {
            failures = failures.saturating_add(1);
            tokio::time::sleep(backoff(failures)).await;
            continue;
        }
        if !seen_welcome {
            failures = failures.saturating_add(1);
            tokio::time::sleep(backoff(failures)).await;
            continue;
        }
        cursor = Some(sent.max(shared.outbox.lock().map_or(0, |o| o.acked_through())));
        if sent_events
            && !overloaded
            && shared.outbox.lock().map_or(0, |o| o.acked_through()) <= before_ack
        {
            failures = failures.saturating_add(1);
            tokio::time::sleep(backoff(failures)).await;
        } else {
            failures = 0;
        }
        if shared.finishing.load(std::sync::atomic::Ordering::SeqCst)
            && shared.outbox.lock().is_ok_and(|o| o.drained())
        {
            return Ok(End::Flushed);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A server cannot resume before locally acknowledged records, which may already be gone.
fn validate_welcome(welcome: &Welcome, acked: u64) -> Result<(), &'static str> {
    if welcome.protocol != PROTOCOL_VERSION {
        return Err("unsupported server protocol");
    }
    if welcome.limits.max_inflight_events == 0
        || welcome.limits.max_frame_bytes == 0
        || welcome.limits.heartbeat_interval_ms == 0
    {
        return Err("invalid server limits");
    }
    if welcome.committed_through < acked {
        return Err("server commit regressed below acknowledged events");
    }
    Ok(())
}

/// Publishes the credential in use to the uploader.
fn share(shared: &Shared, token: Option<&String>) {
    if let (Some(t), Ok(mut slot)) = (token, shared.credential.lock()) {
        *slot = Some(t.clone());
    }
}

/// Exponential backoff with full jitter: uniform in `0..ceiling`.
fn backoff(attempt: u32) -> Duration {
    jitter_below(
        BACKOFF_BASE
            .saturating_mul(1u32 << attempt.min(8))
            .min(BACKOFF_CAP),
    )
}

/// The wait after `attempt` busy refusals in a row: uniform in `0..overload_ceiling(attempt)`.
fn overload_pause(attempt: u32) -> Duration {
    jitter_below(overload_ceiling(attempt))
}

/// The longest wait after `attempt` busy refusals in a row: 200 ms, doubling, to 5 s.
fn overload_ceiling(attempt: u32) -> Duration {
    OVERLOAD_BASE
        .saturating_mul(1u32 << attempt.min(5))
        .min(OVERLOAD_CAP)
}

/// A uniform draw in `0..ceiling`. The draw is OS randomness; clock bits are not (on macOS the
/// nanoseconds are always a multiple of 1000, so the delay was zero).
fn jitter_below(ceiling: Duration) -> Duration {
    let ceiling_ns = u64::try_from(ceiling.as_nanos()).unwrap_or(u64::MAX);
    // No randomness from the OS means no delay: the reconnect loop must not panic over it.
    Duration::from_nanos(
        getrandom::u64()
            .unwrap_or(0)
            .checked_rem(ceiling_ns)
            .unwrap_or(0),
    )
}

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Opens the WebSocket: TCP (through the proxy, if any), TLS with Fleet's roots, upgrade.
async fn open(
    cfg: &Config,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Error,
> {
    let mut request = cfg.url.as_str().into_client_request()?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(WS_SUBPROTOCOL),
    );
    if let Some(t) = &cfg.token {
        request.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {t}"))?,
        );
    }
    let target = crate::net::Target::parse(&cfg.url).ok_or("FLEET_SERVER is not a ws(s):// URL")?;
    let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, async {
        let proxy = if cfg.token.is_some() && crate::net::direct_loopback(&cfg.url) {
            None
        } else {
            cfg.proxy.as_ref()
        };
        let tcp = crate::net::tcp(&target, proxy).await?;
        tokio_tungstenite::client_async_tls_with_config(
            request,
            tcp,
            None,
            Some(tokio_tungstenite::Connector::Rustls(Arc::clone(&cfg.tls))),
        )
        .await
        .map_err(Error::from)
    })
    .await??;
    Ok(ws)
}

#[expect(
    clippy::too_many_lines,
    reason = "keep the websocket handshake, bounded sends and receive deadlines in one state machine"
)]
async fn connection(
    cfg: &Config,
    shared: &Shared,
    telemetry: &mut mpsc::Receiver<Telemetry>,
    clock: &impl Fn() -> ClockSample,
) -> Result<End, Error> {
    let ws = open(cfg).await?;
    shared
        .connected
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (mut tx, mut rx) = ws.split();

    tokio::time::timeout(
        CONNECT_TIMEOUT,
        tx.send(Message::text(serde_json::to_string(&hello_frame(
            cfg, shared, clock,
        )?)?)),
    )
    .await??;

    let welcome = tokio::time::timeout(HELLO_TIMEOUT, await_welcome(&mut rx)).await??;
    let welcome = match welcome {
        Ok(w) => w,
        Err(end) => return Ok(end),
    };
    let acked = shared
        .outbox
        .lock()
        .map_err(|_| "outbox lock")?
        .acked_through();
    validate_welcome(&welcome, acked)?;
    shared
        .connected
        .store(true, std::sync::atomic::Ordering::SeqCst);
    tracing::info!(
        committed = welcome.committed_through,
        epoch = welcome.epoch,
        "agent connected"
    );
    let limits: Limits = welcome.limits;
    share_intervals(shared, &limits);
    let mut sent = welcome.committed_through;
    shared
        .outbox
        .lock()
        .map_err(|_| "outbox lock")?
        .ack(welcome.committed_through);

    let mut heartbeat = tokio::time::interval(Duration::from_millis(
        limits.heartbeat_interval_ms.max(1000),
    ));
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut ping = tokio::time::interval(PING_INTERVAL);
    let mut last_frame = tokio::time::Instant::now();
    loop {
        if last_frame.elapsed() >= cfg.silence_timeout {
            return Err("server stopped sending frames".into());
        }
        // Send what the window allows.
        let batch = {
            let mut o = shared.outbox.lock().map_err(|_| "outbox lock")?;
            let inflight = sent.saturating_sub(o.acked_through());
            let room =
                usize::try_from(u64::from(limits.max_inflight_events).saturating_sub(inflight))
                    .unwrap_or(0);
            o.after(
                sent,
                BATCH.min(room),
                event_budget(limits.max_frame_bytes, 0),
            )
        };
        if let Some(last) = batch.last() {
            sent = last.seq;
            tokio::time::timeout(
                CONNECT_TIMEOUT,
                tx.send(Message::text(serde_json::to_string(
                    &ClientFrame::Events { events: batch },
                )?)),
            )
            .await??;
            continue;
        }
        let (drained, finishing) = {
            let o = shared.outbox.lock().map_err(|_| "outbox lock")?;
            (
                o.drained(),
                shared.finishing.load(std::sync::atomic::Ordering::SeqCst),
            )
        };
        if finishing && drained {
            tokio::time::timeout(
                CONNECT_TIMEOUT,
                tx.send(Message::text(serde_json::to_string(&ClientFrame::Bye)?)),
            )
            .await??;
            let _ = tokio::time::timeout(CONNECT_TIMEOUT, tx.close()).await;
            return Ok(End::Flushed);
        }
        tokio::select! {
            msg = rx.next() => {
                let Some(msg) = msg else { return Err("connection closed".into()) };
                last_frame = tokio::time::Instant::now();
                if let Some(end) = on_frame(decode(&msg?), shared, &mut sent).await? {
                    return Ok(end);
                }
            }
            t = telemetry.recv() => {
                if let Some(t) = t {
                    tokio::time::timeout(CONNECT_TIMEOUT, tx.send(Message::text(serde_json::to_string(&ClientFrame::Telemetry(t))?))).await??;
                }
            }
            () = shared.wake.notified() => {}
            _ = heartbeat.tick() => {
                let unacked = { let o = shared.outbox.lock().map_err(|_| "outbox lock")?; o.last_seq().saturating_sub(o.acked_through()) };
                tokio::time::timeout(CONNECT_TIMEOUT, tx.send(Message::text(serde_json::to_string(&ClientFrame::Heartbeat { clock: clock(), unacked })?))).await??;
            }
            _ = ping.tick() => {
                tokio::time::timeout(CONNECT_TIMEOUT, tx.send(Message::Ping(Vec::new().into()))).await??;
            }
            _ = tick.tick() => {}
        }
    }
}

/// Applies one server frame. `Some` ends the connection for good.
async fn on_frame(
    frame: Option<ServerFrame>,
    shared: &Shared,
    sent: &mut u64,
) -> Result<Option<End>, Error> {
    match frame {
        Some(ServerFrame::Ack { through, .. }) => {
            shared
                .overloads
                .store(0, std::sync::atomic::Ordering::SeqCst);
            shared
                .outbox
                .lock()
                .map_err(|_| "outbox lock")?
                .ack(through);
        }
        Some(ServerFrame::EventRejected { seq, code, .. }) => {
            tracing::warn!(seq, ?code, "event rejected");
            shared.outbox.lock().map_err(|_| "outbox lock")?.ack(seq);
        }
        Some(ServerFrame::Error {
            code: ErrorCode::SeqGap,
            expected_seq: Some(e),
            ..
        }) => *sent = e.saturating_sub(1),
        // The server is busy and committed none of the batch. It keeps the socket, so wait, then
        // resend from its first seq on this connection. The run does not fail over this (I2).
        Some(ServerFrame::Error {
            code: ErrorCode::Overloaded,
            fatal: false,
            expected_seq,
            ..
        }) => {
            let attempt = shared
                .overloads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tracing::warn!(attempt, "server busy; resending after a pause");
            tokio::time::sleep(overload_pause(attempt)).await;
            if let Some(seq) = expected_seq {
                *sent = seq.saturating_sub(1);
            }
        }
        Some(ServerFrame::Error {
            fatal: true,
            code: code @ (ErrorCode::Superseded | ErrorCode::Unauthorized),
            message,
            ..
        }) => {
            return Ok(Some(End::Refused(format!("{code:?}: {message}"))));
        }
        Some(ServerFrame::Error {
            fatal: true,
            message,
            ..
        }) => return Err(message.into()),
        Some(ServerFrame::Goaway {
            reconnect_after_ms, ..
        }) => {
            tokio::time::sleep(Duration::from_millis(reconnect_after_ms)).await;
            return Err("server going away".into());
        }
        _ => {}
    }
    Ok(None)
}

fn hello_frame(
    cfg: &Config,
    shared: &Shared,
    clock: &impl Fn() -> ClockSample,
) -> Result<ClientFrame, Error> {
    let resume = {
        let o = shared.outbox.lock().map_err(|_| "outbox lock")?;
        ResumeState {
            next_seq: o.next_seq(),
            oldest_unacked_seq: o.oldest_unacked(),
        }
    };
    Ok(ClientFrame::Hello(Box::new(Hello {
        protocol: ProtocolRange {
            min: PROTOCOL_VERSION,
            max: PROTOCOL_VERSION,
        },
        agent: AgentInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        },
        agent_run_id: cfg.agent_run_id,
        previous_agent_run_id: None,
        ci: cfg.ci.clone(),
        resume,
        clock: clock(),
        origin: cfg.origin,
        started_at: cfg.started_at,
        catalog: cfg.catalog.clone(),
    })))
}

/// Waits for `welcome`; `Ok(Err(..))` when the server refuses the run for good.
async fn await_welcome<S>(rx: &mut S) -> Result<Result<Welcome, End>, Error>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(msg) = rx.next().await {
        match decode(&msg?) {
            Some(ServerFrame::Welcome(w)) => return Ok(Ok(w)),
            Some(ServerFrame::Reject {
                fatal: true,
                code,
                message,
                ..
            }) => return Ok(Err(End::Refused(format!("{code:?}: {message}")))),
            Some(ServerFrame::Reject { code, message, .. }) => {
                return Err(format!("rejected: {code:?}: {message}").into());
            }
            _ => {}
        }
    }
    Err::<_, Error>("closed before welcome".into())
}

fn decode(msg: &Message) -> Option<ServerFrame> {
    match msg {
        Message::Text(t) => serde_json::from_str(t.as_str()).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_protocol::{
        event::EventBody,
        journal::{EventEnvelope, Provenance, SourceKind, Subject},
    };

    #[tokio::test]
    async fn remote_cleartext_token_is_refused_without_retrying() {
        let cfg = Config::from_env(
            "ws://example.com/api/v1/agent/ws".into(),
            Uuid::now_v7(),
            crate::ci::context(|_| None),
            |key| (key == "FLEET_TOKEN").then(|| "test-token".into()),
        );
        let (_tx, rx) = mpsc::channel(1);
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            run(cfg, Arc::new(Shared::default()), rx, || ClockSample {
                wall: time::OffsetDateTime::UNIX_EPOCH,
                mono_ms: 0,
            }),
        )
        .await;
        assert!(matches!(result, Ok(End::Refused(_))), "{result:?}");
    }

    #[test]
    fn welcome_rejects_incompatible_limits_and_regressing_commit() {
        let valid = Welcome {
            protocol: PROTOCOL_VERSION,
            connection_id: Uuid::nil(),
            epoch: 1,
            committed_through: 2,
            resume: fleet_protocol::frame::ResumeOutcome::Resumed,
            limits: Limits::default(),
        };
        assert!(validate_welcome(&valid, 2).is_ok());
        let mut bad = valid.clone();
        bad.protocol += 1;
        assert!(validate_welcome(&bad, 0).is_err());
        bad = valid.clone();
        bad.limits.max_inflight_events = 0;
        assert!(validate_welcome(&bad, 0).is_err());
        assert!(validate_welcome(&valid, 3).is_err());
    }

    #[test]
    fn a_welcomes_limits_set_the_sampling_intervals() {
        let shared = Shared::default();
        share_intervals(&shared, &Limits::default());
        assert_eq!(shared.cadence(), Cadence::default(), "absent: 5 s and 15 s");

        let shared = Shared::default();
        share_intervals(
            &shared,
            &Limits {
                sample_interval_ms: Some(2_000),
                ..Limits::default()
            },
        );
        assert_eq!(shared.cadence().calm, Duration::from_secs(2));
        assert_eq!(shared.cadence().pressure, Duration::from_secs(15));

        let shared = Shared::default();
        share_intervals(
            &shared,
            &Limits {
                sample_interval_ms: Some(0),
                pressure_sample_interval_ms: Some(999_999_999),
                ..Limits::default()
            },
        );
        assert_eq!(
            shared.cadence(),
            Cadence {
                calm: Duration::from_secs(1),
                pressure: Duration::from_secs(300),
            },
            "out of bounds: clamped to 1 s and 300 s"
        );
    }

    #[tokio::test]
    async fn silent_server_is_reconnected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            #[expect(clippy::result_large_err, reason = "tungstenite callback signature")]
            let callback = |_req: &tokio_tungstenite::tungstenite::handshake::server::Request, mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                response.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static(WS_SUBPROTOCOL));
                Ok(response)
            };
            let mut ws = tokio_tungstenite::accept_hdr_async(socket, callback)
                .await
                .unwrap();
            let _ = ws.next().await.unwrap().unwrap();
            ws.send(Message::text(
                serde_json::json!({ "op": "welcome", "protocol": 1,
                "connection_id": "00000000-0000-0000-0000-000000000001", "epoch": 1,
                "committed_through": 0, "resume": "new", "limits": { "max_inflight_events": 512,
                "max_frame_bytes": 1_048_576, "heartbeat_interval_ms": 10_000 } })
                .to_string(),
            ))
            .await
            .unwrap();
            // A failure guard only: a passing run reconnects after the silence timeout above.
            let second = tokio::time::timeout(Duration::from_secs(60), listener.accept()).await;
            drop(ws);
            assert!(second.is_ok(), "agent did not reconnect to silent server");
        });
        let mut cfg = Config::from_env(
            format!("ws://{addr}"),
            Uuid::now_v7(),
            crate::ci::context(|_| None),
            |key| (key == "FLEET_TOKEN").then(|| "test-token".into()),
        );
        // The silence that ends the first connection is the test's own, not the production 30 s.
        cfg.silence_timeout = Duration::from_millis(500);
        let (_tx, rx) = mpsc::channel(1);
        let agent = tokio::spawn(run(cfg, Arc::new(Shared::default()), rx, || ClockSample {
            wall: time::OffsetDateTime::UNIX_EPOCH,
            mono_ms: 0,
        }));
        server.await.unwrap();
        agent.abort();
    }

    #[tokio::test]
    async fn cleartext_loopback_bypasses_a_configured_proxy() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut cfg = Config::from_env(
            format!("ws://{}", target.local_addr().unwrap()),
            Uuid::now_v7(),
            crate::ci::context(|_| None),
            |key| (key == "FLEET_TOKEN").then(|| "test-token".into()),
        );
        cfg.proxy = Some(crate::net::Proxy {
            host: "127.0.0.1".into(),
            port: proxy.local_addr().unwrap().port(),
            auth: None,
        });
        let connection = tokio::spawn(async move {
            let _ = open(&cfg).await;
        });
        let direct = tokio::time::timeout(Duration::from_secs(1), target.accept()).await;
        connection.abort();
        assert!(direct.is_ok(), "connection did not reach loopback directly");
    }

    #[tokio::test]
    async fn loopback_and_secure_connections_are_not_refused() {
        for url in [
            "ws://localhost:9",
            "ws://127.2.3.4:9",
            "ws://[::1]:9",
            "wss://192.0.2.1",
        ] {
            let cfg = Config::from_env(
                url.into(),
                Uuid::now_v7(),
                crate::ci::context(|_| None),
                |key| (key == "FLEET_TOKEN").then(|| "test-token".into()),
            );
            let (_tx, rx) = mpsc::channel(1);
            let result = tokio::time::timeout(
                Duration::from_millis(100),
                run(cfg, Arc::new(Shared::default()), rx, || ClockSample {
                    wall: time::OffsetDateTime::UNIX_EPOCH,
                    mono_ms: 0,
                }),
            )
            .await;
            assert!(result.is_err(), "{url}: {result:?}");
        }
    }

    #[test]
    fn backoff_stays_under_the_cap() {
        for a in 0..40 {
            assert!(backoff(a) <= BACKOFF_CAP);
        }
    }

    #[test]
    fn backoff_is_jittered() {
        let draws: std::collections::HashSet<Duration> = (0..64).map(|_| backoff(6)).collect();
        assert!(draws.len() > 1, "backoff never varies: {draws:?}");
    }

    /// An event with `seq`, for the outbox.
    fn ev(seq: u64) -> EventEnvelope {
        EventEnvelope::new(
            seq,
            ClockSample {
                wall: OffsetDateTime::UNIX_EPOCH,
                mono_ms: 0,
            },
            Subject::default(),
            Provenance::new(SourceKind::AgentObserved),
            &EventBody::RunFinalizing,
        )
    }

    #[test]
    fn overload_waits_grow_from_200_ms_to_5_s_and_are_jittered() {
        let ceilings: Vec<Duration> = (0..8).map(overload_ceiling).collect();
        assert_eq!(ceilings[0], Duration::from_millis(200));
        assert_eq!(ceilings[1], Duration::from_millis(400));
        assert_eq!(ceilings[4], Duration::from_millis(3_200));
        assert_eq!(ceilings[5], OVERLOAD_CAP, "5 s is the cap");
        assert_eq!(overload_ceiling(u32::MAX), OVERLOAD_CAP);
        assert!((0..64).all(|_| overload_pause(3) < overload_ceiling(3)));
        let draws: std::collections::HashSet<Duration> =
            (0..64).map(|_| overload_pause(6)).collect();
        assert!(draws.len() > 1, "overload waits never vary: {draws:?}");
    }

    #[tokio::test]
    async fn busy_refusals_rewind_to_their_seq_and_an_ack_ends_the_streak() {
        let shared = Shared::default();
        let mut sent = 10;
        let busy = |expected_seq| ServerFrame::Error {
            code: ErrorCode::Overloaded,
            fatal: false,
            message: "busy".into(),
            expected_seq,
        };
        let end = on_frame(Some(busy(Some(7))), &shared, &mut sent)
            .await
            .unwrap();
        assert_eq!(end, None, "a busy refusal never ends the connection");
        assert_eq!(sent, 6, "the batch from seq 7 is resent");
        assert_eq!(
            shared.overloads.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        on_frame(Some(busy(None)), &shared, &mut sent)
            .await
            .unwrap();
        assert_eq!(sent, 6, "without a seq nothing is rewound");
        assert_eq!(
            shared.overloads.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        on_frame(
            Some(ServerFrame::Ack {
                through: 0,
                epoch: 1,
                duplicates: 0,
            }),
            &shared,
            &mut sent,
        )
        .await
        .unwrap();
        assert_eq!(
            shared.overloads.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    /// The `seq`s of the next `events` frame on `ws`, skipping the frames that are not events.
    async fn next_event_seqs(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> Vec<u64> {
        loop {
            let Message::Text(text) = ws.next().await.unwrap().unwrap() else {
                continue;
            };
            if let Ok(ClientFrame::Events { events }) = serde_json::from_str(text.as_str()) {
                return events.iter().map(|e| e.seq).collect();
            }
        }
    }

    #[tokio::test]
    async fn a_busy_batch_is_resent_on_the_same_socket_and_acked() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            #[expect(clippy::result_large_err, reason = "tungstenite callback signature")]
            let callback = |_req: &tokio_tungstenite::tungstenite::handshake::server::Request, mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                response.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static(WS_SUBPROTOCOL));
                Ok(response)
            };
            let mut ws = tokio_tungstenite::accept_hdr_async(socket, callback)
                .await
                .unwrap();
            let _hello = ws.next().await.unwrap().unwrap();
            ws.send(Message::text(
                serde_json::json!({ "op": "welcome", "protocol": 1,
                "connection_id": "00000000-0000-0000-0000-000000000001", "epoch": 1,
                "committed_through": 0, "resume": "new", "limits": { "max_inflight_events": 512,
                "max_frame_bytes": 1_048_576, "heartbeat_interval_ms": 10_000 } })
                .to_string(),
            ))
            .await
            .unwrap();
            assert_eq!(next_event_seqs(&mut ws).await, vec![1, 2, 3]);
            // Busy: nothing is committed, and the socket stays open.
            ws.send(Message::text(
                serde_json::json!({ "op": "error", "code": "overloaded", "fatal": false,
                "message": "the server is busy; resend from seq 1", "expected_seq": 1 })
                .to_string(),
            ))
            .await
            .unwrap();
            // The agent resends from seq 1 on this same socket, and the server commits it.
            assert_eq!(next_event_seqs(&mut ws).await, vec![1, 2, 3]);
            ws.send(Message::text(
                serde_json::json!({ "op": "ack", "through": 3, "epoch": 1, "duplicates": 0 })
                    .to_string(),
            ))
            .await
            .unwrap();
        });
        let cfg = Config::from_env(
            format!("ws://{addr}"),
            Uuid::now_v7(),
            crate::ci::context(|_| None),
            |key| (key == "FLEET_TOKEN").then(|| "test-token".into()),
        );
        let shared = Arc::new(Shared::default());
        shared
            .outbox
            .lock()
            .unwrap()
            .push(vec![ev(1), ev(2), ev(3)]);
        let (_tx, rx) = mpsc::channel(1);
        let agent = tokio::spawn(run(cfg, Arc::clone(&shared), rx, || ClockSample {
            wall: time::OffsetDateTime::UNIX_EPOCH,
            mono_ms: 0,
        }));
        server.await.unwrap();
        let acked = tokio::time::timeout(Duration::from_secs(5), async {
            while shared.outbox.lock().unwrap().acked_through() < 3 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        agent.abort();
        assert!(acked.is_ok(), "the resent batch was not acked");
        assert_eq!(
            shared.overloads.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the ack ended the busy streak"
        );
    }

    /// A batch response carrying `committed` as the welcome and `frames` after it.
    fn batch_response(committed: u64, frames: Vec<serde_json::Value>) -> serde_json::Value {
        let mut all = vec![serde_json::json!({ "op": "welcome", "protocol": 1,
            "connection_id": "00000000-0000-0000-0000-000000000001", "epoch": 1,
            "committed_through": committed, "resume": "new", "limits": { "max_inflight_events": 512,
            "max_frame_bytes": 1_048_576, "heartbeat_interval_ms": 10_000 } })];
        all.extend(frames);
        serde_json::json!({ "frames": all })
    }

    #[tokio::test]
    async fn a_busy_batch_round_is_resent_and_is_not_a_link_failure() {
        // Five busy answers in a row would end the batch transport (its failure limit is five),
        // so the batch only gets acked if busy rounds do not count as failures.
        const REFUSALS: usize = 5;
        let server = wiremock::MockServer::start().await;
        let seen: Arc<std::sync::Mutex<Vec<Vec<u64>>>> = Arc::default();
        let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (log, count) = (Arc::clone(&seen), Arc::clone(&rounds));
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v1/agent/batch"))
            .respond_with(move |req: &wiremock::Request| {
                let body: serde_json::Value = req.body_json().unwrap_or_default();
                let seqs: Vec<u64> = body["events"]
                    .as_array()
                    .map(|events| events.iter().filter_map(|e| e["seq"].as_u64()).collect())
                    .unwrap_or_default();
                if !seqs.is_empty() {
                    log.lock().unwrap().push(seqs.clone());
                }
                let round = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = match (round, seqs.first(), seqs.last()) {
                    // Nothing to send yet: the welcome only.
                    (0, _, _) => batch_response(0, vec![]),
                    // Busy: nothing committed, resend from the batch's first seq.
                    (r, Some(&first), _) if r <= REFUSALS => batch_response(
                        0,
                        vec![serde_json::json!({ "op": "error", "code": "overloaded",
                            "fatal": false, "message": "busy", "expected_seq": first })],
                    ),
                    // The resend commits.
                    (_, _, Some(&last)) if !seqs.is_empty() => batch_response(
                        last,
                        vec![
                            serde_json::json!({ "op": "ack", "through": last, "epoch": 1,
                            "duplicates": 0 }),
                        ],
                    ),
                    _ => batch_response(3, vec![]),
                };
                wiremock::ResponseTemplate::new(200).set_body_json(response)
            })
            .mount(&server)
            .await;
        let cfg = Config::from_env(
            format!("ws://{}", server.address()),
            Uuid::now_v7(),
            crate::ci::context(|_| None),
            |key| (key == "FLEET_TOKEN").then(|| "test-token".into()),
        );
        let shared = Arc::new(Shared::default());
        shared
            .outbox
            .lock()
            .unwrap()
            .push(vec![ev(1), ev(2), ev(3)]);
        let (_tx, mut rx) = mpsc::channel::<Telemetry>(1);
        let task = {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                let clock = || ClockSample {
                    wall: OffsetDateTime::UNIX_EPOCH,
                    mono_ms: 0,
                };
                batch(&cfg, &shared, &mut rx, &clock).await
            })
        };
        let acked = tokio::time::timeout(Duration::from_secs(30), async {
            while shared.outbox.lock().unwrap().acked_through() < 3 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let still_running = !task.is_finished();
        task.abort();
        assert!(acked.is_ok(), "the resent batch was not acked");
        assert!(still_running, "the batch transport gave up on busy rounds");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen.len(),
            REFUSALS + 1,
            "each refused round was resent: {seen:?}"
        );
        assert!(
            seen.iter().all(|seqs| seqs == &[1, 2, 3]),
            "every round resends from seq 1: {seen:?}"
        );
        assert_eq!(
            shared.overloads.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the ack ended the busy streak"
        );
    }

    #[tokio::test]
    async fn refused_reconnects_are_backed_off() {
        // Accepts every connection and drops it at once: each one is a failed attempt.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = Arc::clone(&attempts);
        let refuse = tokio::spawn(async move {
            while listener.accept().await.is_ok() {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let cfg = Config::from_env(
            format!("ws://{addr}"),
            Uuid::now_v7(),
            crate::ci::context(|_| None),
            |key| (key == "FLEET_TOKEN").then(|| "test-token".into()),
        );
        let (_tx, rx) = mpsc::channel(1);
        let agent = tokio::spawn(run(cfg, Arc::new(Shared::default()), rx, || ClockSample {
            wall: time::OffsetDateTime::UNIX_EPOCH,
            mono_ms: 0,
        }));
        tokio::time::sleep(Duration::from_secs(5)).await;
        agent.abort();
        refuse.abort();
        let n = attempts.load(std::sync::atomic::Ordering::SeqCst);
        assert!(n <= 50, "{n} reconnect attempts in 5 s: no backoff");
    }
}
