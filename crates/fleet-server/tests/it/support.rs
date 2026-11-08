// SPDX-License-Identifier: AGPL-3.0-only
//! Test harness: a real server on a fresh Postgres 18 database, and a protocol client.

use std::{net::SocketAddr, time::Duration};

use fleet_protocol::{
    PROTOCOL_VERSION,
    brand::WS_SUBPROTOCOL,
    event::{
        EventBody, ExitClass, FlowEnded, FlowOutcome, FlowRef, FlowStarted, ProcessExited,
        ReportParsed, ReportStatus, RunEnded, RunStarted, ShardAssigned, ShardMode, SuiteStarted,
    },
    frame::{
        AgentInfo, CiContext, CiProvider, ClientFrame, Hello, ProtocolRange, ResumeState,
        RunOrigin, ServerFrame,
    },
    journal::{ClockSample, EventEnvelope, Provenance, SourceKind, Subject},
};
use fleet_server::config::{Config, ProjectConfig, Secret};
use futures_util::{SinkExt, StreamExt};
use time::OffsetDateTime;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::HeaderValue,
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};
use uuid::Uuid;

/// The viewer token every test server accepts.
pub(crate) const VIEWER: &str = "viewer-secret";

pub(crate) struct Harness {
    pub(crate) server: Option<fleet_server::Server>,
    pub(crate) config: Config,
    pub(crate) key: String,
    /// The test's database, dropped with the harness.
    _db: fleet_store::testing::TestDatabase,
}

pub(crate) fn test_config(url: &str) -> Config {
    let mut c = Config {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        ..Config::default()
    };
    c.database.url = Some(Secret::from(url));
    c.ingest.telemetry_flush_ms = 200;
    c.ingest.ping_interval_ms = 500;
    c.ingest.shutdown_grace_ms = 5_000;
    c.ingest.reconnect_jitter_ms = 10;
    c.api.viewer_tokens = vec![Secret::from(VIEWER)];
    c.projects = vec![ProjectConfig {
        slug: "app".into(),
        name: "App".into(),
        trust_policies: Vec::new(),
    }];
    c
}

impl Harness {
    pub(crate) async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    pub(crate) async fn start_with(tweak: impl FnOnce(&mut Config)) -> Self {
        let db = fleet_store::testing::fresh_database().await.unwrap();
        let mut config = test_config(db.url());
        tweak(&mut config);
        let server = fleet_server::start(config.clone()).await.unwrap();
        let store = db.store();
        let project = store.project_id("app").await.unwrap().unwrap();
        let (_, key) = store
            .create_api_key(
                project,
                "ci",
                fleet_store::auth::TrustLevel::Trusted,
                time::OffsetDateTime::now_utc() + time::Duration::days(90),
            )
            .await
            .unwrap();
        Self {
            server: Some(server),
            config,
            key: key.expose().to_owned(),
            _db: db,
        }
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.server.as_ref().unwrap().addr()
    }

    pub(crate) async fn stop(&mut self) {
        if let Some(s) = self.server.take() {
            s.stop().await.unwrap();
        }
    }

    /// A new server process on the same database (a restart).
    pub(crate) async fn restart(&mut self) {
        self.restart_observed(async {}).await;
    }

    /// A restart that runs `observe` while the old server drains its sockets. A client that
    /// reads `goaway` and the close frame must do so during the drain: the server finishes the
    /// close handshake only while the client reads (see `ws::close_handshake`).
    pub(crate) async fn restart_observed<T>(&mut self, observe: impl Future<Output = T>) -> T {
        let ((), observed) = tokio::join!(self.stop(), observe);
        self.server = Some(fleet_server::start(self.config.clone()).await.unwrap());
        observed
    }

    pub(crate) async fn connect(&self, token: Option<&str>) -> Client {
        Client::connect(self.addr(), token.or(Some(&self.key)), true).await
    }
}

pub(crate) type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub(crate) struct Client {
    pub(crate) ws: Ws,
}

impl Client {
    pub(crate) async fn connect(addr: SocketAddr, token: Option<&str>, subprotocol: bool) -> Self {
        let mut req = format!("ws://{addr}/api/v1/agent/ws")
            .into_client_request()
            .unwrap();
        if subprotocol {
            req.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                HeaderValue::from_static(WS_SUBPROTOCOL),
            );
        }
        if let Some(t) = token {
            req.headers_mut().insert(
                "Authorization",
                HeaderValue::from_str(&format!("Bearer {t}")).unwrap(),
            );
        }
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        Self { ws }
    }

    pub(crate) async fn send(&mut self, frame: &ClientFrame) {
        self.ws
            .send(Message::text(serde_json::to_string(frame).unwrap()))
            .await
            .unwrap();
    }

    /// The next protocol frame within 10 s, skipping pings.
    pub(crate) async fn recv(&mut self) -> Received {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let msg = tokio::time::timeout_at(deadline, self.ws.next())
                .await
                .expect("a frame within 10 s");
            match msg {
                Some(Ok(Message::Text(t))) => {
                    return Received::Frame(serde_json::from_str(t.as_str()).unwrap());
                }
                Some(Ok(Message::Close(c))) => return Received::Closed(c),
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return Received::Closed(None),
            }
        }
    }

    pub(crate) async fn frame(&mut self) -> ServerFrame {
        match self.recv().await {
            Received::Frame(f) => f,
            Received::Closed(c) => panic!("closed: {c:?}"),
        }
    }

    /// Sends `hello` and returns the `welcome`'s `committed_through` and epoch.
    pub(crate) async fn hello(
        &mut self,
        agent: &Agent,
        oldest_unacked: Option<u64>,
    ) -> ServerFrame {
        self.send(&ClientFrame::Hello(Box::new(agent.hello(oldest_unacked))))
            .await;
        self.frame().await
    }

    /// Sends events and waits for the `ack` (skipping `event_rejected` and `telemetry_ack`).
    pub(crate) async fn events(&mut self, events: Vec<EventEnvelope>) -> (u64, u32) {
        self.send(&ClientFrame::Events { events }).await;
        loop {
            match self.frame().await {
                ServerFrame::Ack {
                    through,
                    duplicates,
                    ..
                } => return (through, duplicates),
                ServerFrame::EventRejected { .. } | ServerFrame::TelemetryAck { .. } => {}
                other => panic!("expected ack, got {other:?}"),
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum Received {
    Frame(ServerFrame),
    Closed(Option<CloseFrame>),
}

impl Received {
    pub(crate) fn close_code(&self) -> Option<CloseCode> {
        match self {
            Self::Closed(Some(c)) => Some(c.code),
            _ => None,
        }
    }
}

fn now() -> ClockSample {
    ClockSample {
        wall: OffsetDateTime::now_utc(),
        mono_ms: 0,
    }
}

/// A simulated agent run.
#[derive(Clone)]
pub(crate) struct Agent {
    pub(crate) id: Uuid,
    pub(crate) seq: u64,
    pub(crate) run_id: String,
    pub(crate) job_key: String,
}

impl Agent {
    pub(crate) fn new(run_id: &str, job_key: &str) -> Self {
        Self {
            id: Uuid::now_v7(),
            seq: 0,
            run_id: run_id.into(),
            job_key: job_key.into(),
        }
    }

    pub(crate) fn hello(&self, oldest_unacked: Option<u64>) -> Hello {
        Hello {
            protocol: ProtocolRange {
                min: PROTOCOL_VERSION,
                max: PROTOCOL_VERSION,
            },
            agent: AgentInfo {
                version: "0.0.0".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
            },
            agent_run_id: self.id,
            previous_agent_run_id: None,
            ci: CiContext {
                provider: CiProvider::Generic,
                repository: Some("o/r".into()),
                run_id: Some(self.run_id.clone()),
                run_attempt: Some(1),
                job_key: self.job_key.clone(),
                job_name: None,
                sha: None,
                git_ref: None,
                runner: Some("runner".into()),
            },
            resume: ResumeState {
                next_seq: self.seq + 1,
                oldest_unacked_seq: oldest_unacked,
            },
            clock: now(),
            origin: RunOrigin::Live,
            started_at: None,
        }
    }

    pub(crate) fn event(
        &mut self,
        subject: Subject,
        src: SourceKind,
        body: &EventBody,
    ) -> EventEnvelope {
        self.seq += 1;
        EventEnvelope::new(self.seq, now(), subject, Provenance::new(src), body)
    }

    /// A whole run of passing (or failing) flows on one device, ending normally.
    #[expect(clippy::too_many_lines, reason = "one straight-line event script")]
    pub(crate) fn suite(&mut self, flows: &[(&str, bool)]) -> Vec<EventEnvelope> {
        let suite = Subject {
            suite_execution_id: Some(Uuid::now_v7()),
            ..Subject::default()
        };
        let shard = Subject {
            shard_id: Some(Uuid::now_v7()),
            device_session_id: Some(Uuid::now_v7()),
            ..suite.clone()
        };
        let mut out = vec![
            self.event(
                Subject::default(),
                SourceKind::AgentObserved,
                &EventBody::RunStarted(RunStarted {
                    argv: vec!["maestro".into()],
                    agent_version: "0.0.0".into(),
                    evidence: None,
                }),
            ),
            self.event(
                suite.clone(),
                SourceKind::Declared,
                &EventBody::SuiteStarted(SuiteStarted {
                    shard_mode: ShardMode::None,
                    shards: 1,
                }),
            ),
            self.event(
                shard.clone(),
                SourceKind::DebugLog,
                &EventBody::ShardAssigned(ShardAssigned {
                    shard_number: 1,
                    device_serial: "emulator-5554".into(),
                    planned: flows
                        .iter()
                        .map(|(n, _)| FlowRef {
                            name: (*n).into(),
                            path: None,
                        })
                        .collect(),
                }),
            ),
        ];
        for (i, (name, passed)) in flows.iter().enumerate() {
            let f = Subject {
                flow_execution_id: Some(Uuid::now_v7()),
                ..shard.clone()
            };
            out.push(self.event(
                f.clone(),
                SourceKind::DebugLog,
                &EventBody::FlowStarted(FlowStarted {
                    flow: FlowRef {
                        name: (*name).into(),
                        path: None,
                    },
                    shard_number: 1,
                    ordinal: u32::try_from(i).unwrap(),
                }),
            ));
            out.push(self.event(
                f,
                SourceKind::LiveConsole,
                &EventBody::FlowEnded(FlowEnded {
                    outcome: if *passed {
                        FlowOutcome::Passed
                    } else {
                        FlowOutcome::Failed
                    },
                    duration_ms: Some(1000),
                    failure: None,
                    flow: None,
                }),
            ));
        }
        let ok = flows.iter().all(|(_, p)| *p);
        out.push(self.event(
            suite.clone(),
            SourceKind::ProcessExit,
            &EventBody::ProcessExited(ProcessExited {
                code: Some(i32::from(!ok)),
                signal: None,
                class: if ok {
                    ExitClass::Passed
                } else {
                    ExitClass::TestFailures
                },
            }),
        ));
        out.push(self.event(
            suite,
            SourceKind::JunitFinal,
            &EventBody::ReportParsed(ReportParsed {
                status: ReportStatus::Absent,
                suites: 0,
                tests: 0,
                failures: 0,
            }),
        ));
        out.push(self.event(
            Subject::default(),
            SourceKind::AgentObserved,
            &EventBody::RunEnded(RunEnded {
                interrupted: false,
                output_bytes_dropped: 0,
            }),
        ));
        out
    }
}

/// A minimal HTTP/1.1 POST over a raw socket (no HTTP client in the tree).
pub(crate) async fn post_json(
    addr: SocketAddr,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, String) {
    http(addr, "POST", path, token, body).await
}

/// A GET with an optional bearer token.
pub(crate) async fn get(addr: SocketAddr, path: &str, token: Option<&str>) -> (u16, String) {
    http(addr, "GET", path, token, "").await
}

async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

/// A browser-style live subscriber on `/api/v1/live`.
pub(crate) async fn live(
    addr: SocketAddr,
    token: Option<&str>,
    origin: Option<&str>,
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://{addr}/api/v1/live")
        .into_client_request()
        .unwrap();
    if let Some(t) = token {
        req.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {t}")).unwrap(),
        );
    }
    if let Some(o) = origin {
        req.headers_mut()
            .insert("Origin", HeaderValue::from_str(o).unwrap());
    }
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
}

/// The next JSON message on a live socket, within 10 s.
pub(crate) async fn live_next(ws: &mut Ws) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, ws.next())
            .await
            .expect("a live message within 10 s")
        {
            Some(Ok(Message::Text(t))) => return serde_json::from_str(t.as_str()).unwrap(),
            Some(Ok(_)) => {}
            other => panic!("live socket ended: {other:?}"),
        }
    }
}
