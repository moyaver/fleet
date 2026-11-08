// SPDX-License-Identifier: AGPL-3.0-only
//! `fleet-loadgen`: simulated agents that speak the real protocol to a real server (PLAN §13,
//! M3 scenarios).
//!
//! Each simulated agent runs one suite sharded across `devices` emulators, `flows` flows each,
//! and streams it like the real agent: `hello` → `welcome{committed_through}` → contiguous
//! batches → cumulative acks; on any disconnect it reconnects with jittered backoff and resumes
//! where the **server** says. Scenario knobs:
//!
//! - `job_keys`: agents share job keys (overlapping job names in one pipeline attempt);
//! - `storm`: every agent drops its connection at its half-way point and reconnects at once;
//! - `duplicates`: batches are resent and shuffled (the server sorts and skips duplicates);
//! - `commit`, `git_ref`, `arch`: what the CI run tests and the runner it runs on, which history
//!   and comparisons group by;
//! - `first_agent`: the index of the first simulated agent. Which scripted flows fail depends on
//!   the index (see [`fails`]), so two runs can pass or fail the same flow;
//! - `hold`: agents stop at chosen points of their script until the test lets them go, so a
//!   browser test can look at a run that is open without racing the clock;
//! - a server restart needs nothing special: agents reconnect until `deadline`.
//!
//! [`run`] returns what each agent knows, so a test can compare it with the server's state.
//! [`quiet`] is the opposite scenario: an agent that stops talking with its socket still open.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use fleet_protocol::{
    PROTOCOL_VERSION,
    brand::WS_SUBPROTOCOL,
    event::{
        EventBody, ExitClass, FlowEnded, FlowOutcome, FlowRef, FlowReported, FlowStarted,
        JunitTestcase, MatchMethod, ProcessExited, ReportParsed, ReportStatus, RunEnded,
        RunStarted, ShardAssigned, ShardMode, StepEnded, StepStarted, StepStatus, SuiteStarted,
    },
    frame::{
        AgentInfo, CiContext, CiProvider, ClientFrame, Hello, ProtocolRange, ResumeState, Sample,
        ServerFrame, Telemetry,
    },
    journal::{ClockSample, EventEnvelope, Provenance, SourceKind, StepRef, Subject},
};
use futures_util::{SinkExt, StreamExt};
use time::OffsetDateTime;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};
use uuid::Uuid;

/// What to simulate.
#[derive(Debug, Clone)]
pub struct Options {
    /// `ws://host:port` (the path is added).
    pub server: String,
    /// API key or agent token.
    pub token: String,
    /// The CI run all agents belong to.
    pub run_id: String,
    /// Simulated agents.
    pub agents: usize,
    /// Devices (shards) per agent.
    pub devices: u32,
    /// Flows per device.
    pub flows: u32,
    /// Distinct job keys; agents share them round-robin.
    pub job_keys: usize,
    /// Events per batch.
    pub batch: usize,
    /// Drop every connection once, half-way, and reconnect together.
    pub storm: bool,
    /// Resend and shuffle batches.
    pub duplicates: bool,
    /// Pause between batches, to stretch a run over time.
    pub pace: Duration,
    /// Give up on an agent after this long.
    pub deadline: Duration,
    /// The commit the CI run tests; `None` when the CI didn't say.
    pub commit: Option<String>,
    /// The git ref of the CI run; `None` when the CI didn't say.
    pub git_ref: Option<String>,
    /// The CPU architecture the agents report (a fact of the condition fingerprint).
    pub arch: String,
    /// The index of the first agent: agent `first_agent + n` follows [`fails`] as that agent.
    pub first_agent: usize,
    /// The shape of real suites: long flow names, a long failure message and many steps per flow.
    /// Off, flows are short and have two steps each.
    pub wide: bool,
    /// Stop at points of the script until released.
    pub hold: Option<Hold>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            server: "ws://127.0.0.1:8080".into(),
            token: String::new(),
            run_id: "loadgen".into(),
            agents: 50,
            devices: 4,
            flows: 3,
            job_keys: 10,
            batch: 32,
            storm: false,
            duplicates: false,
            pace: Duration::ZERO,
            deadline: Duration::from_secs(120),
            commit: None,
            git_ref: None,
            arch: "x86_64".into(),
            first_agent: 0,
            wide: false,
            hold: None,
        }
    }
}

/// Where in its script an agent waits for [`Hold::release`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldAt {
    /// Right after the `n`th flow (in script order) ended in the live console.
    AfterFlows(usize),
    /// After every flow ended live, before the process exit and the JUnit report: the run is
    /// still open and every outcome is provisional.
    BeforeReport,
}

/// Holds every agent that shares it at the same points of its script, one point per release,
/// in script order. Clones share the gate.
#[derive(Debug, Clone)]
pub struct Hold {
    points: Vec<HoldAt>,
    released: Arc<tokio::sync::watch::Sender<usize>>,
}

impl Hold {
    /// A gate with one stop per point; they are applied in script order.
    #[must_use]
    pub fn new(points: impl IntoIterator<Item = HoldAt>) -> Self {
        Self {
            points: points.into_iter().collect(),
            released: Arc::new(tokio::sync::watch::channel(0).0),
        }
    }

    /// Lets the agents past the next stop (all of them, when no stop is left).
    pub fn release(&self) {
        self.released.send_modify(|n| *n += 1);
    }

    /// The event index of every stop in `script`, ascending.
    fn indices(&self, script: &Script) -> Vec<usize> {
        let mut at: Vec<usize> = self
            .points
            .iter()
            .filter_map(|p| match p {
                HoldAt::AfterFlows(n) => n.checked_sub(1).and_then(|i| script.flow_ends.get(i)),
                HoldAt::BeforeReport => Some(&script.live_end),
            })
            .copied()
            .collect();
        at.sort_unstable();
        at
    }
}

/// What one simulated agent sent and saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReport {
    /// Its agent run.
    pub agent_run_id: Uuid,
    /// Its job key.
    pub job_key: String,
    /// Events it generated (the last `seq`).
    pub events: u64,
    /// The highest cumulative ack it received.
    pub acked: u64,
    /// Connections it opened.
    pub connections: u32,
    /// Events the server refused.
    pub rejected: u32,
    /// Flows that failed in its script.
    pub failed_flows: u32,
    /// Device serials it used.
    pub serials: Vec<String>,
}

/// Everything [`run`] did.
#[derive(Debug, Clone)]
pub struct Report {
    /// Per agent.
    pub agents: Vec<AgentReport>,
    /// Wall time.
    pub elapsed: Duration,
}

impl Report {
    /// Events the server acked, over all agents.
    #[must_use]
    pub fn acked(&self) -> u64 {
        self.agents.iter().map(|a| a.acked).sum()
    }
}

/// Runs the scenario: every agent concurrently, each to completion or its deadline.
pub async fn run(opts: Options) -> anyhow::Result<Report> {
    let t0 = Instant::now();
    let opts = Arc::new(opts);
    let storm = Arc::new(tokio::sync::Barrier::new(opts.agents.max(1)));
    let mut set = tokio::task::JoinSet::new();
    for i in 0..opts.agents {
        let (opts, storm) = (Arc::clone(&opts), Arc::clone(&storm));
        set.spawn(async move { agent(i, &opts, &storm).await });
    }
    let mut agents = Vec::new();
    while let Some(r) = set.join_next().await {
        agents.push(r.context("an agent task panicked")??);
    }
    agents.sort_by_key(|a| a.agent_run_id);
    Ok(Report {
        agents,
        elapsed: t0.elapsed(),
    })
}

/// Whether the scripted flow fails: a stable, sparse pattern.
#[must_use]
pub fn fails(agent: usize, device: u32, flow: u32) -> bool {
    (agent + device as usize * 3 + flow as usize * 5).is_multiple_of(11)
}

/// Steps per flow in a [`Options::wide`] suite.
const WIDE_STEPS: u32 = 24;

fn flow_name(device: u32, flow: u32, wide: bool) -> String {
    if wide {
        format!(
            "flow-{device}-{flow}-checkout-with-saved-card-promo-code-and-gift-wrap-then-receipt-email"
        )
    } else {
        format!("flow-{device}-{flow}")
    }
}

fn step_description(step: u32, wide: bool) -> String {
    if wide {
        format!(
            "Tap on \"step {step}\" and wait until the order summary shows the total with tax and shipping"
        )
    } else {
        format!("Tap on \"step {step}\"")
    }
}

fn failure_message(wide: bool) -> &'static str {
    if wide {
        "Assertion is false: \"Order total\" was expected to read \"$84.10\" but read \"$0.00\" after the promo code was applied; the checkout screen stayed on https://shop.example.test/checkout/review?session=7f3c9d1e2ab4455f8e0c1d9a6b2e7f40&promo=SPRING-FREE-SHIPPING-2026 and the tax request timed out after 30000 ms"
    } else {
        "Assertion is false"
    }
}

fn clock(t0: Instant) -> ClockSample {
    ClockSample {
        wall: OffsetDateTime::now_utc(),
        mono_ms: u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

/// One host sample with fixed readings; the newer optional host fields are left unreported.
fn host_sample(t0: Instant) -> Sample {
    Sample {
        at: clock(t0),
        host_cpu_pct: Some(25.0),
        host_mem_used_bytes: Some(4 << 30),
        host_mem_total_bytes: Some(16 << 30),
        maestro_cpu_pct: Some(80.0),
        maestro_rss_bytes: Some(300 << 20),
        agent_cpu_pct: Some(0.2),
        agent_rss_bytes: Some(8 << 20),
        host_load_1m: None,
        host_cpu_cores: None,
        host_swap_used_bytes: None,
        host_swap_total_bytes: None,
        host_psi_cpu_pct: None,
        host_psi_memory_pct: None,
        host_psi_io_pct: None,
    }
}

struct Script {
    /// The index just after each flow's live end, in script order.
    flow_ends: Vec<usize>,
    /// The index of the process exit: everything before it is the live part.
    live_end: usize,
    events: Vec<EventEnvelope>,
    failed: u32,
    serials: Vec<String>,
}

#[expect(clippy::too_many_lines, reason = "one straight-line event script")]
fn script(i: usize, opts: &Options, t0: Instant) -> Script {
    let mut events = Vec::new();
    // How many events were pushed, readable while `push` holds `events`.
    let pushed = std::cell::Cell::new(0usize);
    let mut push = |subject: Subject, src: SourceKind, body: &EventBody| {
        pushed.set(pushed.get() + 1);
        let seq = u64::try_from(events.len()).unwrap_or(u64::MAX) + 1;
        events.push(EventEnvelope::new(
            seq,
            clock(t0),
            subject,
            Provenance::new(src),
            body,
        ));
    };
    push(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunStarted(RunStarted {
            argv: vec!["maestro".into(), "test".into(), "flows/".into()],
            agent_version: "loadgen".into(),
            evidence: None,
        }),
    );
    let suite = Subject {
        suite_execution_id: Some(Uuid::now_v7()),
        ..Subject::default()
    };
    push(
        suite.clone(),
        SourceKind::Declared,
        &EventBody::SuiteStarted(SuiteStarted {
            shard_mode: if opts.devices > 1 {
                ShardMode::Split
            } else {
                ShardMode::None
            },
            shards: opts.devices,
        }),
    );
    let mut shards = Vec::new();
    let mut serials = Vec::new();
    for d in 0..opts.devices {
        let shard = Subject {
            shard_id: Some(Uuid::now_v7()),
            device_session_id: Some(Uuid::now_v7()),
            ..suite.clone()
        };
        let serial = format!("emulator-{}", 5554 + 2 * d);
        push(
            shard.clone(),
            SourceKind::DebugLog,
            &EventBody::ShardAssigned(ShardAssigned {
                shard_number: d + 1,
                device_serial: serial.clone(),
                planned: (0..opts.flows)
                    .map(|f| {
                        let name = flow_name(d, f, opts.wide);
                        FlowRef {
                            path: Some(format!("flows/{name}.yaml")),
                            name,
                        }
                    })
                    .collect(),
            }),
        );
        serials.push(serial);
        shards.push(shard);
    }
    let mut failed = 0;
    let mut flow_ids = Vec::new();
    let mut flow_ends = Vec::new();
    let steps = if opts.wide { WIDE_STEPS } else { 2 };
    for f in 0..opts.flows {
        for (d, shard) in (0u32..).zip(&shards) {
            let flow = Subject {
                flow_execution_id: Some(Uuid::now_v7()),
                ..shard.clone()
            };
            let name = flow_name(d, f, opts.wide);
            push(
                flow.clone(),
                SourceKind::DebugLog,
                &EventBody::FlowStarted(FlowStarted {
                    flow: FlowRef {
                        name: name.clone(),
                        path: Some(format!("flows/{name}.yaml")),
                    },
                    shard_number: d + 1,
                    ordinal: f,
                }),
            );
            // A failing flow fails its last step, as the scripted failure does.
            let fail = fails(i, d, f);
            let status = |s: u32| {
                if fail && s + 1 == steps {
                    StepStatus::Failed
                } else {
                    StepStatus::Completed
                }
            };
            for s in 0..steps {
                let step = Subject {
                    step: Some(StepRef { index: s }),
                    ..flow.clone()
                };
                push(
                    step.clone(),
                    SourceKind::DebugLog,
                    &EventBody::StepStarted(StepStarted {
                        description: step_description(s, opts.wide),
                        depth: None,
                        parent: None,
                        attempt: None,
                    }),
                );
                push(
                    step,
                    SourceKind::DebugLog,
                    &EventBody::StepEnded(StepEnded {
                        status: status(s),
                        timing: None,
                    }),
                );
            }
            failed += u32::from(fail);
            push(
                flow.clone(),
                SourceKind::LiveConsole,
                &EventBody::FlowEnded(FlowEnded {
                    outcome: if fail {
                        FlowOutcome::Failed
                    } else {
                        FlowOutcome::Passed
                    },
                    duration_ms: Some(1000),
                    failure: None,
                    flow: None,
                }),
            );
            // The final commands.json settles every step the log reported (issue #148).
            for s in 0..steps {
                push(
                    Subject {
                        step: Some(StepRef { index: s }),
                        ..flow.clone()
                    },
                    SourceKind::ArtifactFile,
                    &EventBody::StepEnded(StepEnded {
                        status: status(s),
                        timing: None,
                    }),
                );
            }
            flow_ends.push(pushed.get());
            flow_ids.push((flow, name, d, f, fail));
        }
    }
    let live_end = pushed.get();
    push(
        suite.clone(),
        SourceKind::ProcessExit,
        &EventBody::ProcessExited(ProcessExited {
            code: Some(i32::from(failed > 0)),
            signal: None,
            class: if failed > 0 {
                ExitClass::TestFailures
            } else {
                ExitClass::Passed
            },
        }),
    );
    push(
        suite,
        SourceKind::JunitFinal,
        &EventBody::ReportParsed(ReportParsed {
            status: ReportStatus::Complete,
            suites: opts.devices,
            tests: opts.devices * opts.flows,
            failures: failed,
        }),
    );
    for (flow, name, d, f, fail) in flow_ids {
        push(
            flow,
            SourceKind::JunitFinal,
            &EventBody::FlowReported(FlowReported {
                repo_path: None,
                testsuite_index: d,
                testcase_index: f,
                matched: MatchMethod::Positional,
                junit: JunitTestcase {
                    id: Some(name.clone()),
                    file: Some(format!("flows/{name}.yaml")),
                    name,
                    time_ms: Some(1000),
                    timestamp_local: None,
                    status: Some(if fail { "ERROR" } else { "SUCCESS" }.into()),
                    failure: fail.then(|| failure_message(opts.wide).to_owned()),
                },
                outcome: if fail {
                    FlowOutcome::Failed
                } else {
                    FlowOutcome::Passed
                },
            }),
        );
    }
    push(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunEnded(RunEnded {
            interrupted: false,
            output_bytes_dropped: 0,
        }),
    );
    Script {
        flow_ends,
        live_end,
        events,
        failed,
        serials,
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(opts: &Options) -> anyhow::Result<Ws> {
    let mut req =
        format!("{}/api/v1/agent/ws", opts.server.trim_end_matches('/')).into_client_request()?;
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(WS_SUBPROTOCOL),
    );
    req.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", opts.token))?,
    );
    let (ws, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(req),
    )
    .await??;
    Ok(ws)
}

fn hello(
    id: Uuid,
    job_key: &str,
    opts: &Options,
    next_seq: u64,
    oldest: u64,
    t0: Instant,
) -> Hello {
    Hello {
        protocol: ProtocolRange {
            min: PROTOCOL_VERSION,
            max: PROTOCOL_VERSION,
        },
        agent: AgentInfo {
            version: "loadgen".into(),
            os: "linux".into(),
            arch: opts.arch.clone(),
        },
        agent_run_id: id,
        previous_agent_run_id: None,
        ci: CiContext {
            provider: CiProvider::Generic,
            repository: Some("loadgen/app".into()),
            run_id: Some(opts.run_id.clone()),
            run_attempt: Some(1),
            job_key: job_key.into(),
            job_name: Some(job_key.into()),
            sha: opts.commit.clone(),
            git_ref: opts.git_ref.clone(),
            runner: Some(format!("runner-{id}")),
        },
        resume: ResumeState {
            next_seq,
            oldest_unacked_seq: Some(oldest),
        },
        clock: clock(t0),
    }
}

/// Why a connection ended.
enum Ended {
    Done,
    Dropped,
}

async fn agent(
    i: usize,
    opts: &Options,
    storm: &tokio::sync::Barrier,
) -> anyhow::Result<AgentReport> {
    let t0 = Instant::now();
    let id = Uuid::now_v7();
    let job_key = format!("job-{}", i % opts.job_keys.max(1));
    let s = script(opts.first_agent + i, opts, t0);
    let hold_at = opts
        .hold
        .as_ref()
        .map(|h| h.indices(&s))
        .unwrap_or_default();
    let total = u64::try_from(s.events.len()).unwrap_or(u64::MAX);
    let mut report = AgentReport {
        agent_run_id: id,
        job_key: job_key.clone(),
        events: total,
        acked: 0,
        connections: 0,
        rejected: 0,
        failed_flows: s.failed,
        serials: s.serials,
    };
    let deadline = t0 + opts.deadline;
    let mut stormed = !opts.storm;
    let mut attempt: u32 = 0;
    while report.acked < total {
        if Instant::now() > deadline {
            bail!(
                "agent {i}: only {} of {total} events acked before the deadline",
                report.acked
            );
        }
        match stream(
            opts,
            id,
            &job_key,
            &s.events,
            &hold_at,
            &mut report,
            &mut stormed,
            storm,
            t0,
        )
        .await
        {
            Ok(Ended::Done) => break,
            Ok(Ended::Dropped) | Err(_) => {
                attempt = attempt.saturating_add(1);
                let ceiling = Duration::from_millis(100 * u64::from(attempt.min(10)));
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.subsec_nanos());
                tokio::time::sleep(ceiling.mul_f64(f64::from(nanos % 1000) / 1000.0)).await;
            }
        }
    }
    Ok(report)
}

#[expect(clippy::too_many_arguments, reason = "one connection's whole context")]
async fn stream(
    opts: &Options,
    id: Uuid,
    job_key: &str,
    events: &[EventEnvelope],
    hold_at: &[usize],
    report: &mut AgentReport,
    stormed: &mut bool,
    storm: &tokio::sync::Barrier,
    t0: Instant,
) -> anyhow::Result<Ended> {
    let total = u64::try_from(events.len()).unwrap_or(u64::MAX);
    let mut ws = connect(opts).await?;
    report.connections += 1;
    // The agent holds everything not yet acked: it may have missed acks the server sent.
    let oldest = report.acked + 1;
    let frame = |f: &ClientFrame| Message::text(serde_json::to_string(f).unwrap_or_default());
    ws.send(frame(&ClientFrame::Hello(Box::new(hello(
        id,
        job_key,
        opts,
        total + 1,
        oldest,
        t0,
    )))))
    .await?;
    let mut sent = match recv(&mut ws).await? {
        Some(ServerFrame::Welcome(w)) => w.committed_through,
        other => bail!("expected welcome, got {other:?}"),
    };
    report.acked = report.acked.max(sent);
    let mut tseq = 0u64;
    while report.acked < total {
        if !*stormed && report.acked * 2 >= total {
            // Everyone drops at once, then everyone reconnects at once.
            *stormed = true;
            let _ = ws.close(None).await;
            storm.wait().await;
            return Ok(Ended::Dropped);
        }
        let from = usize::try_from(sent).unwrap_or(usize::MAX);
        let mut to = (from + opts.batch.max(1)).min(events.len());
        if let Some(hold) = &opts.hold {
            // The next stop not yet released: never send past it, and wait there.
            let released = *hold.released.borrow();
            if let Some(&stop) = hold_at.get(released) {
                if from >= stop {
                    let mut rx = hold.released.subscribe();
                    rx.wait_for(|n| *n > released).await?;
                    continue;
                }
                to = to.min(stop);
            }
        }
        let mut batch: Vec<EventEnvelope> = events.get(from..to).unwrap_or_default().to_vec();
        if opts.duplicates && from > 0 {
            // Resend a few already-sent events and shuffle the batch.
            let back = from.saturating_sub(opts.batch / 2);
            batch.extend(events.get(back..from).unwrap_or_default().iter().cloned());
            batch.reverse();
        }
        ws.send(frame(&ClientFrame::Events { events: batch }))
            .await?;
        sent = u64::try_from(to).unwrap_or(u64::MAX);
        tseq += 1;
        ws.send(frame(&ClientFrame::Telemetry(Telemetry {
            tseq_from: tseq,
            tseq_to: tseq,
            dropped_since_last: 0,
            devices: Vec::new(),
            samples: vec![host_sample(t0)],
        })))
        .await?;
        // Stop-and-wait for the cumulative ack of this batch.
        loop {
            match recv(&mut ws).await? {
                Some(ServerFrame::Ack { through, .. }) => {
                    report.acked = report.acked.max(through);
                    if through >= sent {
                        break;
                    }
                }
                Some(ServerFrame::EventRejected { .. }) => report.rejected += 1,
                Some(ServerFrame::Error {
                    expected_seq: Some(e),
                    ..
                }) => {
                    sent = e.saturating_sub(1);
                    break;
                }
                Some(ServerFrame::Goaway { .. } | ServerFrame::Error { .. }) | None => {
                    return Ok(Ended::Dropped);
                }
                Some(_) => {}
            }
        }
        if !opts.pace.is_zero() {
            tokio::time::sleep(opts.pace).await;
        }
    }
    ws.send(frame(&ClientFrame::Bye)).await?;
    let _ = ws.close(None).await;
    Ok(Ended::Done)
}

/// An agent that went quiet: it sent the start of a run and now holds its socket open without
/// another word, like a runner whose process hung. The server shows it `stale`; after
/// [`QuietAgent::disconnect`] it shows `disconnected`.
#[derive(Debug)]
pub struct QuietAgent {
    ws: Ws,
    /// Its agent run.
    pub agent_run_id: Uuid,
}

impl QuietAgent {
    /// Closes the socket without a `bye`: the run did not end, its connection did.
    pub async fn disconnect(mut self) {
        let _ = self.ws.close(None).await;
    }
}

/// Connects one agent, sends the first `events` events of its script (and waits for their ack),
/// then stops talking. `opts.agents` and `opts.pace` are ignored.
pub async fn quiet(opts: &Options, events: usize) -> anyhow::Result<QuietAgent> {
    let t0 = Instant::now();
    let id = Uuid::now_v7();
    let job_key = "job-0".to_owned();
    let s = script(0, opts, t0);
    let total = u64::try_from(s.events.len()).unwrap_or(u64::MAX);
    let mut ws = connect(opts).await?;
    let frame = |f: &ClientFrame| Message::text(serde_json::to_string(f).unwrap_or_default());
    ws.send(frame(&ClientFrame::Hello(Box::new(hello(
        id,
        &job_key,
        opts,
        total + 1,
        1,
        t0,
    )))))
    .await?;
    match recv(&mut ws).await? {
        Some(ServerFrame::Welcome(_)) => {}
        other => bail!("expected welcome, got {other:?}"),
    }
    let sent = events.min(s.events.len());
    ws.send(frame(&ClientFrame::Events {
        events: s.events.get(..sent).unwrap_or_default().to_vec(),
    }))
    .await?;
    let want = u64::try_from(sent).unwrap_or(u64::MAX);
    loop {
        match recv(&mut ws).await? {
            Some(ServerFrame::Ack { through, .. }) if through >= want => break,
            Some(ServerFrame::Error { .. } | ServerFrame::Goaway { .. }) | None => {
                bail!("the server stopped the quiet agent before acking its events")
            }
            Some(_) => {}
        }
    }
    Ok(QuietAgent {
        ws,
        agent_run_id: id,
    })
}

/// The next protocol frame; `None` when the socket closed.
async fn recv(ws: &mut Ws) -> anyhow::Result<Option<ServerFrame>> {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(30), ws.next())
            .await
            .context("no frame within 30 s")?;
        match msg {
            Some(Ok(Message::Text(t))) => return Ok(serde_json::from_str(t.as_str()).ok()),
            Some(Ok(Message::Close(_))) | None => return Ok(None),
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_are_gap_free_and_fail_where_planned() {
        let opts = Options {
            devices: 4,
            flows: 3,
            ..Options::default()
        };
        let s = script(0, &opts, Instant::now());
        for (n, e) in (1u64..).zip(&s.events) {
            assert_eq!(e.seq, n);
        }
        let expected = (0..4)
            .flat_map(|d| (0..3).map(move |f| (d, f)))
            .filter(|&(d, f)| fails(0, d, f))
            .count();
        assert_eq!(s.failed as usize, expected);
        assert_eq!(s.serials.len(), 4);
    }

    #[test]
    fn hold_points_sit_after_live_flow_ends_and_before_the_process_exit() {
        let opts = Options {
            devices: 2,
            flows: 3,
            ..Options::default()
        };
        let s = script(0, &opts, Instant::now());
        let hold = Hold::new([
            HoldAt::BeforeReport,
            HoldAt::AfterFlows(3),
            HoldAt::AfterFlows(0),
        ]);
        let at = hold.indices(&s);
        // AfterFlows(0) is no stop; the others come out in script order.
        assert_eq!(at.len(), 2);
        assert!(at[0] < at[1]);
        // The third flow's steps are settled from its final file before the hold (issue #148),
        // so the hold comes after its last settle.
        assert_eq!(
            s.events[at[0] - 1].kind,
            "step.ended",
            "after the third flow's last settle"
        );
        assert_eq!(
            s.events[at[0] - 1].src.kind,
            SourceKind::ArtifactFile,
            "the settle comes from the final file"
        );
        assert_eq!(s.events[at[1]].kind, "process.exited", "before the exit");
        assert_eq!(
            s.events[..at[1]]
                .iter()
                .filter(|e| e.kind == "flow.ended")
                .count(),
            6,
            "every flow ended live before the report"
        );
    }
}
