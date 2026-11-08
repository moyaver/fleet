// SPDX-License-Identifier: AGPL-3.0-only
//! M3 exit criteria under load (`fleet-loadgen` against the real server): concurrent agents
//! publish without corrupting state; reconnect storms, duplicates and reordering, a server
//! restart mid-run and overlapping job names lose nothing and mix nothing up.

use std::{collections::BTreeMap, time::Duration};

use fleet_loadgen::{Options, Report};
use serde_json::Value;

use crate::support::Stack;

fn opts(stack: &Stack, run_id: &str) -> Options {
    Options {
        server: stack.ws_url(),
        token: stack.key.clone(),
        run_id: run_id.into(),
        deadline: Duration::from_secs(120),
        ..Options::default()
    }
}

async fn attempt(stack: &Stack, run_id: &str) -> Value {
    let attempts = stack.get("/api/v1/projects/app/attempts?limit=500").await;
    let id = attempts
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_key"] == format!("loadgen/app:{run_id}"))
        .expect("the run's attempt")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    stack
        .get(&format!("/api/v1/projects/app/attempts/{id}"))
        .await
}

/// Waits (up to 30 s) until the run's agents have committed at least `at_least` events in total,
/// as the attempt snapshot shows them. Returns early only on that observable state.
async fn await_commits(stack: &Stack, run_id: &str, at_least: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let attempts = stack.get("/api/v1/projects/app/attempts?limit=500").await;
        let found = attempts
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["run_key"] == format!("loadgen/app:{run_id}"))
            .and_then(|a| a["id"].as_str().map(str::to_owned));
        if let Some(id) = found {
            let snap = stack
                .get(&format!("/api/v1/projects/app/attempts/{id}"))
                .await;
            let committed: u64 = snap["agent_runs"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|r| r["committed_seq"].as_u64())
                .sum();
            if committed >= at_least {
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{run_id}: fewer than {at_least} events committed 30 s into the run"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Everything the server holds matches what every agent sent, run by run.
fn verify(report: &Report, snap: &Value, devices: u32, per_device: u32) {
    let runs: BTreeMap<String, &Value> = snap["agent_runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["id"].as_str().unwrap().to_owned(), r))
        .collect();
    assert_eq!(runs.len(), report.agents.len(), "one agent run per agent");
    let jobs: BTreeMap<String, &Value> = snap["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| (j["id"].as_str().unwrap().to_owned(), j))
        .collect();
    let job_keys: std::collections::BTreeSet<&str> =
        report.agents.iter().map(|a| a.job_key.as_str()).collect();
    assert_eq!(
        jobs.len(),
        job_keys.len(),
        "one job per job key, shared by its runs"
    );
    let flows = snap["flows"].as_array().unwrap();
    let devices_of: BTreeMap<&str, Vec<&Value>> =
        snap["devices"]
            .as_array()
            .unwrap()
            .iter()
            .fold(BTreeMap::new(), |mut m, d| {
                m.entry(d["agent_run_id"].as_str().unwrap())
                    .or_insert_with(Vec::new)
                    .push(d);
                m
            });
    let mut failed_by_job: BTreeMap<String, u32> = BTreeMap::new();
    for a in &report.agents {
        let id = a.agent_run_id.to_string();
        let r = runs[&id];
        assert_eq!(a.acked, a.events, "every event acked");
        assert_eq!(
            r["committed_seq"].as_u64(),
            Some(a.events),
            "committed = sent, gap-free"
        );
        assert_eq!(r["gaps"].as_u64(), Some(0), "no recorded gap (I5)");
        assert_eq!(r["state"], "ended");
        assert_eq!(a.rejected, 0);
        let job = jobs[r["job_id"].as_str().unwrap()];
        assert_eq!(
            job["job_key"],
            a.job_key.as_str(),
            "the run sits under its own job key"
        );
        let mine: Vec<&Value> = flows
            .iter()
            .filter(|f| f["agent_run_id"] == id.as_str())
            .collect();
        assert_eq!(
            mine.len(),
            (devices * per_device) as usize,
            "every flow of this run"
        );
        let failed = mine.iter().filter(|f| f["state"] == "failed").count();
        assert_eq!(failed, a.failed_flows as usize);
        assert!(
            mine.iter()
                .all(|f| f["finality"] == "verified" && f["outcome_source"] == "junit_final"),
            "the report decided every flow"
        );
        let devs = &devices_of[id.as_str()];
        assert_eq!(
            devs.len(),
            devices as usize,
            "this run's devices, none borrowed"
        );
        let dev_ids: Vec<&str> = devs.iter().map(|d| d["id"].as_str().unwrap()).collect();
        assert!(
            mine.iter()
                .all(|f| dev_ids.contains(&f["device_session_id"].as_str().unwrap())),
            "flows ran on this run's devices only"
        );
        *failed_by_job
            .entry(job["id"].as_str().unwrap().to_owned())
            .or_default() += a.failed_flows;
    }
    for (id, j) in &jobs {
        let expected = if failed_by_job[id] > 0 {
            "failed"
        } else {
            "completed"
        };
        assert_eq!(
            (j["lifecycle"].as_str(), j["outcome"].as_str()),
            (Some("closed"), Some(expected)),
            "{id}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_agents_with_four_devices_publish_without_corrupting_state() {
    let stack = Stack::start(|c| c.database.max_connections = 30).await;
    let o = Options {
        agents: 50,
        devices: 4,
        flows: 3,
        job_keys: 10,
        duplicates: true,
        ..opts(&stack, "fifty")
    };
    let report = fleet_loadgen::run(o).await.unwrap();
    let snap = attempt(&stack, "fifty").await;
    verify(&report, &snap, 4, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reconnect_storm_loses_nothing() {
    let stack = Stack::start(|_| {}).await;
    let o = Options {
        agents: 20,
        devices: 2,
        flows: 4,
        job_keys: 5,
        batch: 8,
        storm: true,
        ..opts(&stack, "storm")
    };
    let report = fleet_loadgen::run(o).await.unwrap();
    assert!(
        report.agents.iter().all(|a| a.connections >= 2),
        "every agent reconnected"
    );
    verify(&report, &attempt(&stack, "storm").await, 2, 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_restart_mid_run_loses_no_committed_event() {
    let mut stack = Stack::start(|_| {}).await;
    let o = Options {
        agents: 10,
        devices: 2,
        flows: 4,
        job_keys: 3,
        batch: 4,
        pace: Duration::from_millis(40),
        ..opts(&stack, "restart")
    };
    let load = tokio::spawn(fleet_loadgen::run(o));
    // Restart only once events are committed, so the restart lands mid-run, not before the run.
    await_commits(&stack, "restart", 20).await;
    stack.restart().await;
    let report = load.await.unwrap().unwrap();
    assert!(
        report.agents.iter().any(|a| a.connections >= 2),
        "agents had to reconnect"
    );
    verify(&report, &attempt(&stack, "restart").await, 2, 4);
}
