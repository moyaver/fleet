// SPDX-License-Identifier: AGPL-3.0-only
//! Ingest transaction tests against Postgres 18 (`#[sqlx::test]`: a fresh database each).
//!
//! Events here are protocol events built by hand, not Maestro output: the fixture-driven tests
//! (real agent, real fixtures) live in fleet-server and fleet-e2e.

use fleet_protocol::{
    PROTOCOL_VERSION,
    event::{
        AppCrashed, ExitClass, FailureInfo, FlowEnded, FlowOutcome, FlowRef, FlowReported,
        FlowStarted, JournalGap, JunitTestcase, MatchMethod, ProcessExited, ReportParsed,
        ReportStatus, RunEnded, RunStarted, ShardAssigned, ShardMode, StepEnded, StepStarted,
        StepStatus, StepTiming, SuiteStarted,
    },
    frame::{AgentInfo, CiContext, ProtocolRange, ResumeState},
    journal::{ClockSample, Provenance, SourceKind, StepRef, Subject},
    rest::CrashView,
};
use sqlx::PgPool;

use super::*;
use crate::auth::TrustLevel;

fn now() -> ClockSample {
    ClockSample {
        wall: OffsetDateTime::now_utc(),
        mono_ms: 0,
    }
}

/// A simulated agent run: assigns `seq` like the agent's outbox.
pub(crate) struct Agent {
    pub(crate) id: Uuid,
    pub(crate) seq: u64,
    pub(crate) job_key: String,
    pub(crate) run_id: String,
}

impl Agent {
    pub(crate) fn new(run_id: &str, job_key: &str) -> Self {
        Self {
            id: Uuid::now_v7(),
            seq: 0,
            job_key: job_key.into(),
            run_id: run_id.into(),
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
                job_name: Some(self.job_key.clone()),
                sha: None,
                git_ref: None,
                runner: Some(format!("runner-{}", self.job_key)),
            },
            resume: ResumeState {
                next_seq: self.seq + 1,
                oldest_unacked_seq: oldest_unacked,
            },
            clock: now(),
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
}

/// IDs of one simulated suite with one shard on one device.
#[derive(Clone, Copy)]
pub(crate) struct SuiteIds {
    pub(crate) suite: Uuid,
    pub(crate) shard: Uuid,
    pub(crate) device: Uuid,
}

impl SuiteIds {
    pub(crate) fn new() -> Self {
        Self {
            suite: Uuid::now_v7(),
            shard: Uuid::now_v7(),
            device: Uuid::now_v7(),
        }
    }

    pub(crate) fn suite(&self) -> Subject {
        Subject {
            suite_execution_id: Some(self.suite),
            ..Subject::default()
        }
    }

    pub(crate) fn shard(&self) -> Subject {
        Subject {
            shard_id: Some(self.shard),
            device_session_id: Some(self.device),
            ..self.suite()
        }
    }

    pub(crate) fn flow(&self, flow: Uuid) -> Subject {
        Subject {
            flow_execution_id: Some(flow),
            ..self.shard()
        }
    }
}

/// A whole run of `flows` (name, passed?) on one device, ending normally.
pub(crate) fn suite_run(
    a: &mut Agent,
    ids: SuiteIds,
    flows: &[(&str, bool)],
) -> Vec<EventEnvelope> {
    let scripts: Vec<Script<'_>> = flows
        .iter()
        .map(|(name, passed)| Script {
            name,
            failure: (!passed).then_some("Assertion is false"),
            failed_step: None,
        })
        .collect();
    suite_run_scripted(a, ids, &scripts)
}

/// One flow of a scripted run: how it ends.
#[derive(Clone, Copy)]
pub(crate) struct Script<'a> {
    pub(crate) name: &'a str,
    /// `None` passes; `Some(message)` fails with that message.
    pub(crate) failure: Option<&'a str>,
    /// The description of the step that failed, when the script has one.
    pub(crate) failed_step: Option<&'a str>,
}

/// A whole run of scripted flows on one device, ending normally.
#[expect(clippy::too_many_lines, reason = "one straight-line event script")]
pub(crate) fn suite_run_scripted(
    a: &mut Agent,
    ids: SuiteIds,
    flows: &[Script<'_>],
) -> Vec<EventEnvelope> {
    let mut out = vec![
        a.event(
            Subject::default(),
            SourceKind::AgentObserved,
            &EventBody::RunStarted(RunStarted {
                argv: vec!["maestro".into(), "test".into()],
                agent_version: "0.0.0".into(),
                evidence: None,
            }),
        ),
        a.event(
            ids.suite(),
            SourceKind::Declared,
            &EventBody::SuiteStarted(SuiteStarted {
                shard_mode: ShardMode::None,
                shards: 1,
            }),
        ),
        a.event(
            ids.shard(),
            SourceKind::DebugLog,
            &EventBody::ShardAssigned(ShardAssigned {
                shard_number: 1,
                device_serial: "emulator-5554".into(),
                planned: flows
                    .iter()
                    .map(|s| FlowRef {
                        name: s.name.into(),
                        path: Some(format!("flows/{}.yaml", s.name)),
                    })
                    .collect(),
            }),
        ),
    ];
    let mut flow_ids = Vec::new();
    for (i, s) in flows.iter().enumerate() {
        let (name, passed) = (s.name, s.failure.is_none());
        let f = Uuid::now_v7();
        flow_ids.push(f);
        out.push(a.event(
            ids.flow(f),
            SourceKind::DebugLog,
            &EventBody::FlowStarted(FlowStarted {
                flow: FlowRef {
                    name: name.into(),
                    path: Some(format!("flows/{name}.yaml")),
                },
                shard_number: 1,
                ordinal: u32::try_from(i).unwrap(),
            }),
        ));
        if let Some(step) = s.failed_step {
            let subject = Subject {
                step: Some(StepRef { index: 0 }),
                ..ids.flow(f)
            };
            out.push(a.event(
                subject.clone(),
                SourceKind::LiveConsole,
                &EventBody::StepStarted(StepStarted {
                    description: step.into(),
                    depth: None,
                    parent: None,
                    attempt: None,
                }),
            ));
            out.push(a.event(
                subject,
                SourceKind::LiveConsole,
                &EventBody::StepEnded(StepEnded {
                    status: StepStatus::Failed,
                    timing: None,
                }),
            ));
        }
        out.push(a.event(
            ids.flow(f),
            SourceKind::LiveConsole,
            &EventBody::FlowEnded(FlowEnded {
                outcome: if passed {
                    FlowOutcome::Passed
                } else {
                    FlowOutcome::Failed
                },
                duration_ms: Some(1000),
                failure: s.failure.map(|m| FailureInfo { message: m.into() }),
                flow: None,
            }),
        ));
    }
    let all_passed = flows.iter().all(|s| s.failure.is_none());
    out.push(a.event(
        ids.suite(),
        SourceKind::ProcessExit,
        &EventBody::ProcessExited(ProcessExited {
            code: Some(i32::from(!all_passed)),
            signal: None,
            class: if all_passed {
                ExitClass::Passed
            } else {
                ExitClass::TestFailures
            },
        }),
    ));
    out.push(a.event(
        ids.suite(),
        SourceKind::JunitFinal,
        &EventBody::ReportParsed(ReportParsed {
            status: ReportStatus::Complete,
            suites: 1,
            tests: u32::try_from(flows.len()).unwrap(),
            failures: u32::try_from(flows.iter().filter(|s| s.failure.is_some()).count()).unwrap(),
        }),
    ));
    for (i, (s, f)) in flows.iter().zip(&flow_ids).enumerate() {
        let (name, passed) = (s.name, s.failure.is_none());
        out.push(a.event(
            ids.flow(*f),
            SourceKind::JunitFinal,
            &EventBody::FlowReported(FlowReported {
                repo_path: None,
                testsuite_index: 0,
                testcase_index: u32::try_from(i).unwrap(),
                matched: MatchMethod::Positional,
                junit: JunitTestcase {
                    id: Some(name.into()),
                    name: name.into(),
                    file: Some(format!("flows/{name}.yaml")),
                    time_ms: Some(1000),
                    timestamp_local: None,
                    status: Some(if passed { "SUCCESS" } else { "ERROR" }.into()),
                    failure: s.failure.map(Into::into),
                },
                outcome: if passed {
                    FlowOutcome::Passed
                } else {
                    FlowOutcome::Failed
                },
            }),
        ));
    }
    out.push(a.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunEnded(RunEnded {
            interrupted: false,
            output_bytes_dropped: 0,
        }),
    ));
    out
}

pub(crate) async fn publisher(store: &Store) -> Publisher {
    let project = store.upsert_project("app", "App").await.unwrap();
    let (key, _) = store
        .create_api_key(
            project,
            "ci",
            TrustLevel::Trusted,
            time::OffsetDateTime::now_utc() + time::Duration::days(90),
        )
        .await
        .unwrap();
    Publisher {
        project_id: project,
        credential: crate::auth::Credential::ApiKey(key),
        trust: TrustLevel::Trusted,
        claims: None,
    }
}

async fn flows(store: &Store, run: Uuid) -> Vec<(String, String, String, Option<String>)> {
    sqlx::query!(
        "SELECT flow_name, state, finality::text AS \"finality!\", outcome_source::text AS source
           FROM flow_executions WHERE agent_run_id = $1 ORDER BY ordinal, flow_name",
        run
    )
    .fetch_all(store.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.flow_name, r.state, r.finality, r.source))
    .collect()
}

async fn job(store: &Store, id: Uuid) -> (String, Option<String>) {
    let r = sqlx::query!("SELECT lifecycle, outcome FROM jobs WHERE id = $1", id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    (r.lifecycle, r.outcome)
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_run_streams_resolves_and_closes_its_job(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("100", "e2e");
    let opened = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap();
    assert_eq!(
        (opened.resume, opened.committed_through, opened.handle.epoch),
        (ResumeOutcome::New, 0, 1)
    );
    let events = suite_run(
        &mut a,
        SuiteIds::new(),
        &[("login", true), ("checkout", false)],
    );
    let applied = store.apply_batch(&opened.handle, &events).await.unwrap();
    assert_eq!(applied.through, a.seq);
    assert!(applied.rejected.is_empty(), "{:?}", applied.rejected);
    assert!(applied.change.is_some());
    assert_eq!(
        flows(&store, a.id).await,
        vec![
            (
                "login".into(),
                "passed".into(),
                "verified".into(),
                Some("junit_final".into())
            ),
            (
                "checkout".into(),
                "failed".into(),
                "verified".into(),
                Some("junit_final".into())
            ),
        ]
    );
    assert_eq!(
        job(&store, opened.handle.job_id).await,
        ("closed".into(), Some("failed".into()))
    );
    let journal: i64 = sqlx::query_scalar!(
        "SELECT count(*) AS \"n!\" FROM agent_events WHERE agent_run_id = $1",
        a.id
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(
        u64::try_from(journal).unwrap(),
        a.seq,
        "every event is in the journal"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn run_started_persists_the_evidence_description(pool: PgPool) {
    use fleet_protocol::event::{EvidenceSources, ReportReason, ReportSource};
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("102", "e2e");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let evidence = EvidenceSources {
        report: ReportSource {
            path: None,
            reason: ReportReason::UserFormat,
        },
        debug_output: Some("/run/artifacts".into()),
        maestro_log: true,
    };
    let started = a.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunStarted(RunStarted {
            argv: vec!["maestro".into(), "test".into()],
            agent_version: "0.0.0".into(),
            evidence: Some(evidence.clone()),
        }),
    );
    let applied = store.apply_batch(&h, &[started]).await.unwrap();
    assert!(applied.rejected.is_empty(), "{:?}", applied.rejected);
    let stored = sqlx::query_scalar!("SELECT evidence FROM agent_runs WHERE id = $1", a.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    let stored: EvidenceSources = serde_json::from_value(stored.unwrap()).unwrap();
    assert_eq!(stored, evidence);
    // An agent that sends none leaves NULL (unknown), not a made-up description.
    let mut b = Agent::new("103", "e2e");
    let hb = store
        .open_run(&p, &b.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let old = b.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunStarted(RunStarted {
            argv: vec!["maestro".into()],
            agent_version: "0.0.0".into(),
            evidence: None,
        }),
    );
    store.apply_batch(&hb, &[old]).await.unwrap();
    let none = sqlx::query_scalar!("SELECT evidence FROM agent_runs WHERE id = $1", b.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert!(none.is_none());
}

#[sqlx::test(migrations = "../../migrations")]
async fn duplicates_are_acked_without_effect_and_gaps_are_refused(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("101", "e2e");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let events = suite_run(&mut a, SuiteIds::new(), &[("login", true)]);
    let (head, tail) = events.split_at(4);
    store.apply_batch(&h, head).await.unwrap();
    // The whole stream again, reordered: the first four are duplicates, the rest apply in order.
    let mut again = events.clone();
    again.reverse();
    let applied = store.apply_batch(&h, &again).await.unwrap();
    assert_eq!((applied.through, applied.duplicates), (a.seq, 4));
    let applied = store.apply_batch(&h, &events).await.unwrap();
    assert_eq!(applied.duplicates, u32::try_from(events.len()).unwrap());
    assert!(applied.change.is_none(), "duplicates change nothing");
    // A batch that skips ahead.
    let late = a.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunFinalizing,
    );
    let skipped = a.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunFinalizing,
    );
    assert!(matches!(
        store.apply_batch(&h, std::slice::from_ref(&skipped)).await,
        Err(ApplyError::SeqGap { expected }) if expected == late.seq
    ));
    // A batch with a hole applies the contiguous prefix and asks for the rest.
    let hole = a.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunFinalizing,
    );
    let applied = store.apply_batch(&h, &[late.clone(), hole]).await.unwrap();
    assert_eq!(
        (applied.through, applied.expected),
        (late.seq, Some(skipped.seq))
    );
    let _ = tail;
}

/// A `journal.gap` as the agent sequences it: at `to`, the last seq it stands in for.
fn gap_event(seq: u64, from: u64, to: u64) -> EventEnvelope {
    EventEnvelope::new(
        seq,
        now(),
        Subject::default(),
        Provenance::new(SourceKind::AgentObserved),
        &EventBody::JournalGap(JournalGap {
            from_seq: from,
            to_seq: to,
            reason: "spool_full".into(),
        }),
    )
}

fn ev(a: &mut Agent) -> EventEnvelope {
    a.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunFinalizing,
    )
}

async fn gap_rows(store: &Store, run: Uuid) -> Vec<(i64, i64)> {
    sqlx::query_as(
        "SELECT from_seq, to_seq FROM ingest_gaps WHERE agent_run_id = $1 ORDER BY from_seq",
    )
    .bind(run)
    .fetch_all(store.pool())
    .await
    .unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_journal_gap_that_covers_a_hole_keeps_the_prefix_contiguous(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("110", "e2e");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let before: Vec<_> = (0..3).map(|_| ev(&mut a)).collect();
    // Events 4, 5 and 6 were dropped; the marker takes the last of those slots (6).
    a.seq = 5;
    let marker = gap_event(a.seq + 1, 4, 6);
    a.seq = 6;
    let after = ev(&mut a);
    assert_eq!(after.seq, 7);
    // One batch with the gap in the middle.
    let mut batch = before.clone();
    batch.extend([marker.clone(), after.clone()]);
    let applied = store.apply_batch(&h, &batch).await.unwrap();
    assert_eq!((applied.through, applied.expected), (7, None));
    assert_eq!(gap_rows(&store, a.id).await, [(4, 6)]);
    // The marker is a duplicate once committed.
    let again = store.apply_batch(&h, &[marker]).await.unwrap();
    assert_eq!((again.through, again.duplicates), (7, 1));
    // A gap in a batch of its own, then the event after it.
    a.seq = 10;
    let next = ev(&mut a);
    let applied = store
        .apply_batch(&h, &[gap_event(10, 8, 10)])
        .await
        .unwrap();
    assert_eq!(applied.through, 10);
    let applied = store.apply_batch(&h, &[next]).await.unwrap();
    assert_eq!(applied.through, 11);
    assert_eq!(gap_rows(&store, a.id).await, [(4, 6), (8, 10)]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_journal_gap_that_does_not_fit_its_hole_is_a_sequence_gap(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("111", "e2e");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let mut events: Vec<_> = (0..3)
        .map(|_| {
            a.event(
                Subject::default(),
                SourceKind::AgentObserved,
                &EventBody::RunFinalizing,
            )
        })
        .collect();
    let third = events.pop().unwrap();
    store.apply_batch(&h, &events).await.unwrap(); // committed through 2
    for bad in [
        gap_event(5, 4, 5), // starts after committed + 1: events 3 is missing
        gap_event(6, 3, 5), // sequenced at 6, not at its own to_seq
        gap_event(5, 6, 5), // backwards
    ] {
        let out = store.apply_batch(&h, &[bad]).await;
        assert!(
            matches!(out, Err(ApplyError::SeqGap { expected: 3 })),
            "{out:?}"
        );
    }
    assert_eq!(gap_rows(&store, a.id).await.len(), 0);
    // A batch where the bad gap follows good events applies the prefix and asks for the rest.
    let applied = store
        .apply_batch(&h, &[third, gap_event(9, 7, 9)])
        .await
        .unwrap();
    assert_eq!((applied.through, applied.expected), (3, Some(4)));
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_newer_connection_fences_the_old_one(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("102", "e2e");
    let old = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let events = suite_run(&mut a, SuiteIds::new(), &[("login", true)]);
    store.apply_batch(&old, &events[..3]).await.unwrap();
    let new = store
        .open_run(&p, &a.hello(Some(4)), Transport::WebSocket)
        .await
        .unwrap();
    assert_eq!(
        (new.resume, new.committed_through, new.handle.epoch),
        (ResumeOutcome::Resumed, 3, 2)
    );
    assert!(matches!(
        store.apply_batch(&old, &events[3..]).await,
        Err(ApplyError::Superseded)
    ));
    let applied = store.apply_batch(&new.handle, &events[3..]).await.unwrap();
    assert_eq!(applied.through, a.seq);
    // A detach from the fenced connection leaves the live one alone.
    store.detach(&old, "closed").await.unwrap();
    let state = sqlx::query_scalar!("SELECT state FROM agent_runs WHERE id = $1", a.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(state, "ended");
}

#[sqlx::test(migrations = "../../migrations")]
async fn resume_rules(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("103", "e2e");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let events = suite_run(&mut a, SuiteIds::new(), &[("login", true)]);
    store.apply_batch(&h, &events[..2]).await.unwrap();

    // The agent lost its spool: it holds only seq 6 onwards. 3..5 become an explicit gap.
    let opened = store
        .open_run(&p, &a.hello(Some(6)), Transport::WebSocket)
        .await
        .unwrap();
    assert_eq!(opened.committed_through, 5);
    let gap = sqlx::query!(
        "SELECT from_seq, to_seq, reason FROM ingest_gaps WHERE agent_run_id = $1",
        a.id
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(
        (gap.from_seq, gap.to_seq, gap.reason.as_str()),
        (3, 5, "agent_discarded")
    );

    // An agent that reuses a run ID with an older sequence is refused for good.
    let mut stale = a.hello(None);
    stale.resume.next_seq = 2;
    assert!(matches!(
        store.open_run(&p, &stale, Transport::WebSocket).await,
        Err(OpenError::Refused {
            code: ErrorCode::SeqRegression,
            fatal: true,
            ..
        })
    ));

    // A run unknown to the server (say, a restored backup) starts at the agent's oldest event.
    let mut b = Agent::new("103", "other");
    b.seq = 9;
    let opened = store
        .open_run(&p, &b.hello(Some(4)), Transport::WebSocket)
        .await
        .unwrap();
    assert_eq!(
        (opened.resume, opened.committed_through),
        (ResumeOutcome::New, 3)
    );

    // Another project's credential can't touch the run.
    let other = store.upsert_project("other", "Other").await.unwrap();
    let foreign = Publisher {
        project_id: other,
        ..p.clone()
    };
    assert!(matches!(
        store
            .open_run(&foreign, &a.hello(None), Transport::WebSocket)
            .await,
        Err(OpenError::Refused {
            code: ErrorCode::Unauthorized,
            ..
        })
    ));
}

#[sqlx::test(migrations = "../../migrations")]
async fn inferred_sources_can_never_claim_passed(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("104", "e2e");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let ids = SuiteIds::new();
    let mut events = suite_run(&mut a, ids, &[])[..3].to_vec();
    a.seq = 3;
    let flow = Uuid::now_v7();
    events.push(a.event(
        ids.flow(flow),
        SourceKind::DebugLog,
        &EventBody::FlowStarted(FlowStarted {
            flow: FlowRef {
                name: "login".into(),
                path: None,
            },
            shard_number: 1,
            ordinal: 0,
        }),
    ));
    for src in [
        SourceKind::ProcessExit,
        SourceKind::AgentObserved,
        SourceKind::Declared,
        SourceKind::ServerInferred,
    ] {
        events.push(a.event(
            ids.flow(flow),
            src,
            &EventBody::FlowEnded(FlowEnded {
                outcome: FlowOutcome::Passed,
                duration_ms: None,
                failure: None,
                flow: None,
            }),
        ));
    }
    let applied = store.apply_batch(&h, &events).await.unwrap();
    assert_eq!(applied.through, a.seq, "rejections move the cursor on");
    assert_eq!(applied.rejected.len(), 4, "{:?}", applied.rejected);
    assert_eq!(
        flows(&store, a.id).await,
        vec![("login".into(), "running".into(), "provisional".into(), None)]
    );
    // The database refuses it too (the second line of defence).
    let direct = sqlx::query!(
        "INSERT INTO flow_claims (flow_execution_id, source, origin, outcome, claim_order)
         VALUES ($1, 'server_inferred', 'test', 'passed', 1)",
        flow
    )
    .execute(store.pool())
    .await;
    assert!(direct.is_err());
    let direct = sqlx::query!(
        "UPDATE flow_executions SET state = 'passed', outcome_source = 'process_exit' WHERE id = $1",
        flow
    )
    .execute(store.pool())
    .await;
    assert!(direct.is_err());
}

#[sqlx::test(migrations = "../../migrations")]
async fn foreign_entities_unknown_types_and_events_after_the_end(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("105", "a");
    let ha = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let ids = SuiteIds::new();
    let events = suite_run(&mut a, ids, &[("login", true)]);
    store.apply_batch(&ha, &events).await.unwrap();
    let theirs = sqlx::query_scalar!(
        "SELECT id FROM flow_executions WHERE agent_run_id = $1",
        a.id
    )
    .fetch_one(store.pool())
    .await
    .unwrap();

    // Run B reuses run A's suite and flow IDs: refused, and A's rows are untouched.
    let mut b = Agent::new("105", "b");
    let hb = store
        .open_run(&p, &b.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let mut batch = vec![
        b.event(
            ids.suite(),
            SourceKind::Declared,
            &EventBody::SuiteStarted(SuiteStarted {
                shard_mode: ShardMode::None,
                shards: 1,
            }),
        ),
        b.event(
            ids.flow(theirs),
            SourceKind::JunitFinal,
            &EventBody::FlowEnded(FlowEnded {
                outcome: FlowOutcome::Failed,
                duration_ms: None,
                failure: None,
                flow: None,
            }),
        ),
    ];
    let mut unknown = b.event(
        Subject::default(),
        SourceKind::LiveConsole,
        &EventBody::RunFinalizing,
    );
    unknown.kind = "future.thing".into();
    batch.push(unknown);
    batch.push(b.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::JournalGap(JournalGap {
            from_seq: 1,
            to_seq: 2,
            reason: "spool_full".into(),
        }),
    ));
    let applied = store.apply_batch(&hb, &batch).await.unwrap();
    let codes: Vec<&str> = applied
        .rejected
        .iter()
        .map(|r| r.message.split(':').next().unwrap())
        .collect();
    assert_eq!(codes, vec!["foreign_entity", "foreign_entity"]);
    assert_eq!(
        flows(&store, a.id).await[0].1,
        "passed",
        "A's flow is untouched"
    );
    let statuses: Vec<String> = sqlx::query_scalar!(
        "SELECT apply_status FROM agent_events WHERE agent_run_id = $1 ORDER BY seq",
        b.id
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(statuses, ["rejected", "rejected", "unsupported", "applied"]);
    let gaps: i64 = sqlx::query_scalar!(
        "SELECT count(*) AS \"n!\" FROM ingest_gaps WHERE agent_run_id = $1",
        b.id
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(gaps, 1, "journal.gap is recorded (I5)");

    // After run.ended, later sequence numbers are refused.
    let ha = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let after = a.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunFinalizing,
    );
    let applied = store.apply_batch(&ha, &[after]).await.unwrap();
    assert!(applied.rejected[0].message.starts_with("after_end"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn concurrent_runs_in_one_job_and_attempt_stay_consistent(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..8 {
        let (store, p) = (store.clone(), p.clone());
        tasks.spawn(async move {
            // Two runs per job key: overlapping job names in one attempt.
            let mut a = Agent::new("106", &format!("job-{}", i % 4));
            let h = store
                .open_run(&p, &a.hello(None), Transport::WebSocket)
                .await
                .unwrap()
                .handle;
            let events = suite_run(&mut a, SuiteIds::new(), &[("a", true), ("b", i % 3 != 0)]);
            for chunk in events.chunks(3) {
                store.apply_batch(&h, chunk).await.unwrap();
            }
            (a.id, a.seq)
        });
    }
    let mut runs = Vec::new();
    while let Some(r) = tasks.join_next().await {
        runs.push(r.unwrap());
    }
    for (run, seq) in &runs {
        let committed =
            sqlx::query_scalar!("SELECT committed_seq FROM agent_runs WHERE id = $1", run)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(u64::try_from(committed).unwrap(), *seq);
        assert_eq!(flows(&store, *run).await.len(), 2);
    }
    let jobs = sqlx::query!("SELECT job_key, lifecycle, outcome FROM jobs ORDER BY job_key")
        .fetch_all(store.pool())
        .await
        .unwrap();
    assert_eq!(jobs.len(), 4, "one job per key, shared by its two runs");
    for j in jobs {
        // Runs 0, 3 and 6 fail a flow: jobs 0, 3 and 2.
        let expected = if j.job_key == "job-1" {
            "completed"
        } else {
            "failed"
        };
        assert_eq!(
            (j.lifecycle.as_str(), j.outcome.as_deref()),
            ("closed", Some(expected)),
            "{}",
            j.job_key
        );
    }
    let attempts: i64 = sqlx::query_scalar!("SELECT count(*) AS \"n!\" FROM pipeline_attempts")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(attempts, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_new_run_never_deadlocks_with_a_batch_in_its_job(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let first = Agent::new("107", "job");
    let h = store
        .open_run(&p, &first.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    // A batch mid-transaction, as apply_sorted holds it: derive has the job row and
    // bump_version is still to come.
    let mut batch = store.pool().begin().await.unwrap();
    sqlx::query!("UPDATE jobs SET name = name WHERE id = $1", h.job_id)
        .execute(&mut *batch)
        .await
        .unwrap();
    // A second run in the same job opens meanwhile and waits for the job row.
    let second = {
        let (store, p) = (store.clone(), p.clone());
        tokio::spawn(async move {
            store
                .open_run(
                    &p,
                    &Agent::new("107", "job").hello(None),
                    Transport::WebSocket,
                )
                .await
                .map(|_| ())
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // The batch then bumps the attempt: open_run must not be holding it.
    store
        .bump_version(
            &mut batch,
            h.project_id,
            h.pipeline_attempt_id,
            BTreeSet::from([EntityRef::Job(h.job_id)]),
        )
        .await
        .unwrap();
    batch.commit().await.unwrap();
    assert!(second.await.unwrap().is_ok());
}

#[sqlx::test(migrations = "../../migrations")]
async fn foreign_device_on_flow_is_rejected_without_changing_its_owner(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("foreign-device-a", "a");
    let ha = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let a_ids = SuiteIds::new();
    let events = suite_run(&mut a, a_ids, &[]);
    store.apply_batch(&ha, &events[..3]).await.unwrap();
    let before = store
        .snapshot(p.project_id, ha.pipeline_attempt_id)
        .await
        .unwrap()
        .unwrap()
        .devices;

    let mut b = Agent::new("foreign-device-b", "b");
    let hb = store
        .open_run(&p, &b.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let b_ids = SuiteIds::new();
    let events = suite_run(&mut b, b_ids, &[]);
    store.apply_batch(&hb, &events[..3]).await.unwrap();
    b.seq = 3;
    let subject = Subject {
        device_session_id: Some(a_ids.device),
        ..b_ids.flow(Uuid::now_v7())
    };
    let event = b.event(
        subject,
        SourceKind::DebugLog,
        &EventBody::FlowStarted(FlowStarted {
            flow: FlowRef {
                name: "foreign".into(),
                path: None,
            },
            shard_number: 1,
            ordinal: 0,
        }),
    );
    let result = store.apply_batch(&hb, &[event]).await.unwrap();
    assert!(result.rejected[0].message.starts_with("foreign_entity"));
    assert_eq!(
        store
            .snapshot(p.project_id, ha.pipeline_attempt_id)
            .await
            .unwrap()
            .unwrap()
            .devices,
        before
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn foreign_shard_on_flow_is_rejected_without_changing_its_owner(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("foreign-device-a", "a");
    let ha = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let a_ids = SuiteIds::new();
    let events = suite_run(&mut a, a_ids, &[]);
    store.apply_batch(&ha, &events[..3]).await.unwrap();
    let before = store
        .snapshot(p.project_id, ha.pipeline_attempt_id)
        .await
        .unwrap()
        .unwrap()
        .devices;

    let mut b = Agent::new("foreign-device-b", "b");
    let hb = store
        .open_run(&p, &b.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let b_ids = SuiteIds::new();
    let events = suite_run(&mut b, b_ids, &[]);
    store.apply_batch(&hb, &events[..3]).await.unwrap();
    b.seq = 3;
    let subject = Subject {
        shard_id: Some(a_ids.shard),
        ..b_ids.flow(Uuid::now_v7())
    };
    let event = b.event(
        subject,
        SourceKind::DebugLog,
        &EventBody::FlowStarted(FlowStarted {
            flow: FlowRef {
                name: "foreign".into(),
                path: None,
            },
            shard_number: 1,
            ordinal: 0,
        }),
    );
    let result = store.apply_batch(&hb, &[event]).await.unwrap();
    assert!(result.rejected[0].message.starts_with("foreign_entity"));
    assert_eq!(
        store
            .snapshot(p.project_id, ha.pipeline_attempt_id)
            .await
            .unwrap()
            .unwrap()
            .devices,
        before
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn live_entities_are_scoped_to_the_requested_project(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let other = store.upsert_project("other", "Other").await.unwrap();
    let mut a = Agent::new("live-scope", "a");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let ids = SuiteIds::new();
    let events = suite_run(&mut a, ids, &[]);
    store.apply_batch(&h, &events[..3]).await.unwrap();
    assert_eq!(
        store
            .entities(other, &[EntityRef::Device(ids.device)])
            .await
            .unwrap(),
        []
    );
    assert_eq!(
        store
            .entities(p.project_id, &[EntityRef::Device(ids.device)])
            .await
            .unwrap()
            .len(),
        1
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn nul_in_event_data_is_rejected_and_later_sequences_commit(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("nul-data", "a");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let mut bad = a.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunFinalizing,
    );
    bad.data = serde_json::json!({"message": "bad\u{0}text"});
    let good = a.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunFinalizing,
    );
    let applied = store.apply_batch(&h, &[bad, good]).await.unwrap();
    assert_eq!(applied.through, 2);
    assert_eq!(applied.rejected.len(), 1);
    assert_eq!(applied.rejected[0].seq, 1);
}

#[sqlx::test(migrations = "../../migrations")]
async fn nul_in_flow_name_and_source_is_rejected_without_wedging_the_run(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("nul-flow", "a");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let ids = SuiteIds::new();
    let initial = suite_run(&mut a, ids, &[]);
    store.apply_batch(&h, &initial[..3]).await.unwrap();
    a.seq = 3;
    let bad_flow = a.event(
        ids.flow(Uuid::now_v7()),
        SourceKind::DebugLog,
        &EventBody::FlowStarted(FlowStarted {
            flow: FlowRef {
                name: "bad\u{0}name".into(),
                path: None,
            },
            shard_number: 1,
            ordinal: 0,
        }),
    );
    let mut bad_source = a.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunFinalizing,
    );
    bad_source.src.parser = Some("bad\u{0}parser".into());
    let good = a.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunFinalizing,
    );
    let result = store
        .apply_batch(&h, &[bad_flow, bad_source, good])
        .await
        .unwrap();
    assert_eq!(result.through, 6);
    assert_eq!(
        result.rejected.iter().map(|r| r.seq).collect::<Vec<_>>(),
        [4, 5]
    );
    assert_eq!(
        store
            .snapshot(p.project_id, h.pipeline_attempt_id)
            .await
            .unwrap()
            .unwrap()
            .flows,
        []
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn rejected_process_exit_does_not_change_the_run(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("bad-exit", "a");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let event = a.event(
        Subject::default(),
        SourceKind::ProcessExit,
        &EventBody::ProcessExited(ProcessExited {
            code: Some(42),
            signal: None,
            class: ExitClass::TestFailures,
        }),
    );
    let result = store.apply_batch(&h, &[event]).await.unwrap();
    assert_eq!(result.rejected.len(), 1);
    let snapshot = store
        .snapshot(p.project_id, h.pipeline_attempt_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.agent_runs[0].maestro_exit_code, None);
}

#[sqlx::test(migrations = "../../migrations")]
async fn events_after_run_end_in_same_batch_are_rejected(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("same-batch-end", "a");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let end = a.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunEnded(RunEnded {
            interrupted: false,
            output_bytes_dropped: 0,
        }),
    );
    let after = a.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunFinalizing,
    );
    let result = store.apply_batch(&h, &[end, after]).await.unwrap();
    assert_eq!(result.rejected.len(), 1);
    assert!(result.rejected[0].message.starts_with("after_end"));
    assert_eq!(result.rejected[0].seq, 2);
}

#[sqlx::test(migrations = "../../migrations")]
async fn rejected_flow_claims_and_shard_leave_no_entities(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("rejected-entities", "a");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let ids = SuiteIds::new();
    let initial = suite_run(&mut a, ids, &[]);
    store.apply_batch(&h, &initial[..3]).await.unwrap();
    a.seq = 3;
    let ended = a.event(
        ids.flow(Uuid::now_v7()),
        SourceKind::Declared,
        &EventBody::FlowEnded(FlowEnded {
            outcome: FlowOutcome::Passed,
            duration_ms: None,
            failure: None,
            flow: Some(FlowRef {
                name: "ended".into(),
                path: None,
            }),
        }),
    );
    let reported = a.event(
        ids.flow(Uuid::now_v7()),
        SourceKind::Declared,
        &EventBody::FlowReported(FlowReported {
            repo_path: None,
            testsuite_index: 0,
            testcase_index: 0,
            matched: MatchMethod::Positional,
            junit: JunitTestcase {
                id: None,
                name: "reported".into(),
                file: Some("reported.yaml".into()),
                time_ms: None,
                timestamp_local: None,
                status: None,
                failure: None,
            },
            outcome: FlowOutcome::Passed,
        }),
    );
    let foreign = SuiteIds::new();
    let shard = a.event(
        foreign.shard(),
        SourceKind::DebugLog,
        &EventBody::ShardAssigned(ShardAssigned {
            shard_number: 1,
            device_serial: "new-device".into(),
            planned: Vec::new(),
        }),
    );
    let result = store
        .apply_batch(&h, &[ended, reported, shard])
        .await
        .unwrap();
    assert_eq!(result.rejected.len(), 3);
    let snapshot = store
        .snapshot(p.project_id, h.pipeline_attempt_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.flows, []);
    assert_eq!(snapshot.devices.len(), 1);
    assert_eq!(snapshot.devices[0].id, ids.device);
}

#[sqlx::test(migrations = "../../migrations")]
async fn absolute_and_relative_report_paths_share_history_identity(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("identity", "a");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let ids = SuiteIds::new();
    let initial = suite_run(&mut a, ids, &[]);
    store.apply_batch(&h, &initial[..3]).await.unwrap();
    a.seq = 3;
    let make = |file: &str, repo_path: Option<&str>| {
        EventBody::FlowReported(FlowReported {
            testsuite_index: 0,
            testcase_index: 0,
            matched: MatchMethod::Positional,
            junit: JunitTestcase {
                id: None,
                name: "pass".into(),
                file: Some(file.into()),
                time_ms: None,
                timestamp_local: None,
                status: Some("SUCCESS".into()),
                failure: None,
            },
            repo_path: repo_path.map(str::to_owned),
            outcome: FlowOutcome::Passed,
        })
    };
    let absolute = a.event(
        ids.flow(Uuid::now_v7()),
        SourceKind::JunitFinal,
        &make(
            "/fixture/checkout/examples/pass.yaml",
            Some("examples/pass.yaml"),
        ),
    );
    let relative = a.event(
        ids.flow(Uuid::now_v7()),
        SourceKind::JunitFinal,
        &make("examples/pass.yaml", None),
    );
    store.apply_batch(&h, &[absolute, relative]).await.unwrap();
    let history = store
        .history(
            p.project_id,
            &crate::screens::HistoryQuery {
                flow: None,
                branch: None,
                outcome: None,
                from: OffsetDateTime::UNIX_EPOCH,
                to: OffsetDateTime::now_utc() + time::Duration::days(1),
                after: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        history
            .items
            .iter()
            .map(|item| item.flow.as_str())
            .collect::<Vec<_>>(),
        ["examples/pass.yaml", "examples/pass.yaml"]
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_app_crash_is_shown_on_its_flow_and_changes_no_verdict(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("crash", "a");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let ids = SuiteIds::new();
    let mut events = suite_run(&mut a, ids, &[("crash", false)]);
    let flow = events
        .iter()
        .find_map(|e| e.subject.flow_execution_id)
        .unwrap();
    // The crash arrives before run.ended, so it goes just ahead of it in the same sequence.
    events.pop();
    a.seq -= 1;
    events.push(
        a.event(
            ids.flow(flow),
            SourceKind::ArtifactFile,
            &EventBody::AppCrashed(AppCrashed {
                package: "com.android.settings".into(),
                exception:
                    "android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash"
                        .into(),
            }),
        ),
    );
    events.push(a.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunEnded(RunEnded {
            interrupted: false,
            output_bytes_dropped: 0,
        }),
    ));
    let applied = store.apply_batch(&h, &events).await.unwrap();
    assert!(applied.rejected.is_empty(), "the crash is accepted");

    let detail = store
        .flow_detail(p.project_id, flow)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        detail.crash,
        Some(CrashView {
            package: "com.android.settings".into(),
            exception:
                "android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash"
                    .into(),
        })
    );
    // Evidence only (I3): the flow is still Maestro's failure, with Maestro's message.
    assert_eq!(detail.flow.state, "failed");
    assert_eq!(
        detail.failure.and_then(|f| f.message).as_deref(),
        Some("Assertion is false")
    );
    assert_eq!(flows(&store, a.id).await[0].1, "failed");
}

/// The step rows of a flow, by index: `(index, status, source, started, ended)`.
async fn step_rows(
    store: &Store,
    flow: Uuid,
) -> Vec<(
    i32,
    String,
    String,
    Option<OffsetDateTime>,
    Option<OffsetDateTime>,
)> {
    sqlx::query_as(
        "SELECT step_index, status, source::text, started_at, ended_at
           FROM step_executions WHERE flow_execution_id = $1 ORDER BY step_index",
    )
    .bind(flow)
    .fetch_all(store.pool())
    .await
    .unwrap()
}

/// The log's three steps of one flow (issue #148): each started and ended, all completed.
fn logged_steps(a: &mut Agent, ids: &SuiteIds, flow: Uuid) -> Vec<EventEnvelope> {
    let mut out = Vec::new();
    for (index, description) in ["Launch app", "Tap on \"Apps\"", "Assert visible"]
        .into_iter()
        .enumerate()
    {
        let step = Subject {
            step: Some(StepRef {
                index: u32::try_from(index).unwrap(),
            }),
            ..ids.flow(flow)
        };
        out.push(a.event(
            step.clone(),
            SourceKind::DebugLog,
            &EventBody::StepStarted(StepStarted {
                description: description.into(),
                depth: None,
                parent: None,
                attempt: None,
            }),
        ));
        out.push(a.event(
            step,
            SourceKind::DebugLog,
            &EventBody::StepEnded(StepEnded {
                status: StepStatus::Completed,
                timing: None,
            }),
        ));
    }
    out
}

/// A final `commands.json` settles the steps it covers and no others (issue #148): a covered row
/// takes the file's status and timing, an uncovered one keeps the log's, and the file never creates
/// a row. A log event that arrives after the settle does not undo it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_final_file_settles_the_rows_it_covers_and_creates_none(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("settle", "a");
    let h = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap()
        .handle;
    let ids = SuiteIds::new();
    let flow = Uuid::now_v7();
    let flow_ref = FlowRef {
        name: "settle".into(),
        path: Some("flows/settle.yaml".into()),
    };
    let mut events = vec![
        a.event(
            Subject::default(),
            SourceKind::AgentObserved,
            &EventBody::RunStarted(RunStarted {
                argv: vec!["maestro".into(), "test".into()],
                agent_version: "0.0.0".into(),
                evidence: None,
            }),
        ),
        a.event(
            ids.suite(),
            SourceKind::Declared,
            &EventBody::SuiteStarted(SuiteStarted {
                shard_mode: ShardMode::None,
                shards: 1,
            }),
        ),
        a.event(
            ids.shard(),
            SourceKind::DebugLog,
            &EventBody::ShardAssigned(ShardAssigned {
                shard_number: 1,
                device_serial: "emulator-5554".into(),
                planned: vec![flow_ref.clone()],
            }),
        ),
        a.event(
            ids.flow(flow),
            SourceKind::DebugLog,
            &EventBody::FlowStarted(FlowStarted {
                flow: flow_ref,
                shard_number: 1,
                ordinal: 0,
            }),
        ),
    ];
    events.extend(logged_steps(&mut a, &ids, flow));
    let step = |index: u32| Subject {
        step: Some(StepRef { index }),
        ..ids.flow(flow)
    };
    // Step 0 is covered with its timing; step 1 without; step 2 is not covered; step 9 was never
    // reported, so it must not appear.
    let started_ms: i64 = 1_793_751_910_644;
    events.push(a.event(
        step(0),
        SourceKind::ArtifactFile,
        &EventBody::StepEnded(StepEnded {
            status: StepStatus::Failed,
            timing: Some(StepTiming {
                started_ms,
                duration_ms: 4_250,
            }),
        }),
    ));
    events.push(a.event(
        step(1),
        SourceKind::ArtifactFile,
        &EventBody::StepEnded(StepEnded {
            status: StepStatus::Completed,
            timing: None,
        }),
    ));
    events.push(a.event(
        step(9),
        SourceKind::ArtifactFile,
        &EventBody::StepEnded(StepEnded {
            status: StepStatus::Completed,
            timing: None,
        }),
    ));
    // A late log end for the settled step must not undo the file's status.
    events.push(a.event(
        step(0),
        SourceKind::DebugLog,
        &EventBody::StepEnded(StepEnded {
            status: StepStatus::Completed,
            timing: None,
        }),
    ));
    let applied = store.apply_batch(&h, &events).await.unwrap();
    assert!(
        applied.rejected.is_empty(),
        "every event is accepted: {:?}",
        applied.rejected
    );

    let rows = step_rows(&store, flow).await;
    let start =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(started_ms) * 1_000_000).unwrap();
    assert_eq!(rows.len(), 3, "no row is created for step 9: {rows:?}");
    let (index, status, source, from, to) = &rows[0];
    assert_eq!(
        (*index, status.as_str(), source.as_str()),
        (0, "failed", "artifact_file"),
        "covered, and the file's status wins over the late log end"
    );
    assert_eq!(*from, Some(start), "the file's start");
    assert_eq!(
        *to,
        Some(start + time::Duration::milliseconds(4_250)),
        "the file's length"
    );
    assert_eq!(rows[1].1, "completed");
    assert_eq!(rows[1].2, "artifact_file");
    assert_eq!(
        rows[2].2, "debug_log",
        "an uncovered step stays provisional"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_token_in_log_text_is_stored_redacted_though_the_agent_sent_it_raw(pool: PgPool) {
    let store = Store {
        pool,
        origin: uuid::Uuid::now_v7(),
    };
    let p = publisher(&store).await;
    let mut a = Agent::new("110", "e2e");
    let opened = store
        .open_run(&p, &a.hello(None), Transport::WebSocket)
        .await
        .unwrap();
    // A GitHub token shape in the failure message and the step text, sent unredacted.
    let token = "ghp_0123456789abcdefghijABCDEFGHIJ012345";
    let line = format!("tap {token} failed");
    let scripted = [Script {
        name: "checkout",
        failure: Some(&line),
        failed_step: Some(&line),
    }];
    let events = suite_run_scripted(&mut a, SuiteIds::new(), &scripted);
    let applied = store.apply_batch(&opened.handle, &events).await.unwrap();
    assert!(applied.rejected.is_empty(), "{:?}", applied.rejected);
    let message: Option<String> =
        sqlx::query_scalar("SELECT failure_message FROM flow_executions WHERE agent_run_id = $1")
            .bind(opened.handle.agent_run_id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(message.as_deref(), Some("tap [REDACTED] failed"));
    let labels: Vec<String> = sqlx::query_scalar(
        "SELECT st.label FROM step_executions st
           JOIN flow_executions f ON f.id = st.flow_execution_id
          WHERE f.agent_run_id = $1",
    )
    .bind(opened.handle.agent_run_id)
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert!(
        !labels.is_empty() && labels.iter().all(|l| !l.contains(token)),
        "{labels:?}"
    );
    let journal: Vec<String> =
        sqlx::query_scalar("SELECT data::text FROM agent_events WHERE agent_run_id = $1")
            .bind(opened.handle.agent_run_id)
            .fetch_all(store.pool())
            .await
            .unwrap();
    assert!(
        journal.iter().all(|d| !d.contains(token)),
        "the journal kept the token"
    );
}
