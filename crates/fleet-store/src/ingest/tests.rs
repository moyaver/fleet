// SPDX-License-Identifier: AGPL-3.0-only
//! Ingest transaction tests against Postgres 18 (`#[sqlx::test]`: a fresh database each).
//!
//! Events here are protocol events built by hand, not Maestro output: the fixture-driven tests
//! (real agent, real fixtures) live in fleet-server and fleet-e2e.

use fleet_protocol::{
    PROTOCOL_VERSION,
    event::{
        EventBody, ExitClass, FailureInfo, FlowEnded, FlowOutcome, FlowRef, FlowReported,
        FlowStarted, JournalGap, JunitTestcase, MatchMethod, ProcessExited, ReportParsed,
        ReportStatus, RunEnded, RunStarted, ShardAssigned, ShardMode, SuiteStarted,
    },
    frame::{AgentInfo, CiContext, ProtocolRange, ResumeState},
    journal::{ClockSample, Provenance, SourceKind, Subject},
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
#[expect(clippy::too_many_lines, reason = "one straight-line event script")]
pub(crate) fn suite_run(
    a: &mut Agent,
    ids: SuiteIds,
    flows: &[(&str, bool)],
) -> Vec<EventEnvelope> {
    let mut out = vec![
        a.event(
            Subject::default(),
            SourceKind::AgentObserved,
            &EventBody::RunStarted(RunStarted {
                argv: vec!["maestro".into(), "test".into()],
                agent_version: "0.0.0".into(),
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
                    .map(|(n, _)| FlowRef {
                        name: (*n).into(),
                        path: Some(format!("flows/{n}.yaml")),
                    })
                    .collect(),
            }),
        ),
    ];
    let mut flow_ids = Vec::new();
    for (i, (name, passed)) in flows.iter().enumerate() {
        let f = Uuid::now_v7();
        flow_ids.push(f);
        out.push(a.event(
            ids.flow(f),
            SourceKind::DebugLog,
            &EventBody::FlowStarted(FlowStarted {
                flow: FlowRef {
                    name: (*name).into(),
                    path: Some(format!("flows/{name}.yaml")),
                },
                shard_number: 1,
                ordinal: u32::try_from(i).unwrap(),
            }),
        ));
        out.push(a.event(
            ids.flow(f),
            SourceKind::LiveConsole,
            &EventBody::FlowEnded(FlowEnded {
                outcome: if *passed {
                    FlowOutcome::Passed
                } else {
                    FlowOutcome::Failed
                },
                duration_ms: Some(1000),
                failure: (!passed).then(|| FailureInfo {
                    message: "Assertion is false".into(),
                }),
                flow: None,
            }),
        ));
    }
    let all_passed = flows.iter().all(|(_, p)| *p);
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
            failures: u32::try_from(flows.iter().filter(|(_, p)| !p).count()).unwrap(),
        }),
    ));
    for (i, ((name, passed), f)) in flows.iter().zip(&flow_ids).enumerate() {
        out.push(a.event(
            ids.flow(*f),
            SourceKind::JunitFinal,
            &EventBody::FlowReported(FlowReported {
                testsuite_index: 0,
                testcase_index: u32::try_from(i).unwrap(),
                matched: MatchMethod::Positional,
                junit: JunitTestcase {
                    id: Some((*name).into()),
                    name: (*name).into(),
                    file: Some(format!("flows/{name}.yaml")),
                    time_ms: Some(1000),
                    timestamp_local: None,
                    status: Some(if *passed { "SUCCESS" } else { "ERROR" }.into()),
                    failure: (!passed).then(|| "Assertion is false".into()),
                },
                outcome: if *passed {
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
        .create_api_key(project, "ci", TrustLevel::Trusted)
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
    let store = Store { pool };
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
async fn duplicates_are_acked_without_effect_and_gaps_are_refused(pool: PgPool) {
    let store = Store { pool };
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

#[sqlx::test(migrations = "../../migrations")]
async fn a_newer_connection_fences_the_old_one(pool: PgPool) {
    let store = Store { pool };
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
    let store = Store { pool };
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
    let store = Store { pool };
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
    let store = Store { pool };
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
    let store = Store { pool };
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
    let store = Store { pool };
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
    bump_version(
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
