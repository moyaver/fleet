// SPDX-License-Identifier: AGPL-3.0-only
//! The M3 fixture exit criterion: the real agent streams a captured Maestro run into the real
//! server, and the outcomes the server stores are the ones `fleet-agent inspect` computes for
//! that fixture. Also spec acceptance criteria 3 (jobs and emulators stay separate), 11 (the exit
//! code is preserved) and 15 (failed and interrupted jobs never read as successful).

use std::process::Command;

use serde_json::{Value, json};

use crate::support::{Stack, agent_bin, fixture, flows, run_agent};

/// Fixtures with a Maestro session to replay (the fake Maestro needs `run/debug`).
const FIXTURES: &[&str] = &[
    "pass",
    "fail",
    "suite",
    "shard-split-2",
    "shard-all-2",
    "execution-order",
    "device-killed",
    "retry-fail",
    "features",
    "sigterm",
    "sigint",
    "pass-machine-log",
];

/// `fleet-agent inspect --json` flows as (name, outcome, finality, source), sorted.
fn inspected(name: &str) -> Vec<(String, String, String, String)> {
    let out = Command::new(agent_bin())
        .args(["inspect", "--json"])
        .arg(fixture(name))
        .output()
        .unwrap();
    assert!(out.status.success(), "inspect {name}");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut f: Vec<_> = v["flows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["name"].as_str().unwrap_or_default().to_owned(),
                f["outcome"].as_str().unwrap_or_default().to_owned(),
                f["finality"].as_str().unwrap_or_default().to_owned(),
                f["source"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    f.sort();
    f
}

fn exit_code(name: &str) -> Option<i32> {
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture(name).join("exit.json")).unwrap())
            .unwrap();
    v["code"].as_i64().and_then(|c| i32::try_from(c).ok())
}

#[tokio::test(flavor = "multi_thread")]
async fn stored_outcomes_match_inspect_for_every_fixture() {
    let stack = Stack::start(|_| {}).await;
    for name in FIXTURES {
        let (ws, key) = (stack.ws_url(), stack.key.clone());
        let fixture_name = (*name).to_owned();
        let job = format!("job-{name}");
        let out = tokio::task::spawn_blocking(move || {
            run_agent(
                &fixture_name,
                &[
                    ("FLEET_SERVER", &ws),
                    ("FLEET_TOKEN", &key),
                    ("FLEET_RUN_ID", "fixtures"),
                    ("FLEET_REPOSITORY", "acme/app"),
                    ("FLEET_JOB_KEY", &job),
                ],
            )
        })
        .await
        .unwrap();
        assert_eq!(
            out.status.code(),
            exit_code(name),
            "{name}: the agent exits with Maestro's code (criterion 11)"
        );
        let snap = stack.latest_snapshot().await;
        let job = snap["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["job_key"] == format!("job-{name}"))
            .unwrap_or_else(|| panic!("{name}: its job"))
            .clone();
        let run = snap["agent_runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["job_id"] == job["id"])
            .unwrap()
            .clone();
        let mine = json!({
            "flows": snap["flows"].as_array().unwrap().iter()
                .filter(|f| f["job_id"] == job["id"]).cloned().collect::<Vec<_>>()
        });
        let expected = inspected(name);
        assert_ne!(expected, vec![], "{name}: the fixture has flows");
        assert_eq!(flows(&mine), expected, "{name}: server = inspect");
        assert_eq!(run["state"], "ended", "{name}");
        assert_eq!(run["gaps"], 0, "{name}: gap-free (I5)");
        // Criterion 15: only a clean exit can complete a job.
        assert_eq!(job["lifecycle"], "closed", "{name}");
        if exit_code(name) == Some(0) {
            assert_eq!(job["outcome"], "completed", "{name}");
        } else {
            assert_ne!(
                job["outcome"], "completed",
                "{name}: never falsely successful"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_jobs_and_same_serial_emulators_stay_separate() {
    // Criterion 3: two CI jobs at once, one on two emulators and one on one, sharing a serial
    // (emulator-5600 appears in both fixtures). Nothing crosses over.
    let stack = Stack::start(|_| {}).await;
    let mut handles = Vec::new();
    for (name, job) in [("shard-split-2", "e2e-sharded"), ("suite", "e2e-single")] {
        let (ws, key) = (stack.ws_url(), stack.key.clone());
        handles.push(tokio::task::spawn_blocking(move || {
            run_agent(
                name,
                &[
                    ("FLEET_SERVER", &ws),
                    ("FLEET_TOKEN", &key),
                    ("FLEET_RUN_ID", "parallel"),
                    ("FLEET_REPOSITORY", "acme/app"),
                    ("FLEET_JOB_KEY", job),
                ],
            )
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let snap = stack.latest_snapshot().await;
    let jobs = snap["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2, "two jobs in one attempt");
    for (name, job_key) in [("shard-split-2", "e2e-sharded"), ("suite", "e2e-single")] {
        let job = jobs.iter().find(|j| j["job_key"] == job_key).unwrap();
        let run = snap["agent_runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["job_id"] == job["id"])
            .unwrap();
        let devices: Vec<&Value> = snap["devices"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["agent_run_id"] == run["id"])
            .collect();
        let device_ids: Vec<&Value> = devices.iter().map(|d| &d["id"]).collect();
        let job_flows = json!({
            "flows": snap["flows"].as_array().unwrap().iter()
                .filter(|f| f["job_id"] == job["id"]).cloned().collect::<Vec<_>>()
        });
        assert_eq!(
            flows(&job_flows),
            inspected(name),
            "{name}: only its own flows"
        );
        assert!(
            job_flows["flows"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["device_session_id"].is_null()
                    || device_ids.contains(&&f["device_session_id"])),
            "{name}: flows ran only on this job's devices"
        );
    }
    let serials: Vec<&str> = snap["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["serial"].as_str().unwrap())
        .collect();
    assert_eq!(
        serials.iter().filter(|s| **s == "emulator-5600").count(),
        2,
        "the same serial on two runs is two device sessions, not one: {serials:?}"
    );
}
