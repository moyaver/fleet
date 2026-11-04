// SPDX-License-Identifier: AGPL-3.0-only
//! Screen B: `GET /api/v1/projects/{project}/attempts/{attempt}/timeline`.

use fleet_protocol::event::FlowOutcome;
use serde_json::{Value, json};

use super::rig::{Agent, rig};

fn bar<'a>(timeline: &'a Value, name: &str) -> &'a Value {
    timeline["bars"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == name)
        .unwrap_or_else(|| panic!("no bar for {name} in {timeline}"))
}

#[tokio::test]
async fn the_timeline_has_a_lane_per_device_and_a_bar_per_started_flow_with_its_provenance() {
    let rig = rig().await;
    let mut agent = Agent::new("400", "build", "runner-a");
    agent.started();
    let first = agent.shard("emulator-5554", &["login", "pay", "refund"]);
    let login = agent.flow_started(&first, "login", 0);
    agent.flow_ended(&login, FlowOutcome::Passed, None);
    let pay = agent.flow_started(&first, "pay", 1);
    agent.flow_ended(&pay, FlowOutcome::Failed, Some("Assertion is false"));
    agent.flow_reported_unstarted(&first, "refund", FlowOutcome::Failed);
    let second = agent.shard("emulator-5556", &["search"]);
    agent.flow_started(&second, "search", 0);
    rig.publish(&mut agent, vec![]).await;
    let attempt = rig.attempt_id("app").await;

    let timeline = rig
        .json(&format!("/api/v1/projects/app/attempts/{attempt}/timeline"))
        .await;

    assert_eq!(timeline["attempt_id"], attempt.as_str());
    let snapshot = rig
        .json(&format!("/api/v1/projects/app/attempts/{attempt}"))
        .await;
    assert_eq!(timeline["version"], snapshot["attempt"]["version"]);
    let serials: Vec<&str> = timeline["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["serial"].as_str().unwrap())
        .collect();
    assert_eq!(serials, ["emulator-5554", "emulator-5556"]);
    assert_eq!(timeline["lanes"][0]["job_key"], "build");
    assert_eq!(timeline["lanes"][0]["runner_name"], "runner-a");

    let login = bar(&timeline, "login");
    assert_eq!(login["lane_id"], timeline["lanes"][0]["id"]);
    assert_eq!(login["state"], "passed");
    assert_eq!(login["outcome_source"], "live_console");
    assert_eq!(login["finality"], "provisional");
    assert_eq!(login["duration_ms"], 1500);
    assert!(login["started_at"].is_string() && login["ended_at"].is_string());
    assert_eq!(login["ordinal"], 0);

    let pay = bar(&timeline, "pay");
    assert_eq!(pay["state"], "failed");
    assert_eq!(pay["outcome_source"], "live_console");

    let search = bar(&timeline, "search");
    assert_eq!(search["lane_id"], timeline["lanes"][1]["id"]);
    assert_eq!(search["state"], "running");
    assert_eq!(
        search["ended_at"],
        json!(null),
        "a running bar has no end yet"
    );
    assert_eq!(
        search["outcome_source"],
        json!(null),
        "no claim yet, none invented"
    );

    assert_eq!(timeline["bars"].as_array().unwrap().len(), 3);
    assert_eq!(
        timeline["not_started"], 1,
        "refund never started: no bar for it"
    );
}

#[tokio::test]
async fn a_timeline_of_an_unknown_attempt_is_missing() {
    let rig = rig().await;

    let res = rig
        .get(&format!(
            "/api/v1/projects/app/attempts/{}/timeline",
            uuid::Uuid::now_v7()
        ))
        .await;

    assert_eq!(res.status_code(), 404);
    assert_eq!(res.json::<Value>(), json!({"error": "no such attempt"}));
}
