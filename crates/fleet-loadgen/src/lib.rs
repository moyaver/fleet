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
//! - a server restart needs nothing special: agents reconnect until `deadline`.
//!
//! [`run`] returns what each agent knows, so a test can compare it with the server's state.

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
        }
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

fn clock(t0: Instant) -> ClockSample {
    ClockSample {
        wall: OffsetDateTime::now_utc(),
        mono_ms: u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

struct Script {
    events: Vec<EventEnvelope>,
    failed: u32,
    serials: Vec<String>,
}

#[expect(clippy::too_many_lines, reason = "one straight-line event script")]
fn script(i: usize, opts: &Options, t0: Instant) -> Script {
    let mut events = Vec::new();
    let mut push = |subject: Subject, src: SourceKind, body: &EventBody| {
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
                    .map(|f| FlowRef {
                        name: format!("flow-{d}-{f}"),
                        path: Some(format!("flows/flow-{d}-{f}.yaml")),
                    })
                    .collect(),
            }),
        );
        serials.push(serial);
        shards.push(shard);
    }
    let mut failed = 0;
    let mut flow_ids = Vec::new();
    for f in 0..opts.flows {
        for (d, shard) in (0u32..).zip(&shards) {
            let flow = Subject {
                flow_execution_id: Some(Uuid::now_v7()),
                ..shard.clone()
            };
            let name = format!("flow-{d}-{f}");
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
            for s in 0..2 {
                let step = Subject {
                    step: Some(StepRef { index: s }),
                    ..flow.clone()
                };
                push(
                    step.clone(),
                    SourceKind::DebugLog,
                    &EventBody::StepStarted(StepStarted {
                        description: format!("Tap on \"step {s}\""),
                    }),
                );
                push(
                    step,
                    SourceKind::DebugLog,
                    &EventBody::StepEnded(StepEnded {
                        status: StepStatus::Completed,
                    }),
                );
            }
            let fail = fails(i, d, f);
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
            flow_ids.push((flow, name, d, f, fail));
        }
    }
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
                    failure: fail.then(|| "Assertion is false".into()),
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
            arch: "x86_64".into(),
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
            sha: None,
            git_ref: None,
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
    let s = script(i, opts, t0);
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
        let to = (from + opts.batch.max(1)).min(events.len());
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
            samples: vec![Sample {
                at: clock(t0),
                host_cpu_pct: Some(25.0),
                host_mem_used_bytes: Some(4 << 30),
                host_mem_total_bytes: Some(16 << 30),
                maestro_cpu_pct: Some(80.0),
                maestro_rss_bytes: Some(300 << 20),
                agent_cpu_pct: Some(0.2),
                agent_rss_bytes: Some(8 << 20),
            }],
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
}
