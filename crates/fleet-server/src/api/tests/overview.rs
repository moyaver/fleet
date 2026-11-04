// SPDX-License-Identifier: AGPL-3.0-only
//! Screen A: `GET /api/v1/projects/{project}/overview`.

use fleet_protocol::event::{FlowOutcome, StepStatus};
use serde_json::json;

use super::rig::{Agent, rig};

#[tokio::test]
async fn an_attempt_with_an_open_job_is_active_with_its_job_and_flow_counts() {
    let rig = rig().await;
    let mut agent = Agent::new("100", "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["login", "pay"]);
    let login = agent.flow_started(&shard, "login", 0);
    agent.step(&login, 0, "Tap on \"Sign in\"", StepStatus::Completed);
    agent.flow_ended(&login, FlowOutcome::Passed, None);
    agent.flow_started(&shard, "pay", 1);
    rig.publish(&mut agent, vec![]).await;

    let overview = rig.json("/api/v1/projects/app/overview").await;

    let active = overview["active"].as_array().unwrap();
    assert_eq!(active.len(), 1, "{overview}");
    assert_eq!(overview["active_total"], 1);
    assert_eq!(active[0]["attempt"]["run_key"], "o/r:100");
    assert_eq!(active[0]["jobs"]["running"], 1);
    assert_eq!(active[0]["jobs"]["closed"], 0);
    assert_eq!(active[0]["flows"]["passed"], 1);
    assert_eq!(active[0]["flows"]["running"], 1);
    assert_eq!(active[0]["flows"]["failed"], 0);
    assert_eq!(overview["recent"], json!([]));
}

/// One finished run: a single passing flow, a clean exit, then `run.ended`.
async fn finished_run(rig: &super::rig::Rig, ci_run: &str, runner: &str) {
    let mut agent = Agent::new(ci_run, "build", runner);
    agent.started();
    let shard = agent.shard("emulator-5554", &["login"]);
    let login = agent.flow_started(&shard, "login", 0);
    agent.flow_ended(&login, FlowOutcome::Passed, None);
    agent.suite_finished(&shard, false);
    agent.ended();
    rig.publish(&mut agent, vec![]).await;
}

#[tokio::test]
async fn finished_attempts_list_their_jobs_with_outcome_and_source_newest_first_and_bounded() {
    let rig = rig().await;
    for run in ["200", "201", "202"] {
        finished_run(&rig, run, "runner-a").await;
    }

    let overview = rig.json("/api/v1/projects/app/overview?recent=2").await;

    assert_eq!(overview["active"], json!([]));
    assert_eq!(overview["active_total"], 0);
    let recent = overview["recent"].as_array().unwrap();
    let keys: Vec<&str> = recent
        .iter()
        .map(|r| r["attempt"]["run_key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["o/r:202", "o/r:201"], "newest first, cut at 2");
    let job = &recent[0]["jobs"][0];
    assert_eq!(job["job_key"], "build");
    assert_eq!(job["lifecycle"], "closed");
    assert_eq!(job["outcome"], "completed");
    assert!(
        job["outcome_source"].is_string(),
        "an outcome always names its source (I4): {job}"
    );
    assert_eq!(recent[0]["flows"]["passed"], 1);
    assert!(recent[0]["finished_at"].is_string());
}

#[tokio::test]
async fn a_project_without_runs_has_an_empty_overview() {
    let rig = rig().await;

    let overview = rig.json("/api/v1/projects/app/overview").await;

    assert_eq!(overview["active"], json!([]));
    assert_eq!(overview["active_total"], 0);
    assert_eq!(overview["recent"], json!([]));
    assert_eq!(overview["workers"], json!([]));
    assert_eq!(
        overview["worker_counts"],
        json!({"online": 0, "stale": 0, "disconnected": 0, "completed": 0})
    );
}
