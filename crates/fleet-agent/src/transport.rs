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
        AgentInfo, CiContext, ClientFrame, ErrorCode, Hello, Limits, ProtocolRange, ResumeState,
        ServerFrame, Telemetry, Welcome,
    },
    journal::ClockSample,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};
use uuid::Uuid;

use crate::outbox::Outbox;

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
    /// The bearer credential in use (`FLEET_TOKEN`, or the token the OIDC exchange returned),
    /// for the artifact uploader, which speaks as the same agent.
    pub(crate) credential: Arc<Mutex<Option<String>>>,
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
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// Runs until everything is flushed after `finishing`, or the server refuses the run.
/// The caller bounds it with a deadline.
pub(crate) async fn run(
    cfg: Config,
    shared: Arc<Shared>,
    mut telemetry: mpsc::Receiver<Telemetry>,
    clock: impl Fn() -> ClockSample,
) -> End {
    let mut attempt: u32 = 0;
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
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
            }
        }
        match connection(&cfg, &shared, &mut telemetry, &clock).await {
            Ok(end) => return end,
            Err(e) => tracing::warn!(error = %e, attempt, "agent connection lost"),
        }
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(backoff(attempt)).await;
    }
}

/// Publishes the credential in use to the uploader.
fn share(shared: &Shared, token: Option<&String>) {
    if let (Some(t), Ok(mut slot)) = (token, shared.credential.lock()) {
        *slot = Some(t.clone());
    }
}

/// Exponential backoff with full jitter.
fn backoff(attempt: u32) -> Duration {
    let ceiling = BACKOFF_BASE
        .saturating_mul(1u32 << attempt.min(8))
        .min(BACKOFF_CAP);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    ceiling.mul_f64(f64::from(nanos % 1000) / 1000.0)
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

async fn connection(
    cfg: &Config,
    shared: &Shared,
    telemetry: &mut mpsc::Receiver<Telemetry>,
    clock: &impl Fn() -> ClockSample,
) -> Result<End, Error> {
    let ws = open(cfg).await?;
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
    tracing::info!(
        committed = welcome.committed_through,
        epoch = welcome.epoch,
        "agent connected"
    );
    let limits: Limits = welcome.limits;
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
        if last_frame.elapsed() >= SILENCE_TIMEOUT {
            return Err("server stopped sending frames".into());
        }
        // Send what the window allows.
        let batch = {
            let mut o = shared.outbox.lock().map_err(|_| "outbox lock")?;
            let inflight = sent.saturating_sub(o.acked_through());
            let room =
                usize::try_from(u64::from(limits.max_inflight_events).saturating_sub(inflight))
                    .unwrap_or(0);
            o.after(sent, BATCH.min(room))
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
        Some(ServerFrame::Ack { through, .. }) => shared
            .outbox
            .lock()
            .map_err(|_| "outbox lock")?
            .ack(through),
        Some(ServerFrame::EventRejected { seq, code, .. }) => {
            tracing::warn!(seq, ?code, "event rejected");
            shared.outbox.lock().map_err(|_| "outbox lock")?.ack(seq);
        }
        Some(ServerFrame::Error {
            code: ErrorCode::SeqGap,
            expected_seq: Some(e),
            ..
        }) => *sent = e.saturating_sub(1),
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
                "max_frame_bytes": 1048576, "heartbeat_interval_ms": 10000 } })
                .to_string(),
            ))
            .await
            .unwrap();
            let second = tokio::time::timeout(Duration::from_secs(35), listener.accept()).await;
            drop(ws);
            assert!(second.is_ok(), "agent did not reconnect to silent server");
        });
        let cfg = Config::from_env(
            format!("ws://{addr}"),
            Uuid::now_v7(),
            crate::ci::context(|_| None),
            |_| None,
        );
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
}
