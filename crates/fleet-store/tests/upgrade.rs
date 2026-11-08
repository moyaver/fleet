// SPDX-License-Identifier: AGPL-3.0-only
//! The upgrade rehearsal (docs/src/operator-guide.md#upgrades): rows written under each earlier
//! release's migrations survive the current ones, and the current queries read them.
//!
//! For every release directory under `FLEET_REHEARSAL_DIR` (filled by `cargo xtask
//! upgrade-rehearsal` from the release tags' `migrations/`), the test:
//!
//! 1. applies that release's migrations to a fresh database and seeds representative rows,
//! 2. runs the current migrations through [`Store::migrate`], as `fleet serve` does,
//! 3. checks every seeded row is unchanged and the current read queries return the seeded data,
//! 4. checks that the release's own migrator now refuses the upgraded database.
//!
//! Ignored by default: it needs those files and a Postgres 18 server (`DATABASE_URL`).

use std::{
    fs,
    path::{Path, PathBuf},
};

use fleet_store::{PoolOptions, SchemaStatus, Store};
use sqlx::{PgPool, migrate::Migrator};
use uuid::Uuid;

const PROJECT: &str = "00000000-0000-4000-8000-0000000000a1";
const ATTEMPT: &str = "00000000-0000-4000-8000-0000000000a4";
const CHECKOUT: &str = "00000000-0000-4000-8000-0000000000c2";
const SCREENSHOT: &str = "00000000-0000-4000-8000-0000000000d1";

/// Every seeded table, as one sorted line per row: what must read the same after the upgrade.
const FINGERPRINT: &[(&str, &str)] = &[
    (
        "projects",
        "SELECT slug || '|' || name FROM projects ORDER BY slug",
    ),
    (
        "api_keys",
        "SELECT prefix || '|' || encode(token_hash, 'hex') FROM api_keys ORDER BY prefix",
    ),
    (
        "pipeline_runs",
        "SELECT provider || '|' || provider_run_key || '|' || commit_sha FROM pipeline_runs ORDER BY 1",
    ),
    (
        "jobs",
        "SELECT job_key || '|' || lifecycle || '|' || outcome || '|' || outcome_source FROM jobs ORDER BY 1",
    ),
    (
        "agent_runs",
        "SELECT agent_version || '|' || protocol_version || '|' || state || '|' || maestro_exit_code || '|' || committed_seq FROM agent_runs ORDER BY 1",
    ),
    (
        "suite_executions",
        "SELECT shard_mode || '|' || planned_count || '|' || outcome || '|' || report_status FROM suite_executions ORDER BY 1",
    ),
    (
        "device_sessions",
        "SELECT serial || '|' || state FROM device_sessions ORDER BY 1",
    ),
    (
        "flow_executions",
        "SELECT flow_name || '|' || state || '|' || finality || '|' || coalesce(failure_message, '-') || '|' || duration_ms FROM flow_executions ORDER BY ordinal",
    ),
    (
        "flow_claims",
        "SELECT origin || '|' || source || '|' || outcome || '|' || claim_order FROM flow_claims ORDER BY 1",
    ),
    (
        "step_executions",
        "SELECT flow_execution_id || '|' || step_index || '|' || status || '|' || coalesce(label, '-') FROM step_executions ORDER BY flow_execution_id, step_index",
    ),
    (
        "agent_events",
        "SELECT agent_run_id || '|' || seq || '|' || type || '|' || data::text FROM agent_events ORDER BY seq",
    ),
    (
        "device_samples",
        "SELECT round(emu_cpu_pct::numeric, 1) || '|' || guest_mem_available_bytes FROM device_samples ORDER BY 1",
    ),
    (
        "blobs",
        "SELECT encode(sha256, 'hex') || '|' || size_bytes || '|' || state FROM blobs ORDER BY 1",
    ),
    (
        "artifacts",
        "SELECT class || '|' || state || '|' || encode(sha256, 'hex') || '|' || path || '|' || retention_class FROM artifacts ORDER BY path",
    ),
];

#[tokio::test]
#[ignore = "needs release migrations and Postgres: run `cargo xtask upgrade-rehearsal`"]
async fn rows_from_every_release_survive_the_current_migrations() {
    let base = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL should point at a Postgres 18 server (a database may be created)");
    let releases = std::env::var("FLEET_REHEARSAL_DIR").expect(
        "FLEET_REHEARSAL_DIR should name the directory `cargo xtask upgrade-rehearsal` fills",
    );
    let mut dirs: Vec<PathBuf> = fs::read_dir(&releases)
        .expect("reading the rehearsal directory")
        .map(|e| e.expect("reading a rehearsal entry").path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    assert!(!dirs.is_empty(), "no release directories under {releases}");
    assert_forward_only(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations"));
    for dir in &dirs {
        let release = dir
            .file_name()
            .and_then(|n| n.to_str())
            .expect("a release directory should have a UTF-8 name")
            .to_owned();
        rehearse(&base, &release, dir).await;
    }
}

/// Every migration is a plain up migration (`NNNN_name.sql`): there is no `.down.sql`, so the
/// only way back is a restore (operator-guide: rollback).
fn assert_forward_only(dir: &Path) {
    let mut count = 0;
    for entry in fs::read_dir(dir).expect("reading the current migrations") {
        let name = entry.expect("reading a migration entry").file_name();
        let name = name.to_string_lossy();
        let numbered = name.len() > 5
            && name[..4].bytes().all(|b| b.is_ascii_digit())
            && name.as_bytes()[4] == b'_';
        assert!(
            numbered && name.ends_with(".sql") && !name.contains(".down."),
            "{name}: migrations must be forward-only NNNN_name.sql files"
        );
        count += 1;
    }
    assert!(count > 0, "no migrations found in {}", dir.display());
}

#[expect(
    clippy::print_stdout,
    reason = "the rehearsal reports the rows it checked for each release"
)]
async fn rehearse(base: &str, release: &str, dir: &Path) {
    let database = format!(
        "fleet_upgrade_{}",
        release.replace(|c: char| !c.is_ascii_alphanumeric(), "_")
    );
    let admin = PgPool::connect(base).await.expect("connecting to Postgres");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {database} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .expect("dropping a leftover rehearsal database");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {database}")))
        .execute(&admin)
        .await
        .expect("creating the rehearsal database");
    let url = with_database(base, &database);

    // 1. The release's own migrations, then rows written under its schema.
    let old_migrator = Migrator::new(dir)
        .await
        .unwrap_or_else(|e| panic!("{release}: reading its migrations: {e}"));
    let old_count = old_migrator.iter().count();
    let pool = PgPool::connect(&url)
        .await
        .expect("connecting to the rehearsal database");
    old_migrator
        .run(&pool)
        .await
        .unwrap_or_else(|e| panic!("{release}: applying its own migrations: {e}"));
    seed(&pool, release).await;
    let before = fingerprint(&pool).await;
    assert!(!before.is_empty(), "{release}: the seed wrote nothing");

    // 2. The current build starts on it: refuses nothing, migrates, reports Current.
    let store = Store::connect_lazy(&url, PoolOptions::default()).expect("a valid rehearsal URL");
    match store.schema_status().await.expect("reading the schema") {
        SchemaStatus::Behind { applied, known } => {
            assert_eq!(applied, old_count, "{release}: applied migrations");
            assert!(
                known > applied,
                "{release}: the current build should know more"
            );
        }
        other => panic!("{release}: expected the old schema to be behind, got {other:?}"),
    }
    store
        .migrate()
        .await
        .unwrap_or_else(|e| panic!("{release}: current migrations: {e:#?}"));
    assert_eq!(
        store.schema_status().await.expect("reading the schema"),
        SchemaStatus::Current,
        "{release}: the schema should be current after migrating"
    );
    store
        .ensure_partitions(1, 7)
        .await
        .unwrap_or_else(|e| panic!("{release}: partitions at start-up: {e:#?}"));

    // 3. Rows intact, and readable by the current queries.
    let after = fingerprint(&pool).await;
    assert_eq!(before, after, "{release}: the upgrade changed seeded rows");
    read_back(&store, release).await;

    // 4. The release's migrator refuses the upgraded database, so an old binary cannot run on it.
    let refused = old_migrator.run(&pool).await;
    assert!(
        refused.is_err(),
        "{release}: the release's migrator should refuse a newer schema"
    );

    store.close().await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {database} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .expect("dropping the rehearsal database");
    println!(
        "{release}: {} rows checked across {} tables",
        before.len(),
        FINGERPRINT.len()
    );
}

/// Representative rows of the release's schema: a project with an API key, a CI attempt with a
/// closed job, an agent run with a suite, two flows (one passed, one failed with steps, events,
/// telemetry, and two artifacts). Uses only columns every release has.
async fn seed(pool: &PgPool, release: &str) {
    sqlx::raw_sql(
        "INSERT INTO projects (id, slug, name) VALUES
           ('00000000-0000-4000-8000-0000000000a1', 'rehearsal', 'Upgrade rehearsal');
         INSERT INTO api_keys (id, project_id, name, prefix, token_hash) VALUES
           ('00000000-0000-4000-8000-0000000000a2', '00000000-0000-4000-8000-0000000000a1', 'ci',
            'mf_live_rhrs', sha256(convert_to('rehearsal-key', 'UTF8')));
         INSERT INTO pipeline_runs (id, project_id, provider, provider_run_key, repository, commit_sha, git_ref)
           VALUES ('00000000-0000-4000-8000-0000000000a3', '00000000-0000-4000-8000-0000000000a1',
                   'github_actions', 'example/app:1001', 'example/app',
                   '0123456789abcdef0123456789abcdef01234567', 'refs/heads/main');
         INSERT INTO pipeline_attempts (id, project_id, pipeline_run_id, attempt) VALUES
           ('00000000-0000-4000-8000-0000000000a4', '00000000-0000-4000-8000-0000000000a1',
            '00000000-0000-4000-8000-0000000000a3', 1);
         INSERT INTO jobs (id, project_id, pipeline_attempt_id, job_key, name, lifecycle, outcome, outcome_source, closed_at)
           VALUES ('00000000-0000-4000-8000-0000000000a5', '00000000-0000-4000-8000-0000000000a1',
                   '00000000-0000-4000-8000-0000000000a4', 'android', 'Android UI', 'closed',
                   'failed', 'junit_final', now());",
    )
    .execute(pool)
    .await
    .expect("seeding projects, CI identity and the job");

    sqlx::query(
        "INSERT INTO agent_runs (id, project_id, job_id, trust_level, run_day, agent_version,
                                 protocol_version, state, end_reason, committed_seq, end_seq,
                                 maestro_exit_code, ended_at)
         VALUES ('00000000-0000-4000-8000-0000000000a6', '00000000-0000-4000-8000-0000000000a1',
                 '00000000-0000-4000-8000-0000000000a5', 'trusted',
                 (now() AT TIME ZONE 'UTC')::date, $1, 1, 'ended', 'completed', 3, 3, 1, now())",
    )
    .bind(release.trim_start_matches('v'))
    .execute(pool)
    .await
    .expect("seeding the agent run");

    sqlx::raw_sql(
        "INSERT INTO suite_executions (id, project_id, agent_run_id, job_id, shard_mode, shard_count,
                                       planned_count, lifecycle, outcome, outcome_source,
                                       report_status, exit_code, started_at, ended_at)
           VALUES ('00000000-0000-4000-8000-0000000000a7', '00000000-0000-4000-8000-0000000000a1',
                   '00000000-0000-4000-8000-0000000000a6', '00000000-0000-4000-8000-0000000000a5',
                   'split', 1, 2, 'closed', 'failed', 'junit_final', 'complete', 1,
                   now() - interval '5 minutes', now());
         INSERT INTO device_sessions (id, project_id, agent_run_id, serial, state, first_seen_at, last_seen_at)
           VALUES ('00000000-0000-4000-8000-0000000000a8', '00000000-0000-4000-8000-0000000000a1',
                   '00000000-0000-4000-8000-0000000000a6', 'emulator-5554', 'released',
                   now() - interval '5 minutes', now());
         INSERT INTO shards (id, project_id, agent_run_id, suite_execution_id, shard_index,
                             device_session_id, planned, planned_count)
           VALUES ('00000000-0000-4000-8000-0000000000a9', '00000000-0000-4000-8000-0000000000a1',
                   '00000000-0000-4000-8000-0000000000a6', '00000000-0000-4000-8000-0000000000a7', 1,
                   '00000000-0000-4000-8000-0000000000a8',
                   '[\"flows/login.yaml\", \"flows/checkout.yaml\"]', 2);
         INSERT INTO flow_definitions (id, project_id, repo_path, last_name) VALUES
           ('00000000-0000-4000-8000-0000000000b1', '00000000-0000-4000-8000-0000000000a1', 'flows/login.yaml', 'login'),
           ('00000000-0000-4000-8000-0000000000b2', '00000000-0000-4000-8000-0000000000a1', 'flows/checkout.yaml', 'checkout');",
    )
    .execute(pool)
    .await
    .expect("seeding the suite, device and shard");

    seed_flows(pool).await;
}

/// The flows of the suite: one passed and one failed with steps, claims, events, and the
/// partitions their rows need.
async fn seed_flows(pool: &PgPool) {
    sqlx::raw_sql(
        "INSERT INTO flow_executions (id, project_id, agent_run_id, suite_execution_id, shard_id,
                                      device_session_id, flow_definition_id, flow_name, flow_path,
                                      shard_number, ordinal, started, state, outcome_source, finality,
                                      failure_message, started_at, ended_at, duration_ms,
                                      pipeline_attempt_id, job_id)
           VALUES
           ('00000000-0000-4000-8000-0000000000c1', '00000000-0000-4000-8000-0000000000a1',
            '00000000-0000-4000-8000-0000000000a6', '00000000-0000-4000-8000-0000000000a7',
            '00000000-0000-4000-8000-0000000000a9', '00000000-0000-4000-8000-0000000000a8',
            '00000000-0000-4000-8000-0000000000b1', 'login', 'flows/login.yaml', 1, 1, true,
            'passed', 'junit_final', 'verified', NULL, now() - interval '5 minutes',
            now() - interval '4 minutes', 4200, '00000000-0000-4000-8000-0000000000a4',
            '00000000-0000-4000-8000-0000000000a5'),
           ('00000000-0000-4000-8000-0000000000c2', '00000000-0000-4000-8000-0000000000a1',
            '00000000-0000-4000-8000-0000000000a6', '00000000-0000-4000-8000-0000000000a7',
            '00000000-0000-4000-8000-0000000000a9', '00000000-0000-4000-8000-0000000000a8',
            '00000000-0000-4000-8000-0000000000b2', 'checkout', 'flows/checkout.yaml', 1, 2, true,
            'failed', 'junit_final', 'verified', 'Assertion is false: \"Pay now\" is visible',
            now() - interval '4 minutes', now(), 9100, '00000000-0000-4000-8000-0000000000a4',
            '00000000-0000-4000-8000-0000000000a5');
         INSERT INTO flow_claims (flow_execution_id, source, origin, outcome, claim_order, observed_at, detail) VALUES
           ('00000000-0000-4000-8000-0000000000c1', 'junit_final', 'junit:login', 'passed', 1, now(), '{}'),
           ('00000000-0000-4000-8000-0000000000c2', 'junit_final', 'junit:checkout', 'failed', 2, now(), '{\"file\": \"checkout.xml\"}');
         SELECT fleet_ensure_partitions('step_executions'::regclass, ((now() AT TIME ZONE 'UTC')::date - 1), ((now() AT TIME ZONE 'UTC')::date + 1));
         SELECT fleet_ensure_partitions('agent_events'::regclass, ((now() AT TIME ZONE 'UTC')::date - 1), ((now() AT TIME ZONE 'UTC')::date + 1));
         SELECT fleet_ensure_partitions('device_samples'::regclass, ((now() AT TIME ZONE 'UTC')::date - 1), ((now() AT TIME ZONE 'UTC')::date + 1));
         INSERT INTO step_executions (run_day, flow_execution_id, step_index, label, status, source, started_at, ended_at) VALUES
           ((now() AT TIME ZONE 'UTC')::date, '00000000-0000-4000-8000-0000000000c2', 0, 'launchApp', 'completed', 'artifact_file', now() - interval '4 minutes', now() - interval '4 minutes'),
           ((now() AT TIME ZONE 'UTC')::date, '00000000-0000-4000-8000-0000000000c2', 1, 'tapOn: Add to cart', 'completed', 'artifact_file', now() - interval '4 minutes', now() - interval '4 minutes'),
           ((now() AT TIME ZONE 'UTC')::date, '00000000-0000-4000-8000-0000000000c2', 2, 'assertVisible: Pay now', 'failed', 'artifact_file', now(), now());
         INSERT INTO agent_events (run_day, agent_run_id, seq, type, v, src_kind, occurred_at, mono_ms, apply_status, data, flow_execution_id) VALUES
           ((now() AT TIME ZONE 'UTC')::date, '00000000-0000-4000-8000-0000000000a6', 1, 'flow.finished', 1, 'maestro', now() - interval '4 minutes', 1000, 'applied', '{\"flow\": \"login\", \"state\": \"passed\"}', '00000000-0000-4000-8000-0000000000c1'),
           ((now() AT TIME ZONE 'UTC')::date, '00000000-0000-4000-8000-0000000000a6', 2, 'flow.finished', 1, 'maestro', now(), 2000, 'applied', '{\"flow\": \"checkout\", \"state\": \"failed\"}', '00000000-0000-4000-8000-0000000000c2');
",
    )
    .execute(pool)
    .await
    .expect("seeding flows, claims, steps and events");
    seed_artifacts(pool).await;
}

/// Device telemetry, blobs and the artifacts of the failed flow and of its run.
async fn seed_artifacts(pool: &PgPool) {
    sqlx::raw_sql(
        "INSERT INTO device_samples (ts, device_session_id, agent_run_id, project_id, emu_cpu_pct, emu_rss_bytes, adb_state, boot_completed, guest_cpu_pct, guest_mem_available_bytes) VALUES
           (now(), '00000000-0000-4000-8000-0000000000a8', '00000000-0000-4000-8000-0000000000a6', '00000000-0000-4000-8000-0000000000a1', 12.5, 1073741824, 1, true, 3.0, 2147483648);
         INSERT INTO blobs (project_id, sha256, size_bytes, state) VALUES
           ('00000000-0000-4000-8000-0000000000a1', sha256(convert_to('rehearsal-screenshot', 'UTF8')), 20, 'present'),
           ('00000000-0000-4000-8000-0000000000a1', sha256(convert_to('rehearsal-log', 'UTF8')), 11, 'present');
         INSERT INTO artifacts (id, project_id, agent_run_id, job_id, suite_execution_id, flow_execution_id, class,
                                content_type, served_type, size_bytes, sha256, path, name, retention_class,
                                state, received_at, available_at) VALUES
           ('00000000-0000-4000-8000-0000000000d1', '00000000-0000-4000-8000-0000000000a1',
            '00000000-0000-4000-8000-0000000000a6', '00000000-0000-4000-8000-0000000000a5',
            '00000000-0000-4000-8000-0000000000a7', '00000000-0000-4000-8000-0000000000c2',
            'failure_screenshot', 'image/png', 'image/png', 20,
            sha256(convert_to('rehearsal-screenshot', 'UTF8')), 'screenshots/checkout-failure.png',
            'checkout-failure.png', 'failure_evidence', 'available', now(), now()),
           ('00000000-0000-4000-8000-0000000000d2', '00000000-0000-4000-8000-0000000000a1',
            '00000000-0000-4000-8000-0000000000a6', '00000000-0000-4000-8000-0000000000a5',
            NULL, NULL, 'maestro_log', 'text/plain', 'text/plain', 11,
            sha256(convert_to('rehearsal-log', 'UTF8')), 'maestro.log', 'maestro.log',
            'failure_evidence', 'available', now(), now());",
    )
    .execute(pool)
    .await
    .expect("seeding telemetry, blobs and artifacts");
}

/// The seeded tables, one line per row, as text: equal before and after the upgrade iff no row
/// changed.
async fn fingerprint(pool: &PgPool) -> Vec<String> {
    let mut lines = Vec::new();
    for (table, sql) in FINGERPRINT {
        let rows: Vec<String> = sqlx::query_scalar(*sql)
            .fetch_all(pool)
            .await
            .unwrap_or_else(|e| panic!("fingerprinting {table}: {e}"));
        lines.extend(rows.into_iter().map(|row| format!("{table}: {row}")));
    }
    lines
}

/// The current read paths over the seeded data: the dashboard's project list, attempt list,
/// attempt snapshot, a flow's artifacts and failure triage.
async fn read_back(store: &Store, release: &str) {
    let project = Uuid::parse_str(PROJECT).expect("constant");
    let attempt = Uuid::parse_str(ATTEMPT).expect("constant");
    let checkout = Uuid::parse_str(CHECKOUT).expect("constant");
    let screenshot = Uuid::parse_str(SCREENSHOT).expect("constant");

    let projects = store.projects().await.expect("listing projects");
    assert!(
        projects
            .iter()
            .any(|p| p.id == project && p.slug == "rehearsal"),
        "{release}: the seeded project should be listed"
    );

    let attempts = store.attempts(project, 10).await.expect("listing attempts");
    assert_eq!(
        attempts.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![attempt],
        "{release}: the seeded attempt should be listed"
    );

    let snapshot = store
        .snapshot(project, attempt)
        .await
        .expect("reading the attempt")
        .unwrap_or_else(|| panic!("{release}: the attempt should have a snapshot"));
    assert_eq!(snapshot.jobs.len(), 1, "{release}: jobs");
    assert_eq!(snapshot.agent_runs.len(), 1, "{release}: agent runs");
    assert_eq!(snapshot.suites.len(), 1, "{release}: suites");
    assert_eq!(snapshot.devices.len(), 1, "{release}: devices");
    let mut flows: Vec<(&str, &str)> = snapshot
        .flows
        .iter()
        .map(|f| (f.name.as_str(), f.state.as_str()))
        .collect();
    flows.sort_unstable();
    assert_eq!(
        flows,
        vec![("checkout", "failed"), ("login", "passed")],
        "{release}: flow names and states"
    );
    let failed = snapshot
        .flows
        .iter()
        .find(|f| f.id == checkout)
        .unwrap_or_else(|| panic!("{release}: the failed flow should be in the snapshot"));
    assert_eq!(
        failed.failure_message.as_deref(),
        Some("Assertion is false: \"Pay now\" is visible"),
        "{release}: failure message"
    );
    assert_eq!(failed.steps, 3, "{release}: the steps of the failed flow");

    let artifacts = store
        .flow_artifacts(project, checkout)
        .await
        .expect("listing a flow's artifacts")
        .unwrap_or_else(|| panic!("{release}: the flow should exist"));
    assert!(
        artifacts.iter().any(|a| a.id == screenshot),
        "{release}: the failure screenshot should be listed"
    );

    let triage = store
        .failure_triage(project, &["never-seen".to_owned()])
        .await
        .expect("reading failure triage");
    assert!(
        triage.is_empty(),
        "{release}: an unseen fingerprint has no triage"
    );
}

/// `url` with its database replaced; the server part and query string are kept.
fn with_database(url: &str, database: &str) -> String {
    let (head, query) = match url.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (url, None),
    };
    let (server, _) = head
        .rsplit_once('/')
        .expect("DATABASE_URL should name a database");
    match query {
        Some(query) => format!("{server}/{database}?{query}"),
        None => format!("{server}/{database}"),
    }
}
