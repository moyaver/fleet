// SPDX-License-Identifier: AGPL-3.0-only
//! A server on a fresh database, and simulated agents that publish through the real ingest path.

use std::{net::SocketAddr, time::Duration as StdDuration};

use axum_test::{TestResponse, TestServer};
use fleet_core::policy::{Grant, Role};
use fleet_protocol::{
    PROTOCOL_VERSION,
    brand::WS_SUBPROTOCOL,
    event::{
        EventBody, ExitClass, FailureInfo, FlowEnded, FlowOutcome, FlowRef, FlowReported,
        FlowStarted, JunitTestcase, MatchMethod, PlanAssigned, ProcessExited, ReportParsed,
        ReportStatus, Reproduction, RunEnded, RunStarted, ShardAssigned, ShardMode, StepEnded,
        StepStarted, StepStatus, SuiteStarted,
    },
    frame::{
        AgentInfo, BatchRequest, BatchResponse, CiContext, CiProvider, ClientFrame, Hello,
        ProtocolRange, ResumeState, RunOrigin, Sample, SampleUnavailable, ServerFrame, Telemetry,
    },
    journal::{ClockSample, EventEnvelope, Provenance, SourceKind, StepRef, Subject},
};
use fleet_store::{Store, auth::TrustLevel};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};
use uuid::Uuid;

use crate::{
    app,
    config::{Config, ProjectConfig, Secret},
    session,
};

/// The viewer token the test server accepts: it may read every project.
pub(super) const VIEWER: &str = "viewer-secret";

/// The application under test.
pub(super) struct Rig {
    pub(super) app: TestServer,
    pub(super) state: crate::state::AppState,
    pub(super) store: Store,
    /// A publisher key for project `app`.
    pub(super) key: String,
    /// The test's database, dropped with the rig.
    _db: fleet_store::testing::TestDatabase,
}

pub(super) async fn rig() -> Rig {
    rig_with(|_| {}).await
}

pub(super) async fn rig_with(tweak: impl FnOnce(&mut Config)) -> Rig {
    let db = fleet_store::testing::fresh_database().await.unwrap();
    let mut config = Config::default();
    config.database.url = Some(Secret::from(db.url()));
    config.api.viewer_tokens = vec![Secret::from(VIEWER)];
    config.ingest.telemetry_flush_ms = 50;
    config.projects = ["app", "other"]
        .map(|slug| ProjectConfig {
            slug: slug.into(),
            name: slug.into(),
            trust_policies: Vec::new(),
        })
        .into();
    tweak(&mut config);
    let state = app::init(config).await.unwrap();
    state
        .tracker
        .spawn(crate::ingest::telemetry::flusher(state.clone()));
    let store = state.store.clone();
    let project = store.project_id("app").await.unwrap().unwrap();
    let (_, key) = store
        .create_api_key(
            project,
            "ci",
            TrustLevel::Trusted,
            time::OffsetDateTime::now_utc() + time::Duration::days(90),
        )
        .await
        .unwrap();
    // A real socket, so agent WebSockets work too.
    let app = TestServer::builder()
        .http_transport()
        .build(app::router(&state).into_make_service_with_connect_info::<SocketAddr>());
    Rig {
        app,
        state,
        store,
        key: key.expose().to_owned(),
        _db: db,
    }
}

impl Rig {
    /// A read with the viewer token.
    pub(super) async fn get(&self, path: &str) -> TestResponse {
        self.app.get(path).authorization_bearer(VIEWER).await
    }

    /// A read as a signed-in user whose only grant is `role` on project `slug`.
    pub(super) async fn get_as(&self, slug: &str, role: Role, path: &str) -> TestResponse {
        let project = self.store.project_id(slug).await.unwrap().unwrap();
        let user = self
            .store
            .upsert_user("https://idp.test", slug, None, None)
            .await
            .unwrap();
        let data = json!({
            "grants": [Grant { project: Some(project), role }],
            "csrf": "csrf", "email": null, "name": null,
        });
        let cookie = self
            .store
            .create_session(
                Some(user),
                &data,
                OffsetDateTime::now_utc() + Duration::hours(1),
            )
            .await
            .unwrap();
        self.app
            .get(path)
            .add_header("cookie", format!("{}={}", session::COOKIE, cookie.expose()))
            .await
    }

    /// The JSON of a successful read.
    pub(super) async fn json(&self, path: &str) -> Value {
        let res = self.get(path).await;
        assert_eq!(res.status_code(), 200, "{path}: {}", res.text());
        res.json()
    }

    /// Publishes what `run` has queued (and `telemetry`) in one HTTPS batch, like an agent
    /// behind a proxy that blocks `WebSocket`s.
    pub(super) async fn publish(&self, run: &mut Agent, telemetry: Vec<Telemetry>) {
        self.publish_as(&self.key, run, telemetry).await;
    }

    /// [`publish`](Self::publish) with another publisher's key (another project's).
    pub(super) async fn publish_as(&self, key: &str, run: &mut Agent, telemetry: Vec<Telemetry>) {
        let hello = run.hello();
        let events = std::mem::take(&mut run.queued);
        let res = self
            .app
            .post("/api/v1/agent/batch")
            .authorization_bearer(key)
            .json(&BatchRequest {
                hello,
                events,
                telemetry,
            })
            .await;
        assert_eq!(res.status_code(), 200, "{}", res.text());
        let body: BatchResponse = res.json();
        assert!(
            body.frames.iter().all(|f| matches!(
                f,
                ServerFrame::Welcome(_)
                    | ServerFrame::Ack { .. }
                    | ServerFrame::TelemetryAck { .. }
            )),
            "the batch was refused: {:?}",
            body.frames
        );
    }

    pub(super) async fn attempt_id(&self, project: &str) -> String {
        let attempts = self
            .json(&format!("/api/v1/projects/{project}/attempts"))
            .await;
        attempts[0]["id"].as_str().unwrap().to_owned()
    }
}

/// An agent's WebSocket, as a connected agent holds it.
pub(super) struct Socket(WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>);

impl Socket {
    async fn frame(&mut self) -> ServerFrame {
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(10);
        loop {
            match tokio::time::timeout_at(deadline, self.0.next())
                .await
                .expect("a frame within 10 s")
            {
                Some(Ok(Message::Text(t))) => return serde_json::from_str(t.as_str()).unwrap(),
                Some(Ok(_)) => {}
                other => panic!("the agent socket ended: {other:?}"),
            }
        }
    }

    /// Sends the queued events and waits for their ack.
    pub(super) async fn publish(&mut self, agent: &mut Agent) {
        let events = std::mem::take(&mut agent.queued);
        let last = events.last().map(|e| e.seq).unwrap();
        let frame = serde_json::to_string(&ClientFrame::Events { events }).unwrap();
        self.0.send(Message::text(frame)).await.unwrap();
        loop {
            if let ServerFrame::Ack { through, .. } = self.frame().await
                && through >= last
            {
                return;
            }
        }
    }

    /// Drops the connection without a `bye`: the agent's machine went away.
    pub(super) async fn drop_connection(mut self) {
        let _ = self.0.close(None).await;
    }
}

impl Rig {
    /// Opens `agent`'s run over a WebSocket (`hello` → `welcome`).
    pub(super) async fn connect(&self, agent: &Agent) -> Socket {
        let address = self.app.server_address().unwrap();
        let mut req = format!(
            "ws://{}:{}/api/v1/agent/ws",
            address.host_str().unwrap(),
            address.port().unwrap()
        )
        .into_client_request()
        .unwrap();
        req.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static(WS_SUBPROTOCOL),
        );
        req.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {}", self.key)).unwrap(),
        );
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let mut socket = Socket(ws);
        let hello = ClientFrame::Hello(Box::new(agent.hello()));
        socket
            .0
            .send(Message::text(serde_json::to_string(&hello).unwrap()))
            .await
            .unwrap();
        assert!(matches!(socket.frame().await, ServerFrame::Welcome(_)));
        socket
    }

    /// Polls `path` until `done` holds for its JSON, for at most 10 s.
    pub(super) async fn until(&self, path: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(10);
        loop {
            let v = self.json(path).await;
            if done(&v) {
                return v;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{path} never got there: {v}"
            );
            tokio::time::sleep(StdDuration::from_millis(25)).await;
        }
    }
}

fn now() -> ClockSample {
    ClockSample {
        wall: OffsetDateTime::now_utc(),
        mono_ms: 0,
    }
}

/// A simulated agent run: queues journal events with gap-free `seq`s.
pub(super) struct Agent {
    pub(super) id: Uuid,
    seq: u64,
    queued: Vec<EventEnvelope>,
    ci_run: String,
    job_key: String,
    runner: String,
    sha: Option<String>,
    git_ref: Option<String>,
    arch: String,
    flow_ms: u64,
}

/// The entities of one flow, as events name them.
#[derive(Clone)]
pub(super) struct FlowIds {
    pub(super) subject: Subject,
}

impl Agent {
    pub(super) fn new(ci_run: &str, job_key: &str, runner: &str) -> Self {
        Self {
            id: Uuid::now_v7(),
            seq: 0,
            queued: Vec::new(),
            ci_run: ci_run.into(),
            job_key: job_key.into(),
            runner: runner.into(),
            sha: None,
            git_ref: None,
            arch: "x86_64".into(),
            flow_ms: 1500,
        }
    }

    /// The commit and ref the CI run tests.
    pub(super) fn on_commit(mut self, sha: &str, git_ref: &str) -> Self {
        self.sha = Some(sha.into());
        self.git_ref = Some(git_ref.into());
        self
    }

    /// The CPU architecture of the runner (a fact of the condition fingerprint).
    pub(super) fn on_arch(mut self, arch: &str) -> Self {
        self.arch = arch.into();
        self
    }

    /// How long the flows reported from now on take (Maestro reports the same figure live and in
    /// the final report).
    pub(super) fn set_flow_time(&mut self, ms: u64) {
        self.flow_ms = ms;
    }

    pub(super) fn hello(&self) -> Hello {
        Hello {
            protocol: ProtocolRange {
                min: PROTOCOL_VERSION,
                max: PROTOCOL_VERSION,
            },
            agent: AgentInfo {
                version: "0.0.0".into(),
                os: "linux".into(),
                arch: self.arch.clone(),
            },
            agent_run_id: self.id,
            previous_agent_run_id: None,
            ci: CiContext {
                provider: CiProvider::Generic,
                repository: Some("o/r".into()),
                run_id: Some(self.ci_run.clone()),
                run_attempt: Some(1),
                job_key: self.job_key.clone(),
                job_name: Some(self.job_key.clone()),
                sha: self.sha.clone(),
                git_ref: self.git_ref.clone(),
                runner: Some(self.runner.clone()),
            },
            resume: ResumeState {
                next_seq: self.seq + 1,
                oldest_unacked_seq: self.queued.first().map(|e| e.seq),
            },
            clock: now(),
            origin: RunOrigin::Live,
            started_at: None,
            catalog: fleet_protocol::frame::FlowCatalog::default(),
        }
    }

    fn push(&mut self, subject: Subject, src: SourceKind, body: &EventBody) {
        self.seq += 1;
        self.queued.push(EventEnvelope::new(
            self.seq,
            now(),
            subject,
            Provenance::new(src),
            body,
        ));
    }

    pub(super) fn started(&mut self) {
        self.started_with(&["maestro", "test"], None);
    }

    /// The run start with its command line and the rerun words the agent built from it (#82).
    pub(super) fn started_with(&mut self, argv: &[&str], reproduction: Option<Reproduction>) {
        self.push(
            Subject::default(),
            SourceKind::AgentObserved,
            &EventBody::RunStarted(RunStarted {
                argv: argv.iter().map(|a| (*a).into()).collect(),
                agent_version: "0.0.0".into(),
                reproduction,
                evidence: None,
            }),
        );
    }

    /// A flow that ended without ever starting, with no path: nothing in the run names its file.
    pub(super) fn flow_ended_without_path(&mut self, shard: &Subject, name: &str) {
        let subject = Subject {
            flow_execution_id: Some(Uuid::now_v7()),
            ..shard.clone()
        };
        self.push(
            subject,
            SourceKind::LiveConsole,
            &EventBody::FlowEnded(FlowEnded {
                outcome: FlowOutcome::Failed,
                duration_ms: Some(self.flow_ms),
                failure: Some(FailureInfo {
                    message: "boom".into(),
                }),
                flow: Some(FlowRef {
                    name: name.into(),
                    path: None,
                }),
            }),
        );
    }

    pub(super) fn ended(&mut self) {
        self.push(
            Subject::default(),
            SourceKind::AgentObserved,
            &EventBody::RunEnded(RunEnded {
                interrupted: false,
                output_bytes_dropped: 0,
            }),
        );
    }

    /// This run executes shard `shard_id` of the Fleet plan `plan_id` (`plan.assigned`).
    pub(super) fn plan_assigned(&mut self, plan_id: &str, shard_id: u32) {
        self.push(
            Subject::default(),
            SourceKind::Declared,
            &EventBody::PlanAssigned(PlanAssigned {
                plan_id: plan_id.into(),
                shard_id,
                flows: Vec::new(),
            }),
        );
    }

    /// A suite with one shard on `serial`; returns the shard's subject.
    pub(super) fn shard(&mut self, serial: &str, planned: &[&str]) -> Subject {
        let suite = Subject {
            suite_execution_id: Some(Uuid::now_v7()),
            ..Subject::default()
        };
        self.push(
            suite.clone(),
            SourceKind::Declared,
            &EventBody::SuiteStarted(SuiteStarted {
                shard_mode: ShardMode::None,
                shards: 1,
            }),
        );
        let shard = Subject {
            shard_id: Some(Uuid::now_v7()),
            device_session_id: Some(Uuid::now_v7()),
            ..suite
        };
        self.push(
            shard.clone(),
            SourceKind::DebugLog,
            &EventBody::ShardAssigned(ShardAssigned {
                shard_number: 1,
                device_serial: serial.into(),
                planned: planned
                    .iter()
                    .map(|n| FlowRef {
                        name: (*n).into(),
                        path: Some(format!("flows/{n}.yaml")),
                    })
                    .collect(),
            }),
        );
        shard
    }

    /// Maestro exits (`failed`: with test failures) and no JUnit report arrives: the suite closes.
    pub(super) fn suite_finished(&mut self, shard: &Subject, failed: bool) {
        let suite = Subject {
            suite_execution_id: shard.suite_execution_id,
            ..Subject::default()
        };
        self.push(
            suite.clone(),
            SourceKind::ProcessExit,
            &EventBody::ProcessExited(ProcessExited {
                code: Some(i32::from(failed)),
                signal: None,
                class: if failed {
                    ExitClass::TestFailures
                } else {
                    ExitClass::Passed
                },
            }),
        );
        self.push(
            suite,
            SourceKind::JunitFinal,
            &EventBody::ReportParsed(ReportParsed {
                status: ReportStatus::Absent,
                suites: 0,
                tests: 0,
                failures: 0,
            }),
        );
    }

    pub(super) fn flow_started(&mut self, shard: &Subject, name: &str, ordinal: u32) -> FlowIds {
        let flow = Uuid::now_v7();
        let subject = Subject {
            flow_execution_id: Some(flow),
            ..shard.clone()
        };
        self.push(
            subject.clone(),
            SourceKind::DebugLog,
            &EventBody::FlowStarted(FlowStarted {
                flow: FlowRef {
                    name: name.into(),
                    path: Some(format!("flows/{name}.yaml")),
                },
                shard_number: 1,
                ordinal,
            }),
        );
        FlowIds { subject }
    }

    /// A planned flow Maestro reported on without ever starting it (no `flow.started`).
    pub(super) fn flow_reported_unstarted(
        &mut self,
        shard: &Subject,
        name: &str,
        outcome: FlowOutcome,
    ) {
        let subject = Subject {
            flow_execution_id: Some(Uuid::now_v7()),
            ..shard.clone()
        };
        self.push(
            subject,
            SourceKind::LiveConsole,
            &EventBody::FlowEnded(FlowEnded {
                outcome,
                duration_ms: None,
                failure: None,
                flow: Some(FlowRef {
                    name: name.into(),
                    path: Some(format!("flows/{name}.yaml")),
                }),
            }),
        );
    }

    pub(super) fn step(&mut self, flow: &FlowIds, index: u32, label: &str, status: StepStatus) {
        let subject = Subject {
            step: Some(StepRef { index }),
            ..flow.subject.clone()
        };
        self.push(
            subject.clone(),
            SourceKind::DebugLog,
            &EventBody::StepStarted(StepStarted {
                description: label.into(),
                depth: None,
                parent: None,
                attempt: None,
            }),
        );
        self.push(
            subject,
            SourceKind::DebugLog,
            &EventBody::StepEnded(StepEnded {
                status,
                timing: None,
            }),
        );
    }

    /// A step whose start carries the nesting Maestro's log gave it, and its end.
    pub(super) fn nested_step(
        &mut self,
        flow: &FlowIds,
        index: u32,
        started: StepStarted,
        status: StepStatus,
    ) {
        let subject = Subject {
            step: Some(StepRef { index }),
            ..flow.subject.clone()
        };
        self.push(
            subject.clone(),
            SourceKind::DebugLog,
            &EventBody::StepStarted(started),
        );
        self.push(
            subject,
            SourceKind::DebugLog,
            &EventBody::StepEnded(StepEnded {
                status,
                timing: None,
            }),
        );
    }

    /// The final JUnit report's claim about a flow.
    pub(super) fn flow_reported(
        &mut self,
        flow: &FlowIds,
        name: &str,
        outcome: FlowOutcome,
        failure: Option<&str>,
    ) {
        self.push(
            flow.subject.clone(),
            SourceKind::JunitFinal,
            &EventBody::FlowReported(FlowReported {
                repo_path: None,
                testsuite_index: 0,
                testcase_index: 0,
                matched: MatchMethod::Positional,
                junit: JunitTestcase {
                    id: Some(name.into()),
                    name: name.into(),
                    file: Some(format!("flows/{name}.yaml")),
                    time_ms: Some(self.flow_ms),
                    timestamp_local: None,
                    status: Some(
                        if outcome == FlowOutcome::Passed {
                            "SUCCESS"
                        } else {
                            "ERROR"
                        }
                        .into(),
                    ),
                    failure: failure.map(Into::into),
                },
                outcome,
            }),
        );
    }

    pub(super) fn flow_ended(
        &mut self,
        flow: &FlowIds,
        outcome: FlowOutcome,
        failure: Option<&str>,
    ) {
        self.push(
            flow.subject.clone(),
            SourceKind::LiveConsole,
            &EventBody::FlowEnded(FlowEnded {
                outcome,
                duration_ms: Some(self.flow_ms),
                failure: failure.map(|m| FailureInfo { message: m.into() }),
                flow: None,
            }),
        );
    }
}

/// A telemetry frame of these samples, numbered from `tseq_from`.
pub(super) fn telemetry(tseq_from: u64, samples: Vec<Sample>) -> Telemetry {
    Telemetry {
        tseq_from,
        tseq_to: tseq_from + samples.len() as u64 - 1,
        dropped_since_last: 0,
        samples,
        devices: Vec::new(),
    }
}

/// A sample with every metric present.
pub(super) fn sample(at: OffsetDateTime, cpu: f32) -> Sample {
    Sample {
        at: ClockSample {
            wall: at,
            mono_ms: 0,
        },
        host_cpu_pct: Some(cpu),
        host_mem_used_bytes: Some(4 << 30),
        host_mem_total_bytes: Some(16 << 30),
        runner_cpu_pct: Some(80.0),
        runner_rss_bytes: Some(300 << 20),
        agent_cpu_pct: Some(0.2),
        agent_rss_bytes: Some(8 << 20),
        host_load_1m: None,
        host_cpu_cores: None,
        host_swap_used_bytes: None,
        host_swap_total_bytes: None,
        host_psi_cpu_pct: None,
        host_psi_memory_pct: None,
        host_psi_io_pct: None,
        unavailable: SampleUnavailable::default(),
    }
}
