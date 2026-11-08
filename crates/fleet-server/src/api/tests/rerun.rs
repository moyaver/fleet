// SPDX-License-Identifier: AGPL-3.0-only
//! Rerunning failed flows (issue #82): the attempt's rerun manifest reruns exactly its failed flows
//! and parses with the type `fleet-agent run --plan` reads; a flow's screen D and the history carry
//! the copyable command built from its run's words, and say why when there is none.

use fleet_protocol::{
    event::{FlowOutcome, Reproduction},
    plan::PlanManifest,
};
use serde_json::{Value, json};

use super::rig::{Agent, rig};

fn words(words: &[&str], left_out: &[&str]) -> Reproduction {
    Reproduction {
        words: words.iter().map(|w| (*w).to_owned()).collect(),
        left_out: left_out.iter().map(|w| (*w).to_owned()).collect(),
        unavailable: None,
    }
}

/// The flow id of `name` in the attempt's snapshot.
fn flow_id(snapshot: &Value, name: &str) -> String {
    snapshot["flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .unwrap_or_else(|| panic!("no flow {name} in {snapshot}"))["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario, read top to bottom: the manifest, then each screen"
)]
async fn the_rerun_manifest_has_exactly_the_failed_flows_and_the_commands_follow_their_runs() {
    let rig = rig().await;

    // Run A: an Android job with an env flag and an unknown secret flag. Login passes.
    let mut a = Agent::new("950", "build", "runner-a");
    a.started_with(
        &[
            "maestro",
            "--device",
            "emulator-5554",
            "test",
            "--api-key",
            "[REDACTED]",
            "-e",
            "PASSWORD=[REDACTED]",
            "flows/login.yaml",
        ],
        Some(words(
            &[
                "maestro",
                "--device",
                "emulator-5554",
                "test",
                "-e",
                "PASSWORD=[REDACTED]",
            ],
            &["--api-key"],
        )),
    );
    let shard_a = a.shard("emulator-5554", &["login", "pay", "checkout"]);
    let login = a.flow_started(&shard_a, "login", 0);
    a.flow_ended(&login, FlowOutcome::Passed, None);
    let pay = a.flow_started(&shard_a, "pay", 1);
    a.flow_ended(&pay, FlowOutcome::Failed, Some("Element not found"));
    let checkout = a.flow_started(&shard_a, "checkout", 2);
    a.flow_ended(&checkout, FlowOutcome::Failed, Some("Timed out"));

    // Run B: a second job of the same pipeline run, one failed flow, a shard flag that must not carry.
    let mut b = Agent::new("950", "ios", "runner-b");
    b.started_with(
        &[
            "maestro",
            "test",
            "-e",
            "TOKEN=[REDACTED]",
            "--shard-split",
            "2",
            "flows/signup.yaml",
        ],
        Some(words(&["maestro", "test", "-e", "TOKEN=[REDACTED]"], &[])),
    );
    let shard_b = b.shard("iphone", &["signup"]);
    let signup = b.flow_started(&shard_b, "signup", 0);
    b.flow_ended(&signup, FlowOutcome::Failed, Some("Crash"));

    // Run C: an agent from before the reproduction words; its one flow passes.
    let mut c = Agent::new("950", "smoke", "runner-c");
    c.started();
    let shard_c = c.shard("emulator-5560", &["smoke"]);
    let smoke = c.flow_started(&shard_c, "smoke", 0);
    c.flow_ended(&smoke, FlowOutcome::Passed, None);

    rig.publish(&mut a, vec![]).await;
    rig.publish(&mut b, vec![]).await;
    rig.publish(&mut c, vec![]).await;
    let attempt = rig.attempt_id("app").await;
    let snapshot = rig
        .json(&format!("/api/v1/projects/app/attempts/{attempt}"))
        .await;

    // The manifest: one shard per run with a failed flow, exactly those flows, in run order.
    let path = format!("/api/v1/projects/app/attempts/{attempt}/failed-flows");
    let res = rig.get(&path).await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    let disposition = res.header("content-disposition");
    let disposition = disposition.to_str().unwrap();
    assert!(disposition.starts_with("attachment;"), "{disposition}");
    assert!(
        disposition.contains(&format!("fleet-rerun-{attempt}.json")),
        "{disposition}"
    );
    let manifest: PlanManifest = serde_json::from_str(&res.text()).unwrap();
    assert_eq!(manifest.plan_id, format!("rerun-{attempt}"));
    assert_eq!(manifest.total_flows, Some(3));
    assert_eq!(manifest.planned_total(), 3);
    assert_eq!(
        manifest.config_path, None,
        "no config is pinned without its digest"
    );
    let shards: Vec<(u32, Vec<&str>)> = manifest
        .shards
        .iter()
        .map(|s| (s.id, s.flows.iter().map(String::as_str).collect()))
        .collect();
    assert_eq!(
        shards,
        vec![
            (0, vec!["flows/pay.yaml", "flows/checkout.yaml"]),
            (1, vec!["flows/signup.yaml"]),
        ],
        "the passing login and the smoke run are not in it"
    );

    // Screen D: the command is the run's words with the flow's own path last.
    let pay_view = rig
        .json(&format!(
            "/api/v1/projects/app/flows/{}",
            flow_id(&snapshot, "pay")
        ))
        .await;
    assert_eq!(
        pay_view["reproduction"]["command"],
        "maestro --device emulator-5554 test -e 'PASSWORD=[REDACTED]' flows/pay.yaml"
    );
    assert_eq!(pay_view["reproduction"]["left_out"], json!(["--api-key"]));
    assert_eq!(pay_view["reproduction"]["unavailable"], Value::Null);
    let signup_view = rig
        .json(&format!(
            "/api/v1/projects/app/flows/{}",
            flow_id(&snapshot, "signup")
        ))
        .await;
    assert_eq!(
        signup_view["reproduction"]["command"],
        "maestro test -e 'TOKEN=[REDACTED]' flows/signup.yaml"
    );

    // A run that recorded no words gives no command, and says so.
    let smoke_view = rig
        .json(&format!(
            "/api/v1/projects/app/flows/{}",
            flow_id(&snapshot, "smoke")
        ))
        .await;
    assert_eq!(smoke_view["reproduction"]["command"], Value::Null);
    assert!(
        smoke_view["reproduction"]["unavailable"]
            .as_str()
            .unwrap()
            .contains("did not record"),
        "{smoke_view}"
    );

    // Screen E: the same commands on the history rows.
    let history = rig.json("/api/v1/projects/app/history").await;
    let row = |id: &str| {
        history["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["flow_id"] == id)
            .unwrap_or_else(|| panic!("no history row {id}: {history}"))
            .clone()
    };
    assert_eq!(
        row(&flow_id(&snapshot, "pay"))["reproduction"]["command"],
        "maestro --device emulator-5554 test -e 'PASSWORD=[REDACTED]' flows/pay.yaml"
    );
    assert_eq!(
        row(&flow_id(&snapshot, "smoke"))["reproduction"]["command"],
        Value::Null
    );
}

#[tokio::test]
async fn an_attempt_with_no_failed_flow_has_no_manifest_to_download() {
    let rig = rig().await;
    let mut agent = Agent::new("960", "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["login"]);
    let login = agent.flow_started(&shard, "login", 0);
    agent.flow_ended(&login, FlowOutcome::Passed, None);
    rig.publish(&mut agent, vec![]).await;
    let attempt = rig.attempt_id("app").await;

    let res = rig
        .get(&format!(
            "/api/v1/projects/app/attempts/{attempt}/failed-flows"
        ))
        .await;
    assert_eq!(res.status_code(), 404, "{}", res.text());
    assert_eq!(
        res.json::<Value>()["error"],
        "this attempt has no failed flows to rerun"
    );
}

#[tokio::test]
async fn a_failed_flow_without_a_path_refuses_the_manifest_by_name() {
    let rig = rig().await;
    let mut agent = Agent::new("961", "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["login", "ghost"]);
    let login = agent.flow_started(&shard, "login", 0);
    agent.flow_ended(&login, FlowOutcome::Failed, Some("boom"));
    agent.flow_ended_without_path(&shard, "ghost");
    rig.publish(&mut agent, vec![]).await;
    let attempt = rig.attempt_id("app").await;

    let res = rig
        .get(&format!(
            "/api/v1/projects/app/attempts/{attempt}/failed-flows"
        ))
        .await;
    assert_eq!(res.status_code(), 409, "{}", res.text());
    let error = res.json::<Value>()["error"].as_str().unwrap().to_owned();
    assert!(
        error.contains("1 failed flow(s)") && error.contains("ghost"),
        "{error}"
    );
}

#[tokio::test]
async fn the_manifest_of_an_unknown_attempt_is_missing() {
    let rig = rig().await;
    let res = rig
        .get(&format!(
            "/api/v1/projects/app/attempts/{}/failed-flows",
            uuid::Uuid::now_v7()
        ))
        .await;
    assert_eq!(res.status_code(), 404);
    assert_eq!(res.json::<Value>()["error"], "no such attempt");
}
