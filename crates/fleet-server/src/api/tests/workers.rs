// SPDX-License-Identifier: AGPL-3.0-only
//! Screen C: worker and device detail, and the runner health on screen A.

use fleet_protocol::event::FlowOutcome;
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

use super::rig::{Agent, rig, rig_with, sample, telemetry};

#[tokio::test]
async fn a_worker_is_online_while_its_agent_talks_stale_when_silent_and_disconnected_when_gone() {
    let rig = rig_with(|c| c.liveness.stale_after_ms = 1500).await;
    // Liveness reads the hub's clock. Frozen, the time a request takes cannot make the agent
    // stale; the test moves the clock past the threshold itself.
    rig.state.hub.freeze_clock();
    let mut agent = Agent::new("300", "build", "runner-a");
    let mut socket = rig.connect(&agent).await;
    agent.started();
    let shard = agent.shard("emulator-5554", &["login"]);
    let login = agent.flow_started(&shard, "login", 0);
    agent.flow_ended(&login, FlowOutcome::Passed, None);
    socket.publish(&mut agent).await;

    let overview = rig.json("/api/v1/projects/app/overview").await;
    assert_eq!(overview["worker_counts"]["online"], 1, "{overview}");
    let worker = &overview["workers"][0];
    assert_eq!(worker["runner_name"], "runner-a");
    assert_eq!(worker["os"], "linux");
    assert_eq!(worker["arch"], "x86_64");
    assert_eq!(worker["liveness"], "online");
    assert_eq!(worker["active_runs"], 1);

    // Nothing is heard for longer than the stale threshold: stale, still connected.
    rig.state
        .hub
        .advance_clock(std::time::Duration::from_millis(1600));
    let overview = rig
        .until("/api/v1/projects/app/overview", |o| {
            o["worker_counts"]["stale"] == 1
        })
        .await;
    assert_eq!(overview["worker_counts"]["online"], 0);

    // The socket goes away without a goodbye: disconnected.
    socket.drop_connection().await;
    let overview = rig
        .until("/api/v1/projects/app/overview", |o| {
            o["worker_counts"]["disconnected"] == 1
        })
        .await;
    assert_eq!(overview["workers"][0]["liveness"], "disconnected");
}

/// A finished run of one passing flow on `emulator-5554` for `runner`.
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

async fn worker_id(rig: &super::rig::Rig) -> String {
    let overview = rig.json("/api/v1/projects/app/overview").await;
    overview["workers"][0]["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn a_worker_shows_its_machine_its_recent_runs_and_the_devices_it_hosted() {
    let rig = rig().await;
    let mut agent = Agent::new("500", "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["login"]);
    let login = agent.flow_started(&shard, "login", 0);
    agent.flow_ended(&login, FlowOutcome::Passed, None);
    rig.publish(&mut agent, vec![]).await;
    let worker = worker_id(&rig).await;

    let view = rig
        .json(&format!("/api/v1/projects/app/workers/{worker}"))
        .await;

    assert_eq!(view["worker"]["id"], worker.as_str());
    assert_eq!(view["worker"]["runner_name"], "runner-a");
    assert_eq!(view["worker"]["os"], "linux");
    assert_eq!(view["worker"]["arch"], "x86_64");
    assert_eq!(
        view["worker"]["liveness"], "disconnected",
        "a batch agent is detached between posts"
    );
    let runs = view["recent_runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["run"]["id"], agent.id.to_string());
    assert_eq!(runs[0]["run"]["state"], "detached");
    assert_eq!(runs[0]["run"]["liveness"], "disconnected");
    assert_eq!(runs[0]["job_key"], "build");
    assert!(runs[0]["pipeline_attempt_id"].is_string());
    assert!(runs[0]["started_at"].is_string());
    let devices = view["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["device"]["serial"], "emulator-5554");
    assert_eq!(devices[0]["runner_name"], "runner-a");
    assert_eq!(devices[0]["job_key"], "build");
    assert_eq!(
        devices[0]["profile"],
        json!(null),
        "no device profile was reported: none is invented"
    );
}

#[tokio::test]
async fn a_worker_that_does_not_exist_in_the_project_is_missing() {
    let rig = rig().await;
    finished_run(&rig, "510", "runner-a").await;

    let res = rig
        .get(&format!(
            "/api/v1/projects/app/workers/{}",
            uuid::Uuid::now_v7()
        ))
        .await;

    assert_eq!(res.status_code(), 404);
    assert_eq!(res.json::<Value>(), json!({"error": "no such worker"}));
}

#[tokio::test]
async fn a_workers_run_history_pages_with_a_cursor_newest_first() {
    let rig = rig().await;
    for run in ["1", "2", "3", "4", "5"] {
        finished_run(&rig, run, "runner-a").await;
    }
    let worker = worker_id(&rig).await;
    let base = format!("/api/v1/projects/app/workers/{worker}/runs");
    let keys = |page: &Value| -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["pipeline_attempt_id"].as_str().unwrap().to_owned())
            .collect()
    };

    let first = rig.json(&format!("{base}?limit=2")).await;
    let second = rig
        .json(&format!(
            "{base}?limit=2&cursor={}",
            first["next_cursor"].as_str().unwrap()
        ))
        .await;
    let third = rig
        .json(&format!(
            "{base}?limit=2&cursor={}",
            second["next_cursor"].as_str().unwrap()
        ))
        .await;

    assert_eq!(keys(&first).len(), 2);
    assert_eq!(keys(&second).len(), 2);
    assert_eq!(keys(&third).len(), 1);
    assert_eq!(
        third["next_cursor"],
        json!(null),
        "the last page has no cursor"
    );
    let all: Vec<String> = [&first, &second, &third]
        .into_iter()
        .flat_map(keys)
        .collect();
    let attempts = rig.json("/api/v1/projects/app/attempts").await;
    let newest_first: Vec<String> = attempts
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(all, newest_first, "every run once, newest first");

    let bad = rig.get(&format!("{base}?cursor=not-a-cursor")).await;
    assert_eq!(bad.status_code(), 400);
}

#[tokio::test]
async fn a_device_session_lists_its_flows_running_first_and_how_many_it_hosted() {
    let rig = rig().await;
    let mut agent = Agent::new("520", "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["login", "pay"]);
    let login = agent.flow_started(&shard, "login", 0);
    agent.flow_ended(&login, FlowOutcome::Passed, None);
    agent.flow_started(&shard, "pay", 1);
    rig.publish(&mut agent, vec![]).await;
    let worker = worker_id(&rig).await;
    let worker = rig
        .json(&format!("/api/v1/projects/app/workers/{worker}"))
        .await;
    let device = worker["devices"][0]["device"]["id"].as_str().unwrap();

    let view = rig
        .json(&format!("/api/v1/projects/app/devices/{device}"))
        .await;

    assert_eq!(view["session"]["device"]["serial"], "emulator-5554");
    assert_eq!(view["session"]["runner_name"], "runner-a");
    assert_eq!(view["session"]["worker_id"], worker["worker"]["id"]);
    assert_eq!(view["flow_count"], 2);
    let flows: Vec<(&str, &str)> = view["flows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["name"].as_str().unwrap(), f["state"].as_str().unwrap()))
        .collect();
    assert_eq!(flows, [("pay", "running"), ("login", "passed")]);
    assert_eq!(
        view["flows"][1]["outcome_source"], "live_console",
        "a finished flow names its source (I4)"
    );
}

#[tokio::test]
async fn a_device_that_does_not_exist_in_the_project_is_missing() {
    let rig = rig().await;

    let res = rig
        .get(&format!(
            "/api/v1/projects/app/devices/{}",
            uuid::Uuid::now_v7()
        ))
        .await;

    assert_eq!(res.status_code(), 404);
    assert_eq!(res.json::<Value>(), json!({"error": "no such device"}));
}

#[tokio::test]
async fn runner_health_shows_the_latest_readings_and_when_they_were_taken() {
    let rig = rig().await;
    let t0 = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::minutes(2);
    let mut agent = Agent::new("530", "build", "runner-a");
    let samples = vec![sample(t0, 10.0), sample(t0 + Duration::seconds(5), 20.0)];
    rig.publish(&mut agent, vec![telemetry(1, samples)]).await;

    let overview = rig.json("/api/v1/projects/app/overview").await;

    let latest = &overview["workers"][0]["latest"];
    assert_eq!(
        OffsetDateTime::parse(latest["at"].as_str().unwrap(), &Rfc3339).unwrap(),
        t0 + Duration::seconds(5),
        "the newest sample, so the UI can show how fresh the data is"
    );
    let reading = |metric: &str| -> &Value {
        latest["readings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["metric"] == metric)
            .unwrap()
    };
    let cpu = reading("host_cpu_pct");
    assert_eq!(cpu["label"], "Host CPU");
    assert_eq!(cpu["unit"], "percent");
    assert_eq!(cpu["value"], 20.0);
    assert_eq!(cpu["available"], true);
    let load = reading("host_load_1m");
    assert_eq!(load["value"], json!(null), "unreported is not zero");
    assert_eq!(load["available"], false);
    assert_eq!(load["reason"], "the agent reported no value");
    assert!(
        latest["readings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["scope"] == "host"),
        "process readings belong to a run, not to the worker: {latest}"
    );
    let thermal = reading("host_thermal_c");
    assert_eq!(thermal["available"], false);
    assert_eq!(
        thermal["reason"],
        "the agent does not report this metric yet"
    );
}

#[tokio::test]
async fn a_worker_without_recent_samples_has_no_latest_readings() {
    let rig = rig().await;
    let mut agent = Agent::new("540", "build", "runner-a");
    let old = OffsetDateTime::now_utc() - Duration::hours(3);
    rig.publish(&mut agent, vec![telemetry(1, vec![sample(old, 10.0)])])
        .await;

    let overview = rig.json("/api/v1/projects/app/overview").await;

    assert_eq!(overview["workers"][0]["latest"], json!(null));
    let worker = overview["workers"][0]["id"].as_str().unwrap();
    let view = rig
        .json(&format!("/api/v1/projects/app/workers/{worker}"))
        .await;
    assert_eq!(view["worker"]["latest"], json!(null));
}
