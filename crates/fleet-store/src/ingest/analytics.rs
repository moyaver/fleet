// SPDX-License-Identifier: AGPL-3.0-only
//! Fingerprints at flow finalization, inside the ingest transaction (ADR-0009; PLAN §9).
//!
//! When a flow's outcome becomes a final pass or failure, it gets a condition fingerprint, and a
//! failure also a failure fingerprint. Both are computed by `fleet_core::fingerprint` from facts
//! the database already holds; a fact it doesn't hold reads `unknown`, never a guess.

use fleet_core::fingerprint::{ConditionFacts, FailureFacts};
use sqlx::PgConnection;
use uuid::Uuid;

/// What the store holds about one flow execution, for its fingerprints.
struct Flow {
    id: Uuid,
    project_id: Uuid,
    /// Settled as a pass or a failure (not provisional, not inferred).
    final_outcome: bool,
    failed: bool,
    failure_message: Option<String>,
    /// The repository path when the report linked one, else the name.
    identity: String,
    failed_step: Option<String>,
    conditions: ConditionFacts,
}

/// Computes and stores the fingerprints of `flows` that are final; drops a failure fingerprint
/// from a flow that is no longer a final failure. Writes nothing when nothing changed.
///
/// The condition facts come from what the store holds today: the worker's OS and architecture,
/// the CI provider, the Maestro version the run reported at start, the device profile (still
/// empty until adb properties are reported) and the host's memory from its latest telemetry
/// sample. The host core count is not reported yet, so it reads `unknown`.
pub(crate) async fn finalize_flows(
    tx: &mut PgConnection,
    flows: &[Uuid],
) -> Result<(), sqlx::Error> {
    if flows.is_empty() {
        return Ok(());
    }
    let mut not_failed = Vec::new();
    for flow in load(tx, flows).await? {
        if flow.final_outcome {
            store_condition(tx, &flow).await?;
        }
        if flow.final_outcome && flow.failed {
            store_failure(tx, &flow).await?;
        } else {
            not_failed.push(flow.id);
        }
    }
    sqlx::query!(
        "DELETE FROM failure_fingerprints WHERE flow_execution_id = ANY($1)",
        &not_failed
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}

async fn load(tx: &mut PgConnection, flows: &[Uuid]) -> Result<Vec<Flow>, sqlx::Error> {
    let rows = sqlx::query!(
        "SELECT f.id, f.project_id, f.state, f.finality::text AS \"finality!\", f.failure_message,
                coalesce(d.repo_path, f.flow_name) AS \"flow!\",
                (SELECT s.label FROM step_executions s
                  WHERE s.flow_execution_id = f.id AND s.run_day = r.run_day
                    AND s.status = 'failed'
                  ORDER BY s.step_index LIMIT 1) AS failed_step,
                w.os AS \"os?\", w.arch AS \"arch?\", pr.provider, r.maestro_version,
                coalesce(dp.model, dp.avd_or_runtime) AS device_model, dp.api_level AS \"api_level?\",
                (SELECT h.host_mem_total_bytes FROM host_samples h
                  WHERE h.agent_run_id = f.agent_run_id AND h.host_mem_total_bytes IS NOT NULL
                  ORDER BY h.ts DESC LIMIT 1) AS host_memory
           FROM flow_executions f
           JOIN agent_runs r ON r.id = f.agent_run_id
           LEFT JOIN workers w ON w.id = r.worker_id
           JOIN pipeline_attempts pa ON pa.id = f.pipeline_attempt_id
           JOIN pipeline_runs pr ON pr.id = pa.pipeline_run_id
           LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
           LEFT JOIN device_sessions ds ON ds.id = f.device_session_id
           LEFT JOIN device_profiles dp ON dp.id = ds.profile_id
          WHERE f.id = ANY($1)",
        flows
    )
    .fetch_all(&mut *tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Flow {
            id: row.id,
            project_id: row.project_id,
            final_outcome: matches!(row.finality.as_str(), "verified" | "unverified_final")
                && matches!(row.state.as_str(), "passed" | "failed"),
            failed: row.state == "failed",
            failure_message: row.failure_message,
            identity: row.flow,
            failed_step: row.failed_step,
            conditions: ConditionFacts {
                device_model: row.device_model,
                device_api_level: row.api_level.and_then(|n| u32::try_from(n).ok()),
                maestro_version: row.maestro_version,
                os: row.os,
                arch: row.arch,
                host_cores: None,
                host_memory_bytes: row.host_memory.and_then(|n| u64::try_from(n).ok()),
                ci_provider: Some(row.provider),
            },
        })
        .collect())
}

async fn store_condition(tx: &mut PgConnection, flow: &Flow) -> Result<(), sqlx::Error> {
    let fingerprint = flow.conditions.fingerprint();
    let condition = sqlx::query_scalar!(
        "INSERT INTO condition_fingerprints (project_id, fingerprint, dims)
         VALUES ($1, $2, $3)
         ON CONFLICT (project_id, fingerprint) DO UPDATE SET dims = condition_fingerprints.dims
         RETURNING id",
        flow.project_id,
        fingerprint.as_bytes().as_slice(),
        serde_json::to_value(flow.conditions.dims()).unwrap_or_default()
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE flow_executions SET condition_fingerprint_id = $2
          WHERE id = $1 AND condition_fingerprint_id IS DISTINCT FROM $2",
        flow.id,
        condition
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}

async fn store_failure(tx: &mut PgConnection, flow: &Flow) -> Result<(), sqlx::Error> {
    let signature = FailureFacts {
        flow: &flow.identity,
        step: flow.failed_step.as_deref(),
        message: flow.failure_message.as_deref().unwrap_or_default(),
    }
    .signature();
    sqlx::query!(
        "INSERT INTO failure_fingerprints (flow_execution_id, project_id, fingerprint,
                                           step_kind, message)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (flow_execution_id) DO UPDATE
           SET fingerprint = EXCLUDED.fingerprint, step_kind = EXCLUDED.step_kind,
               message = EXCLUDED.message, computed_at = now()
         WHERE (failure_fingerprints.fingerprint, failure_fingerprints.step_kind,
                failure_fingerprints.message)
               IS DISTINCT FROM (EXCLUDED.fingerprint, EXCLUDED.step_kind, EXCLUDED.message)",
        flow.id,
        flow.project_id,
        signature.fingerprint.as_str(),
        signature.step_kind,
        signature.message
    )
    .execute(&mut *tx)
    .await?;
    // The first sighting of a fingerprint records when it was first seen (issue #84).
    sqlx::query!(
        "INSERT INTO failure_triage (project_id, fingerprint) VALUES ($1, $2)
         ON CONFLICT (project_id, fingerprint) DO NOTHING",
        flow.project_id,
        signature.fingerprint.as_str()
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use fleet_core::fingerprint::FailureFacts;
    use fleet_protocol::{
        event::{EventBody, MaestroBuild, RunEnded},
        frame::{Hello, Sample, Telemetry},
        journal::{ClockSample, SourceKind, Subject},
    };
    use sqlx::PgPool;
    use time::OffsetDateTime;

    use super::super::tests::{Agent, Script, SuiteIds, publisher, suite_run_scripted};
    use crate::{Store, ingest::Transport, telemetry::TelemetryBatch};

    const DEATH_A: &str = "maestro.android.DeviceServerDiedException: Device server died during \
        'viewHierarchy' on emulator-5602 (2267ms since last byte, connection age 34368ms): \
        StatusRuntimeException: UNAVAILABLE";
    const DEATH_B: &str = "maestro.android.DeviceServerDiedException: Device server died during \
        'viewHierarchy' on emulator-5554 (991ms since last byte, connection age 20211ms): \
        StatusRuntimeException: UNAVAILABLE";
    const ASSERTION: &str = r#"Assertion is false: "Order confirmed" is visible"#;
    const STEP: &str = r#"Tap on "Pay""#;

    const GIB: u64 = 1 << 30;

    /// Runs one simulated agent through `flows`, as `tweak` customises its hello. The agent
    /// reports `host_memory` as its host's total memory first, when given.
    async fn run(
        store: &Store,
        run_id: &str,
        flows: &[Script<'_>],
        host_memory: Option<u64>,
        tweak: impl FnOnce(&mut Hello),
    ) -> Agent {
        run_as(store, run_id, flows, host_memory, None, tweak).await
    }

    /// [`run`], with the Maestro build its session log names, as `(version, profile_supported)`
    /// (`None`: never named). The build arrives after the flows are final, as it does when the
    /// observer reads the log late, and before `run.ended`.
    async fn run_as(
        store: &Store,
        run_id: &str,
        flows: &[Script<'_>],
        host_memory: Option<u64>,
        maestro: Option<(&str, bool)>,
        tweak: impl FnOnce(&mut Hello),
    ) -> Agent {
        let p = publisher(store).await;
        let mut a = Agent::new(run_id, "e2e");
        let mut hello = a.hello(None);
        tweak(&mut hello);
        let opened = store
            .open_run(&p, &hello, Transport::WebSocket)
            .await
            .unwrap();
        if let Some(total) = host_memory {
            store.ensure_partitions(1, 1).await.unwrap();
            let sample = Sample {
                at: ClockSample {
                    wall: OffsetDateTime::now_utc(),
                    mono_ms: 0,
                },
                host_cpu_pct: None,
                host_mem_used_bytes: None,
                host_mem_total_bytes: Some(total),
                maestro_cpu_pct: None,
                maestro_rss_bytes: None,
                agent_cpu_pct: None,
                agent_rss_bytes: None,
                host_load_1m: None,
                host_cpu_cores: None,
                host_swap_used_bytes: None,
                host_swap_total_bytes: None,
                host_psi_cpu_pct: None,
                host_psi_memory_pct: None,
                host_psi_io_pct: None,
            };
            store
                .store_telemetry(&[TelemetryBatch {
                    agent_run_id: a.id,
                    project_id: opened.handle.project_id,
                    worker_id: opened.handle.worker_id,
                    batch: Telemetry {
                        tseq_from: 1,
                        tseq_to: 1,
                        dropped_since_last: 0,
                        devices: Vec::new(),
                        samples: vec![sample],
                    },
                }])
                .await
                .unwrap();
        }
        let mut events = suite_run_scripted(&mut a, SuiteIds::new(), flows);
        // The suite closes at `process.exited`, which makes the flows final: they go first.
        let ended = events.pop().expect("run.ended is last");
        let applied = store.apply_batch(&opened.handle, &events).await.unwrap();
        assert!(applied.rejected.is_empty(), "{:?}", applied.rejected);
        // The end takes its place after the build, one event per batch.
        a.seq = ended.seq - 1;
        let mut rest = Vec::new();
        if let Some((version, profile_supported)) = maestro {
            rest.push(a.event(
                Subject::default(),
                SourceKind::DebugLog,
                &EventBody::MaestroDetected(MaestroBuild {
                    version: version.into(),
                    profile_supported,
                }),
            ));
        }
        rest.push(a.event(
            Subject::default(),
            SourceKind::AgentObserved,
            &EventBody::RunEnded(RunEnded {
                interrupted: false,
                output_bytes_dropped: 0,
            }),
        ));
        for e in &rest {
            let applied = store
                .apply_batch(&opened.handle, std::slice::from_ref(e))
                .await
                .unwrap();
            assert!(applied.rejected.is_empty(), "{:?}", applied.rejected);
        }
        a
    }

    fn checkout(failure: Option<&'static str>) -> Script<'static> {
        Script {
            name: "checkout",
            failure,
            failed_step: failure.map(|_| STEP),
        }
    }

    const LOGIN: Script<'static> = Script {
        name: "login",
        failure: None,
        failed_step: None,
    };

    struct Row {
        run: uuid::Uuid,
        flow: String,
        fingerprint: Option<String>,
        step_kind: Option<String>,
        message: Option<String>,
    }

    async fn failure_rows(store: &Store) -> Vec<Row> {
        sqlx::query!(
            "SELECT f.agent_run_id, f.flow_name, ff.fingerprint AS \"fingerprint?\",
                    ff.step_kind AS \"step_kind?\", ff.message AS \"message?\"
               FROM flow_executions f
               LEFT JOIN failure_fingerprints ff ON ff.flow_execution_id = f.id
              ORDER BY f.started_at, f.flow_name",
        )
        .fetch_all(store.pool())
        .await
        .unwrap()
        .into_iter()
        .map(|r| Row {
            run: r.agent_run_id,
            flow: r.flow_name,
            fingerprint: r.fingerprint,
            step_kind: r.step_kind,
            message: r.message,
        })
        .collect()
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_failed_flow_gets_a_fingerprint_and_equivalent_failures_share_it(pool: PgPool) {
        let store = Store {
            pool,
            origin: uuid::Uuid::now_v7(),
        };
        let a = run(
            &store,
            "300",
            &[LOGIN, checkout(Some(DEATH_A))],
            None,
            |_| {},
        )
        .await;
        let b = run(&store, "301", &[checkout(Some(DEATH_B))], None, |_| {}).await;
        let c = run(&store, "302", &[checkout(Some(ASSERTION))], None, |_| {}).await;
        let rows = failure_rows(&store).await;
        let of = |run: uuid::Uuid, flow: &str| {
            rows.iter()
                .find(|r| r.run == run && r.flow == flow)
                .unwrap_or_else(|| panic!("no {flow} in run {run}"))
        };
        // A pass has no failure fingerprint.
        assert_eq!(of(a.id, "login").fingerprint, None);
        // The same failure on another emulator, at other times, is the same fingerprint …
        let first = of(a.id, "checkout");
        let second = of(b.id, "checkout");
        assert!(first.fingerprint.is_some());
        assert_eq!(first.fingerprint, second.fingerprint);
        // … a different failure of the same flow and step is not.
        assert_ne!(first.fingerprint, of(c.id, "checkout").fingerprint);
        // What it was made from is kept: the step, and the message without its volatile parts.
        assert_eq!(first.step_kind.as_deref(), Some(STEP));
        assert_eq!(
            first.message.as_deref(),
            Some(
                "maestro.android.DeviceServerDiedException: Device server died during \
                 'viewHierarchy' on <serial> (<dur> since last byte, connection age <dur>): \
                 StatusRuntimeException: UNAVAILABLE"
            )
        );
        // The flow's identity is its repository path (the JUnit `file`).
        let expected = FailureFacts {
            flow: "flows/checkout.yaml",
            step: Some(STEP),
            message: DEATH_A,
        }
        .signature();
        assert_eq!(
            first.fingerprint.as_deref(),
            Some(expected.fingerprint.as_str())
        );
    }

    struct Condition {
        flow: String,
        id: Option<uuid::Uuid>,
        dims: Option<serde_json::Value>,
    }

    async fn conditions(store: &Store) -> Vec<Condition> {
        sqlx::query!(
            "SELECT f.flow_name, f.condition_fingerprint_id AS id, c.dims AS \"dims?\"
               FROM flow_executions f
               LEFT JOIN condition_fingerprints c ON c.id = f.condition_fingerprint_id
              ORDER BY f.started_at, f.flow_name",
        )
        .fetch_all(store.pool())
        .await
        .unwrap()
        .into_iter()
        .map(|r| Condition {
            flow: r.flow_name,
            id: r.id,
            dims: r.dims,
        })
        .collect()
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn final_flows_get_a_condition_fingerprint_that_says_unknown_for_what_is_unknown(
        pool: PgPool,
    ) {
        let store = Store {
            pool,
            origin: uuid::Uuid::now_v7(),
        };
        run(
            &store,
            "310",
            &[LOGIN, checkout(Some(ASSERTION))],
            Some(16 * GIB),
            |_| {},
        )
        .await;
        run(&store, "311", &[LOGIN], Some(16 * GIB - GIB / 2), |_| {}).await;
        // Another host class: other architecture, no memory report.
        run(&store, "312", &[LOGIN], None, |h| {
            h.agent.arch = "aarch64".into();
        })
        .await;
        let all = conditions(&store).await;
        let [a_login, a_checkout, b_login, c_login] = all.as_slice() else {
            panic!("expected four flows, got {}", all.len());
        };
        assert_eq!(
            (a_login.flow.as_str(), a_checkout.flow.as_str()),
            ("login", "checkout")
        );
        // Comparable runs share one fingerprint; a flow's pass and failure in one run do too.
        assert!(a_login.id.is_some());
        assert_eq!(a_login.id, a_checkout.id);
        assert_eq!(a_login.id, b_login.id);
        assert_ne!(a_login.id, c_login.id);
        // What the store knows is recorded; what it doesn't is the word `unknown`.
        assert_eq!(
            a_login.dims,
            Some(serde_json::json!({
                "arch": "x86_64",
                "ci_provider": "generic",
                "device_api_level": "unknown",
                "device_model": "unknown",
                "host_cores": "unknown",
                "host_memory": "16GiB",
                "maestro": "unknown",
                "os": "linux",
            }))
        );
        assert_eq!(
            c_login.dims.as_ref().and_then(|d| d.get("host_memory")),
            Some(&serde_json::json!("unknown"))
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_condition_fingerprint_includes_the_maestro_version(pool: PgPool) {
        let store = Store {
            pool,
            origin: uuid::Uuid::now_v7(),
        };
        let first = run_as(
            &store,
            "340",
            &[LOGIN],
            None,
            Some(("2.12.1", true)),
            |_| {},
        )
        .await;
        // The same major.minor is the same condition; another minor, or none named, is not.
        run_as(
            &store,
            "341",
            &[LOGIN],
            None,
            Some(("2.12.3", true)),
            |_| {},
        )
        .await;
        run_as(
            &store,
            "342",
            &[LOGIN],
            None,
            Some(("2.13.0", false)),
            |_| {},
        )
        .await;
        run_as(&store, "343", &[LOGIN], None, None, |_| {}).await;
        // The run keeps the version it reported, and whether its profile was supported.
        let stored = sqlx::query!(
            "SELECT maestro_version, maestro_profile_supported FROM agent_runs WHERE id = $1",
            first.id
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(stored.maestro_version.as_deref(), Some("2.12.1"));
        assert_eq!(stored.maestro_profile_supported, Some(true));
        let all = conditions(&store).await;
        let [a, b, c, unknown] = all.as_slice() else {
            panic!("expected four flows, got {}", all.len());
        };
        assert!(a.id.is_some());
        assert_eq!(a.id, b.id);
        assert_ne!(a.id, c.id);
        assert_ne!(a.id, unknown.id);
        let maestro = |c: &Condition| c.dims.as_ref().and_then(|d| d.get("maestro")).cloned();
        assert_eq!(maestro(a), Some(serde_json::json!("2.12")));
        assert_eq!(maestro(c), Some(serde_json::json!("2.13")));
        assert_eq!(maestro(unknown), Some(serde_json::json!("unknown")));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn nothing_is_fingerprinted_before_the_outcome_is_final(pool: PgPool) {
        let store = Store {
            pool,
            origin: uuid::Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        let mut a = Agent::new("320", "e2e");
        let opened = store
            .open_run(&p, &a.hello(None), Transport::WebSocket)
            .await
            .unwrap();
        let events = suite_run_scripted(&mut a, SuiteIds::new(), &[checkout(Some(ASSERTION))]);
        // Up to the live `flow.ended`: the suite is still open, so the failure is provisional.
        let live = events
            .iter()
            .take_while(|e| e.kind != "process.exited")
            .count();
        let (live_part, rest) = events.split_at(live);
        store.apply_batch(&opened.handle, live_part).await.unwrap();
        let provisional = sqlx::query_scalar!(
            "SELECT finality::text AS \"finality!\" FROM flow_executions WHERE agent_run_id = $1",
            a.id
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(provisional, "provisional");
        assert!(
            failure_rows(&store)
                .await
                .iter()
                .all(|r| r.fingerprint.is_none())
        );
        assert!(conditions(&store).await.iter().all(|c| c.id.is_none()));
        // The report arrives and the failure is final.
        store.apply_batch(&opened.handle, rest).await.unwrap();
        assert!(
            failure_rows(&store)
                .await
                .iter()
                .all(|r| r.fingerprint.is_some())
        );
        assert!(conditions(&store).await.iter().all(|c| c.id.is_some()));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_flaky_history_view_lists_final_passes_and_failures_with_their_conditions(
        pool: PgPool,
    ) {
        let store = Store {
            pool,
            origin: uuid::Uuid::now_v7(),
        };
        run(
            &store,
            "330",
            &[LOGIN, checkout(Some(ASSERTION))],
            None,
            |_| {},
        )
        .await;
        let rows = sqlx::query!(
            "SELECT flow AS \"flow!\", state AS \"state!\", outcome_source::text AS \"source!\",
                    finality::text AS \"finality!\", is_default_branch AS \"default!\",
                    condition_fingerprint AS \"condition!\"
               FROM flaky_history ORDER BY flow",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        let summary: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r.flow.as_str(),
                    r.state.as_str(),
                    r.source.as_str(),
                    r.finality.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("flows/checkout.yaml", "failed", "junit_final", "verified"),
                ("flows/login.yaml", "passed", "junit_final", "verified"),
            ]
        );
        // This run's ref is unknown: it is not a default-branch run.
        assert!(rows.iter().all(|r| !r.default));
        assert_eq!(rows[0].condition, rows[1].condition);
        assert_eq!(rows[0].condition.len(), 32);
    }
}
