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
//! - `reorder`: batches go out in reverse, without resends (the server sorts them);
//! - `gap_events`: a full agent buffer drops events, and one `journal.gap` covers them;
//! - `clock_skew_ms`: the agents' clock reads ahead of (or behind) the server's, wall times and ids;
//! - `steps` and `failure_bytes`: a giant log, with long flows and failures over the agent's limit;
//! - `viewers`: live sockets subscribe to the project like a dashboard and time every flow end
//!   from its send to the frame that shows it finished ([`ViewerReport`]);
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
        AdbState, AppCrashed, EventBody, ExitClass, FlowEnded, FlowOutcome, FlowRef, FlowReported,
        FlowStarted, JournalGap, JunitTestcase, MatchMethod, ProcessExited, ReportParsed,
        ReportStatus, RunEnded, RunStarted, ShardAssigned, ShardMode, StepEnded, StepStarted,
        StepStatus, SuiteStarted,
    },
    frame::{
        AgentInfo, CiContext, CiProvider, ClientFrame, DeviceSample, DeviceUnavailable, Hello,
        ProtocolRange, ResumeState, RunOrigin, Sample, SampleUnavailable, ServerFrame, Telemetry,
        Unavailable,
    },
    journal::{ClockSample, EventEnvelope, Provenance, SourceKind, StepRef, Subject},
};
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use time::OffsetDateTime;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};
use uuid::Uuid;

mod viewer;
pub use viewer::ViewerReport;

/// What to simulate.
#[derive(Debug, Clone)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is one independent switch of the simulation"
)]
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
    /// Send every batch in reverse order, without resends: the server must sort what arrives.
    pub reorder: bool,
    /// Drop this many events half-way through the run, as an agent with a full buffer does. One
    /// `journal.gap` covers them, so the run stays gap-free except for that hole (invariant I5).
    pub gap_events: u32,
    /// The agents' clock reads this many milliseconds ahead of the server's (behind when negative):
    /// every wall time they send and every id they make carries the offset (clock skew, PLAN §13).
    pub clock_skew_ms: i64,
    /// Steps per flow: 0 keeps the shape of [`Options::wide`] (or the short two-step flows).
    pub steps: u32,
    /// Live sockets that subscribe to the project like a dashboard, and measure the ingest-to-live
    /// latency of every flow end (see [`ViewerReport`]). 0 runs no viewers.
    pub viewers: usize,
    /// The project the viewers subscribe to.
    pub project: String,
    /// The viewer credential (a configured viewer token) the live sockets use.
    pub viewer_token: String,
    /// How long the viewers keep reading after the last agent finished, so the last deltas arrive.
    pub linger: Duration,
    /// A PEM file of CA certificates for `wss://` (the roots are this file only). Unset: the
    /// WebSocket library's own roots.
    pub ca_file: Option<std::path::PathBuf>,
    /// Bytes of every failure message: 0 keeps the short or wide message. A message over 192 KiB
    /// is a giant log: the agent sends it as a gap, the load generator does too.
    pub failure_bytes: usize,
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
    /// Hostile strings (the XSS corpus): when non-empty, every flow name, step description and
    /// failure message is one of these, every flow fails with an app crash whose exception is one
    /// of these, and the pattern of [`fails`] is not used. Items are used in order, wrapping around.
    pub corpus: Vec<String>,
    /// The final `commands.json` files line up with none of their flows' steps, so every step stays
    /// provisional, read from the live log (issue #148).
    pub unaligned: bool,
    /// Each flow has one more step: the log opened it and never ended it, and the final
    /// `commands.json` does not list it. The file settles it as unknown, so it is Missing (issue #148).
    pub missing_step: bool,
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
            reorder: false,
            gap_events: 0,
            clock_skew_ms: 0,
            steps: 0,
            failure_bytes: 0,
            viewers: 0,
            project: "mobile-app".into(),
            viewer_token: String::new(),
            linger: Duration::from_secs(3),
            ca_file: None,
            pace: Duration::ZERO,
            deadline: Duration::from_secs(120),
            commit: None,
            git_ref: None,
            arch: "x86_64".into(),
            first_agent: 0,
            wide: false,
            hold: None,
            corpus: Vec::new(),
            unaligned: false,
            missing_step: false,
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
    /// The viewers, when the run had any.
    pub viewers: Option<ViewerReport>,
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
    let probe = Arc::new(viewer::Probe::default());
    let stats = Arc::new(viewer::Stats::default());
    // The viewers subscribe first, so they see every flow end the agents send.
    let mut viewers = tokio::task::JoinSet::new();
    for _ in 0..opts.viewers {
        viewers.spawn(viewer::live(
            Arc::clone(&opts),
            Arc::clone(&probe),
            Arc::clone(&stats),
        ));
    }
    let subscribed = Instant::now();
    while stats.subscribed.load(std::sync::atomic::Ordering::Relaxed) < opts.viewers as u64 {
        if subscribed.elapsed() > Duration::from_secs(30) || viewers.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let storm = Arc::new(tokio::sync::Barrier::new(opts.agents.max(1)));
    let mut set = tokio::task::JoinSet::new();
    for i in 0..opts.agents {
        let (opts, storm, probe) = (Arc::clone(&opts), Arc::clone(&storm), Arc::clone(&probe));
        set.spawn(async move { agent(i, &opts, &storm, &probe).await });
    }
    let mut agents = Vec::new();
    while let Some(r) = set.join_next().await {
        agents.push(r.context("an agent task panicked")??);
    }
    agents.sort_by_key(|a| a.agent_run_id);
    let elapsed = t0.elapsed();
    let viewer_report = if opts.viewers > 0 {
        tokio::time::sleep(opts.linger).await;
        viewers.abort_all();
        Some(viewer::report(opts.viewers, &probe, &stats))
    } else {
        None
    };
    Ok(Report {
        agents,
        elapsed,
        viewers: viewer_report,
    })
}

/// Whether the scripted flow fails: a stable, sparse pattern.
#[must_use]
pub fn fails(agent: usize, device: u32, flow: u32) -> bool {
    (agent + device as usize * 3 + flow as usize * 5).is_multiple_of(11)
}

/// Steps per flow in a [`Options::wide`] suite.
const WIDE_STEPS: u32 = 24;

/// The `n`th item of the hostile corpus, when the run has one.
fn corpus_item(opts: &Options, n: usize) -> Option<&str> {
    (!opts.corpus.is_empty()).then(|| opts.corpus[n % opts.corpus.len()].as_str())
}

fn flow_name(device: u32, flow: u32, opts: &Options) -> String {
    if let Some(item) = corpus_item(opts, (device * opts.flows + flow) as usize) {
        return format!("{item} {device}-{flow}");
    }
    if opts.wide {
        format!(
            "flow-{device}-{flow}-checkout-with-saved-card-promo-code-and-gift-wrap-then-receipt-email"
        )
    } else {
        format!("flow-{device}-{flow}")
    }
}

fn step_description(step: u32, n: usize, opts: &Options) -> String {
    if let Some(item) = corpus_item(opts, n) {
        return item.to_owned();
    }
    if opts.wide {
        format!(
            "Tap on \"step {step}\" and wait until the order summary shows the total with tax and shipping"
        )
    } else {
        format!("Tap on \"step {step}\"")
    }
}

fn failure_message(n: usize, opts: &Options) -> String {
    if let Some(item) = corpus_item(opts, n) {
        return item.to_owned();
    }
    if opts.failure_bytes > 0 {
        // A giant log: the message is `failure_bytes` long.
        let head = "Assertion is false: ";
        return format!(
            "{head}{}",
            "x".repeat(opts.failure_bytes.saturating_sub(head.len()))
        );
    }
    if opts.wide {
        "Assertion is false: \"Order total\" was expected to read \"$84.10\" but read \"$0.00\" after the promo code was applied; the checkout screen stayed on https://shop.example.test/checkout/review?session=7f3c9d1e2ab4455f8e0c1d9a6b2e7f40&promo=SPRING-FREE-SHIPPING-2026 and the tax request timed out after 30000 ms".to_owned()
    } else {
        "Assertion is false".to_owned()
    }
}

/// The agents' clock offset from the server's (`clock_skew_ms`).
fn clock_skew(opts: &Options) -> time::Duration {
    time::Duration::milliseconds(opts.clock_skew_ms)
}

/// A UUIDv7 stamped by the agent's clock. The server checks an agent run's id to be within seven
/// days of its own clock, so a skewed agent makes skewed ids.
fn new_id(skew: time::Duration) -> Uuid {
    let at = OffsetDateTime::now_utc() + skew;
    let secs = u64::try_from(at.unix_timestamp()).unwrap_or(0);
    Uuid::new_v7(uuid::Timestamp::from_unix(
        uuid::NoContext,
        secs,
        at.nanosecond(),
    ))
}

fn clock(t0: Instant, skew: time::Duration) -> ClockSample {
    ClockSample {
        wall: OffsetDateTime::now_utc() + skew,
        mono_ms: u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

/// One host sample with fixed readings; the newer optional host fields are left unreported.
/// Every device the run's events name, reported in adb's `device` state, which is what an emulator
/// that is up reads (issue #123: the overview's active emulators count comes from these samples).
fn device_samples(events: &[EventEnvelope], t0: Instant, opts: &Options) -> Vec<DeviceSample> {
    let devices: std::collections::BTreeSet<Uuid> = events
        .iter()
        .filter_map(|e| e.subject.device_session_id)
        .collect();
    devices
        .into_iter()
        .enumerate()
        .map(|(n, device_session_id)| DeviceSample {
            // The corpus names a device's model and OS version too, when the run has one.
            model: corpus_item(opts, n).map(str::to_owned),
            os_version: corpus_item(opts, n + 1).map(str::to_owned),
            at: clock(t0, clock_skew(opts)),
            device_session_id,
            adb_state: Some(AdbState::Device),
            boot_completed: None,
            guest_cpu_pct: None,
            guest_mem_available_bytes: None,
            emu_cpu_pct: None,
            emu_rss_bytes: None,
            avd_name: None,
            api_level: None,
            foreground_activity: None,
            shard: Some(1),
            // The load generator sends no probe, so no reading of this sample was sampled.
            unavailable: DeviceUnavailable {
                adb_state: None,
                boot_completed: Some(Unavailable::NotSampled),
                guest_cpu_pct: Some(Unavailable::NotSampled),
                guest_mem_available_bytes: Some(Unavailable::NotSampled),
                emu_cpu_pct: Some(Unavailable::NotSampled),
                emu_rss_bytes: Some(Unavailable::NotSampled),
                avd_name: Some(Unavailable::NotSampled),
                api_level: Some(Unavailable::NotSampled),
                foreground_activity: Some(Unavailable::NotSampled),
            },
        })
        .collect()
}

fn host_sample(t0: Instant, skew: time::Duration) -> Sample {
    Sample {
        at: clock(t0, skew),
        host_cpu_pct: Some(25.0),
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
        // The load generator takes none of these readings, so the sample does not take them.
        unavailable: SampleUnavailable {
            host_load_1m: Some(Unavailable::NotSampled),
            host_cpu_cores: Some(Unavailable::NotSampled),
            host_swap_used_bytes: Some(Unavailable::NotSampled),
            host_swap_total_bytes: Some(Unavailable::NotSampled),
            host_psi_cpu_pct: Some(Unavailable::NotSampled),
            host_psi_memory_pct: Some(Unavailable::NotSampled),
            host_psi_io_pct: Some(Unavailable::NotSampled),
            ..SampleUnavailable::default()
        },
    }
}

struct Script {
    /// The index just after each flow's live end, in script order.
    flow_ends: Vec<usize>,
    /// The index of the process exit: everything before it is the live part.
    live_end: usize,
    events: Vec<EventEnvelope>,
    /// Indices the agent dropped (see [`make_hole`]): never sent.
    hole: std::ops::Range<usize>,
    failed: u32,
    serials: Vec<String>,
}

/// Drops `count` events half-way through the run, as an agent with a full buffer does, and keeps
/// their slots: the last slot becomes a `journal.gap` covering the whole span (the agent's
/// `spool_full`). Returns the indices the wire does not carry.
fn make_hole(events: &mut [EventEnvelope], count: usize) -> std::ops::Range<usize> {
    if count == 0 || events.len() < count + 2 {
        return 0..0;
    }
    let start = (events.len() - count) / 2;
    let end = start + count;
    let (first, last) = (events[start].seq, events[end - 1].seq);
    let body = EventBody::JournalGap(JournalGap {
        from_seq: first,
        to_seq: last,
        reason: "spool_full".into(),
    });
    let slot = &mut events[end - 1];
    *slot = EventEnvelope::new(
        last,
        slot.at,
        Subject::default(),
        Provenance::new(SourceKind::AgentObserved),
        &body,
    );
    start..end - 1
}

#[expect(clippy::too_many_lines, reason = "one straight-line event script")]
fn script(i: usize, opts: &Options, t0: Instant) -> Script {
    let skew = clock_skew(opts);
    let mut events = Vec::new();
    // How many events were pushed, readable while `push` holds `events`.
    let pushed = std::cell::Cell::new(0usize);
    let mut push = |subject: Subject, src: SourceKind, body: &EventBody| {
        pushed.set(pushed.get() + 1);
        let seq = u64::try_from(events.len()).unwrap_or(u64::MAX) + 1;
        events.push(EventEnvelope::new(
            seq,
            clock(t0, skew),
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
            reproduction: None,
            evidence: None,
        }),
    );
    let suite = Subject {
        suite_execution_id: Some(new_id(skew)),
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
            shard_id: Some(new_id(skew)),
            device_session_id: Some(new_id(skew)),
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
                        let name = flow_name(d, f, opts);
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
    let steps = if opts.steps > 0 {
        opts.steps
    } else if opts.wide {
        WIDE_STEPS
    } else {
        2
    };
    for f in 0..opts.flows {
        for (d, shard) in (0u32..).zip(&shards) {
            let flow = Subject {
                flow_execution_id: Some(new_id(skew)),
                ..shard.clone()
            };
            let name = flow_name(d, f, opts);
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
            // A corpus run fails every flow, so every flow's failure and crash carry an item.
            let fail = !opts.corpus.is_empty() || fails(i, d, f);
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
                let n = (d * opts.flows + f) as usize * steps as usize + s as usize;
                push(
                    step.clone(),
                    SourceKind::DebugLog,
                    &EventBody::StepStarted(StepStarted {
                        description: step_description(s, n, opts),
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
            if let (true, Some(item)) = (fail, corpus_item(opts, (d * opts.flows + f) as usize)) {
                push(
                    flow.clone(),
                    SourceKind::ArtifactFile,
                    &EventBody::AppCrashed(AppCrashed {
                        package: "com.example.app".into(),
                        exception: item.to_owned(),
                    }),
                );
            }
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
            // The final commands.json settles every step the log reported (issue #148), unless its
            // steps line up with none of the log's.
            for s in (0..steps).filter(|_| !opts.unaligned) {
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
            if opts.missing_step {
                let open = Subject {
                    step: Some(StepRef { index: steps }),
                    ..flow.clone()
                };
                push(
                    open.clone(),
                    SourceKind::DebugLog,
                    &EventBody::StepStarted(StepStarted {
                        description: "Assert that \"Ghost\" is visible".to_owned(),
                        depth: None,
                        parent: None,
                        attempt: None,
                    }),
                );
                push(
                    open,
                    SourceKind::ArtifactFile,
                    &EventBody::StepEnded(StepEnded {
                        status: StepStatus::Unknown,
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
                    failure: fail.then(|| failure_message((d * opts.flows + f) as usize, opts)),
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
    oversized_as_gaps(&mut events);
    let hole = make_hole(&mut events, opts.gap_events as usize);
    Script {
        flow_ends,
        live_end,
        events,
        hole,
        failed,
        serials,
    }
}

pub(crate) type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The TLS roots of `--ca-file`: that file's certificates, and no others.
fn ca_connector(path: &std::path::Path) -> anyhow::Result<tokio_tungstenite::Connector> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(path)
        .with_context(|| format!("reading the CA file {}", path.display()))?
    {
        roots
            .add(cert.with_context(|| format!("parsing the CA file {}", path.display()))?)
            .with_context(|| format!("adding a CA from {}", path.display()))?;
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
}

/// Opens a WebSocket on `path` with `token` as the bearer credential. Agents name the protocol
/// subprotocol; viewers do not.
pub(crate) async fn connect_ws(
    opts: &Options,
    path: &str,
    token: &str,
    agent_protocol: bool,
) -> anyhow::Result<Ws> {
    let mut req = format!("{}{path}", opts.server.trim_end_matches('/')).into_client_request()?;
    if agent_protocol {
        req.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static(WS_SUBPROTOCOL),
        );
    }
    req.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    let connector = match &opts.ca_file {
        Some(path) => Some(ca_connector(path)?),
        None => None,
    };
    let (ws, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async_tls_with_config(req, None, false, connector),
    )
    .await??;
    Ok(ws)
}

async fn connect(opts: &Options) -> anyhow::Result<Ws> {
    connect_ws(opts, "/api/v1/agent/ws", &opts.token, true).await
}

/// The workspace catalog of a corpus run: the first few items name flows that have not run, with
/// the item as their tag (a tag is at most 64 bytes, as the agent bounds them).
fn corpus_catalog(opts: &Options) -> fleet_protocol::frame::FlowCatalog {
    let flows = opts
        .corpus
        .iter()
        .enumerate()
        .filter(|(_, tag)| tag.len() <= fleet_protocol::frame::MAX_CATALOG_TAG_BYTES)
        .take(4)
        .map(|(n, tag)| fleet_protocol::frame::CatalogFlow {
            path: format!("flows/corpus-{n}.yaml"),
            name: None,
            tags: vec![tag.clone()],
        })
        .collect();
    fleet_protocol::frame::FlowCatalog {
        flows,
        truncated: false,
    }
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
        catalog: corpus_catalog(opts),
        resume: ResumeState {
            next_seq,
            oldest_unacked_seq: Some(oldest),
        },
        clock: clock(t0, clock_skew(opts)),
        origin: RunOrigin::Live,
        started_at: None,
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
    probe: &viewer::Probe,
) -> anyhow::Result<AgentReport> {
    let t0 = Instant::now();
    let id = new_id(clock_skew(opts));
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
            &s.hole,
            &hold_at,
            &mut report,
            &mut stormed,
            storm,
            probe,
            t0,
        )
        .await
        {
            Ok(Ended::Done) => break,
            Ok(Ended::Dropped) | Err(_) => {
                attempt = attempt.saturating_add(1);
                let ceiling = Duration::from_millis(100 * u64::from(attempt.min(10)));
                tokio::time::sleep(ceiling.mul_f64(jitter_fraction())).await;
            }
        }
    }
    Ok(report)
}

/// The largest event the agent sends whole. A bigger one is not stored: the agent sends a
/// `journal.gap` in its slot instead (`max_event_bytes` in the agent's outbox).
pub const MAX_EVENT_BYTES: usize = 192 * 1024;

/// Bytes a frame takes besides its events (the envelope and the `op` field).
const FRAME_OVERHEAD: usize = 1024;

/// The serialized size of one event, as an `events` frame carries it.
fn event_bytes(e: &EventEnvelope) -> usize {
    serde_json::to_vec(e).map_or(0, |v| v.len())
}

/// Where the batch that starts at `from` ends: `opts.batch` events at most, and no more bytes than
/// `budget` unless one event alone is larger (the agent packs frames the same way).
fn batch_end(events: &[EventEnvelope], from: usize, opts: &Options, budget: usize) -> usize {
    let mut to = from;
    let mut bytes = 0usize;
    while to < events.len() && to - from < opts.batch.max(1) {
        let size = event_bytes(&events[to]);
        if to > from && bytes.saturating_add(size) > budget {
            break;
        }
        bytes = bytes.saturating_add(size);
        to += 1;
    }
    to
}

/// The frame budget for events, from the server's `max_frame_bytes`.
fn frame_budget(max_frame_bytes: u32) -> usize {
    usize::try_from(max_frame_bytes)
        .unwrap_or(usize::MAX)
        .saturating_sub(FRAME_OVERHEAD)
}

/// The events one batch puts on the wire: `events[from..to]`, with the resends and the reordering
/// the scenario asks for. Resends stop at `budget` bytes.
fn wire_batch(
    events: &[EventEnvelope],
    from: usize,
    to: usize,
    opts: &Options,
    budget: usize,
    hole: &std::ops::Range<usize>,
) -> Vec<EventEnvelope> {
    let mut batch: Vec<EventEnvelope> = (from..to)
        .filter(|i| !hole.contains(i))
        .filter_map(|i| events.get(i).cloned())
        .collect();
    if opts.duplicates && from > 0 {
        // Resend a few already-sent events, the nearest ones that the frame has room for.
        let back = from.saturating_sub(opts.batch / 2);
        let mut room = budget.saturating_sub(batch.iter().map(event_bytes).sum());
        let mut resend = Vec::new();
        for e in (back..from)
            .rev()
            .filter(|i| !hole.contains(i))
            .filter_map(|i| events.get(i))
        {
            let size = event_bytes(e);
            if size > room {
                break;
            }
            room -= size;
            resend.push(e.clone());
        }
        resend.reverse();
        batch.extend(resend);
    }
    if opts.duplicates || opts.reorder {
        // The batch goes out in reverse: the server sorts it by `seq` before it applies it.
        batch.reverse();
    }
    batch
}

/// Stores every event bigger than [`MAX_EVENT_BYTES`] as a `journal.gap` in its own slot, as the
/// agent does, so a giant log is recorded as a gap and never refused as a frame.
fn oversized_as_gaps(events: &mut [EventEnvelope]) {
    for e in events.iter_mut() {
        if event_bytes(e) <= MAX_EVENT_BYTES {
            continue;
        }
        let body = EventBody::JournalGap(JournalGap {
            from_seq: e.seq,
            to_seq: e.seq,
            reason: "event_too_large".into(),
        });
        *e = EventEnvelope::new(
            e.seq,
            e.at,
            Subject::default(),
            Provenance::new(SourceKind::AgentObserved),
            &body,
        );
    }
}

/// A reconnect's jitter: a fraction in `[0, 1)` from OS randomness. The clock's sub-second bits
/// are no source: macOS clocks have microsecond resolution, so their nanoseconds are multiples of
/// 1000 and `nanos % 1000` was always zero, which retried a refused reconnect at once (issue #138;
/// the agent and the server keep their own one-line copies).
fn jitter_fraction() -> f64 {
    let permille = getrandom::u64().map_or(0, |r| r % 1000);
    f64::from(u32::try_from(permille).unwrap_or(0)) / 1000.0
}

#[expect(clippy::too_many_arguments, reason = "one connection's whole context")]
async fn stream(
    opts: &Options,
    id: Uuid,
    job_key: &str,
    events: &[EventEnvelope],
    hole: &std::ops::Range<usize>,
    hold_at: &[usize],
    report: &mut AgentReport,
    stormed: &mut bool,
    storm: &tokio::sync::Barrier,
    probe: &viewer::Probe,
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
    let (mut sent, budget) = match recv(&mut ws).await? {
        Some(ServerFrame::Welcome(w)) => {
            (w.committed_through, frame_budget(w.limits.max_frame_bytes))
        }
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
        let mut to = batch_end(events, from, opts, budget);
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
        let batch = wire_batch(events, from, to, opts, budget, hole);
        // The latency clock of each flow end starts before its batch leaves.
        let now = Instant::now();
        for e in batch
            .iter()
            .filter(|e| e.kind == "flow.ended" && e.src.kind == SourceKind::LiveConsole)
        {
            if let Some(flow) = e.subject.flow_execution_id {
                probe.sent(flow, now);
            }
        }
        sent = u64::try_from(to).unwrap_or(u64::MAX);
        // The server acks the last `seq` it applied: a batch that ends in the hole acks below `to`.
        let want = batch.iter().map(|e| e.seq).max().unwrap_or(sent);
        if batch.is_empty() && to < events.len() {
            // Every event of this span is in the hole: nothing is sent for it.
            continue;
        }
        ws.send(frame(&ClientFrame::Events { events: batch }))
            .await?;
        tseq += 1;
        ws.send(frame(&ClientFrame::Telemetry(Telemetry {
            tseq_from: tseq,
            tseq_to: tseq,
            dropped_since_last: 0,
            devices: device_samples(events, t0, opts),
            samples: vec![host_sample(t0, clock_skew(opts))],
        })))
        .await?;
        // Stop-and-wait for the cumulative ack of this batch.
        loop {
            match recv(&mut ws).await? {
                Some(ServerFrame::Ack { through, .. }) => {
                    report.acked = report.acked.max(through);
                    if through >= want {
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
    let id = new_id(clock_skew(opts));
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
    fn reconnect_jitter_varies_on_every_clock() {
        // Issue #138: the clock's nanoseconds are always multiples of 1000 on macOS, so a jitter
        // taken from them is zero every time. OS randomness must vary.
        let fractions: Vec<f64> = (0..64).map(|_| jitter_fraction()).collect();
        assert!(fractions.iter().all(|f| (0.0..1.0).contains(f)));
        assert!(
            fractions.iter().any(|f| *f > 0.0),
            "the jitter never left zero"
        );
    }

    fn seqs(events: &[EventEnvelope]) -> Vec<u64> {
        events.iter().map(|e| e.seq).collect()
    }

    #[test]
    fn a_batch_goes_out_in_reverse_only_when_the_scenario_reorders_it() {
        let s = script(0, &Options::default(), Instant::now());
        let plain = wire_batch(&s.events, 0, 4, &Options::default(), usize::MAX, &(0..0));
        assert_eq!(seqs(&plain), [1, 2, 3, 4]);
        let reordered = wire_batch(
            &s.events,
            0,
            4,
            &Options {
                reorder: true,
                ..Options::default()
            },
            usize::MAX,
            &(0..0),
        );
        assert_eq!(seqs(&reordered), [4, 3, 2, 1]);
    }

    #[test]
    fn duplicates_resend_the_half_batch_before_it_and_shuffle_all() {
        let opts = Options {
            batch: 4,
            duplicates: true,
            ..Options::default()
        };
        let s = script(0, &opts, Instant::now());
        // Events 5..=8 go out with 3 and 4 resent, all reversed.
        let batch = wire_batch(&s.events, 4, 8, &opts, usize::MAX, &(0..0));
        assert_eq!(seqs(&batch), [4, 3, 8, 7, 6, 5]);
    }

    #[test]
    fn clock_skew_moves_every_wall_time_and_every_agent_id() {
        let opts = Options {
            clock_skew_ms: 600_000,
            ..Options::default()
        };
        let s = script(0, &opts, Instant::now());
        let now = OffsetDateTime::now_utc().unix_timestamp();
        // Ten minutes ahead, to within a few seconds of this test's clock reading.
        let ahead = |secs: i64| (secs - now - 600).abs() <= 5;
        assert!(ahead(s.events[0].at.wall.unix_timestamp()));
        let suite = s.events[1].subject.suite_execution_id.expect("a suite id");
        let stamped = suite.get_timestamp().expect("a v7 id").to_unix().0;
        assert!(ahead(i64::try_from(stamped).unwrap_or(0)));
    }

    #[test]
    fn a_giant_failure_is_sent_as_a_gap_and_steps_are_counted() {
        let opts = Options {
            failure_bytes: 300_000,
            steps: 40,
            devices: 2,
            flows: 2,
            ..Options::default()
        };
        let s = script(0, &opts, Instant::now());
        assert!(s.events.iter().all(|e| event_bytes(e) <= MAX_EVENT_BYTES));
        // Each failing flow reports its failure, too big to send: that report is a gap.
        let gaps = s.events.iter().filter(|e| e.kind == "journal.gap").count();
        assert_eq!(gaps, s.failed as usize);
        let steps = s.events.iter().filter(|e| e.kind == "step.started").count();
        assert_eq!(steps, 2 * 2 * 40);
    }

    #[test]
    fn a_hole_is_one_journal_gap_in_the_last_slot_of_its_span() {
        let opts = Options {
            gap_events: 5,
            ..Options::default()
        };
        let s = script(0, &opts, Instant::now());
        let n = s.events.len();
        // Five events starting half-way through the run: four are dropped, the fifth is the gap.
        assert_eq!(s.hole.len(), 4);
        let marker = s.hole.end;
        assert_eq!(s.events[marker].kind, "journal.gap");
        assert_eq!(s.events[marker].seq, marker as u64 + 1);
        assert_eq!(s.events[marker].data["from_seq"], s.hole.start as u64 + 1);
        assert_eq!(s.events[marker].data["to_seq"], marker as u64 + 1);
        // Batch by batch, the wire carries every seq but the dropped ones, in order.
        let mut wire = Vec::new();
        let mut from = 0;
        while from < n {
            let to = batch_end(&s.events, from, &opts, usize::MAX);
            wire.extend(wire_batch(&s.events, from, to, &opts, usize::MAX, &s.hole));
            from = to;
        }
        let expected: Vec<u64> = (1..=n as u64)
            .filter(|q| *q <= s.hole.start as u64 || *q > marker as u64)
            .collect();
        assert_eq!(seqs(&wire), expected);
    }

    /// A journal event whose exception is `bytes` long.
    fn big_event(seq: u64, bytes: usize) -> EventEnvelope {
        EventEnvelope::new(
            seq,
            clock(Instant::now(), time::Duration::ZERO),
            Subject::default(),
            Provenance::new(SourceKind::AgentObserved),
            &EventBody::AppCrashed(AppCrashed {
                package: "com.example.app".into(),
                exception: "x".repeat(bytes),
            }),
        )
    }

    #[test]
    fn a_batch_stops_at_the_frame_budget_but_takes_one_event_that_alone_exceeds_it() {
        let events: Vec<EventEnvelope> = (1..=3).map(|s| big_event(s, 100_000)).collect();
        let opts = Options {
            batch: 32,
            ..Options::default()
        };
        // Two 100 KB events fit a 256 KB frame; a third does not.
        assert_eq!(batch_end(&events, 0, &opts, frame_budget(256 * 1024)), 2);
        // An event over the whole budget still goes out, alone.
        assert_eq!(batch_end(&events, 0, &opts, 10_000), 1);
    }

    #[test]
    fn an_event_over_192_kib_goes_out_as_a_gap_in_its_own_slot() {
        let mut events = vec![
            big_event(1, 10),
            big_event(2, MAX_EVENT_BYTES + 1),
            big_event(3, 10),
        ];
        oversized_as_gaps(&mut events);
        assert_eq!(events[0].kind, "app.crashed");
        assert_eq!(events[1].kind, "journal.gap");
        assert_eq!(events[1].seq, 2);
        assert_eq!(events[1].data["from_seq"], 2);
        assert_eq!(events[1].data["to_seq"], 2);
        assert_eq!(events[2].kind, "app.crashed");
    }

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
