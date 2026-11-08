// SPDX-License-Identifier: AGPL-3.0-only
//! Screen D: `GET /api/v1/projects/{project}/flows/{execution_id}`.

use fleet_protocol::{
    event::{FlowOutcome, StepStarted, StepStatus},
    journal::StepRef,
};
use serde_json::{Value, json};

use fleet_core::policy::Role;

use super::rig::{Agent, Rig, rig};

/// The ID of the only flow named `name` in `attempt`'s snapshot.
async fn flow_id(rig: &Rig, attempt: &str, name: &str) -> String {
    let snapshot = rig
        .json(&format!("/api/v1/projects/app/attempts/{attempt}"))
        .await;
    snapshot["flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// A failed `checkout` flow: three steps (the last one failed), a live claim and a final report.
async fn failed_checkout(rig: &Rig, ci_run: &str) -> String {
    let mut agent = Agent::new(ci_run, "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["checkout"]);
    let checkout = agent.flow_started(&shard, "checkout", 0);
    agent.step(&checkout, 0, "Launch app", StepStatus::Completed);
    agent.step(&checkout, 1, "Tap on \"Pay\"", StepStatus::Completed);
    agent.step(&checkout, 2, "Assert visible \"Done\"", StepStatus::Failed);
    agent.flow_ended(
        &checkout,
        FlowOutcome::Failed,
        Some("Element not found: Done"),
    );
    agent.flow_reported(
        &checkout,
        "checkout",
        FlowOutcome::Failed,
        Some("Element not found: Done"),
    );
    agent.suite_finished(&shard, true);
    rig.publish(&mut agent, vec![]).await;
    let attempts = rig.json("/api/v1/projects/app/attempts").await;
    let attempt = attempts
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_key"] == format!("o/r:{ci_run}"))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    flow_id(rig, &attempt, "checkout").await
}

/// `checkout` runs `pay` (a block with one step) and retries an assertion once, passing on the
/// second attempt; its failing first attempt is the flow's failed step (issue #38).
async fn nested_checkout(rig: &Rig, ci_run: &str) -> String {
    let mut agent = Agent::new(ci_run, "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["checkout"]);
    let checkout = agent.flow_started(&shard, "checkout", 0);
    let top = |label: &str| StepStarted {
        description: label.into(),
        depth: None,
        parent: None,
        attempt: None,
    };
    let inside = |parent: u32, attempt: Option<u32>, label: &str| StepStarted {
        description: label.into(),
        depth: Some(1),
        parent: Some(StepRef { index: parent }),
        attempt,
    };
    agent.nested_step(
        &checkout,
        0,
        top("Run subflows/pay.yaml"),
        StepStatus::Completed,
    );
    agent.nested_step(
        &checkout,
        1,
        inside(0, None, "Tap on \"Pay\""),
        StepStatus::Completed,
    );
    agent.nested_step(&checkout, 2, top("Retry 2 times"), StepStatus::Completed);
    agent.nested_step(
        &checkout,
        3,
        inside(2, Some(1), "Assert visible \"Done\""),
        StepStatus::Failed,
    );
    agent.nested_step(
        &checkout,
        4,
        inside(2, Some(2), "Assert visible \"Done\""),
        StepStatus::Completed,
    );
    agent.flow_ended(&checkout, FlowOutcome::Passed, None);
    agent.suite_finished(&shard, true);
    rig.publish(&mut agent, vec![]).await;
    let attempts = rig.json("/api/v1/projects/app/attempts").await;
    let attempt = attempts
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_key"] == format!("o/r:{ci_run}"))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    flow_id(rig, &attempt, "checkout").await
}

#[tokio::test]
async fn the_steps_of_a_flow_come_back_nested_under_the_block_that_ran_them() {
    let rig = rig().await;
    let flow = nested_checkout(&rig, "710").await;
    let view = rig
        .json(&format!("/api/v1/projects/app/flows/{flow}"))
        .await;

    // Top-level steps are the roots; nested ones sit under their block, in the order they ran.
    let roots = view["steps"].as_array().unwrap();
    let shape = |s: &Value| {
        (
            s["index"].as_i64().unwrap(),
            s["depth"].clone(),
            s["attempt"].clone(),
        )
    };
    assert_eq!(
        roots.iter().map(shape).collect::<Vec<_>>(),
        [(0, Value::Null, Value::Null), (2, Value::Null, Value::Null),]
    );
    assert_eq!(roots[0]["children"][0]["index"], 1);
    assert_eq!(roots[0]["children"][0]["depth"], 1);
    assert_eq!(roots[0]["children"][0]["parent_index"], 0);
    let retried: Vec<_> = roots[1]["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["index"].as_i64().unwrap(),
                s["parent_index"].as_i64().unwrap(),
                s["attempt"].as_i64(),
            )
        })
        .collect();
    assert_eq!(retried, [(3, 2, Some(1)), (4, 2, Some(2))]);
    assert!(
        view["failure"].is_null(),
        "the retry passed, so the flow did not fail"
    );
}

#[tokio::test]
async fn a_failed_flow_shows_every_claim_its_steps_and_where_it_went_wrong() {
    let rig = rig().await;
    let flow = failed_checkout(&rig, "700").await;

    let view = rig
        .json(&format!("/api/v1/projects/app/flows/{flow}"))
        .await;

    assert_eq!(view["flow"]["id"], flow.as_str());
    assert_eq!(view["flow"]["name"], "checkout");
    assert_eq!(view["flow"]["path"], "flows/checkout.yaml");
    assert_eq!(view["flow"]["state"], "failed");
    assert_eq!(view["flow"]["outcome_source"], "junit_final");
    assert_eq!(view["flow"]["finality"], "verified");
    assert_eq!(view["flow"]["has_conflict"], false);

    // All the evidence, not just the winner (I4).
    let claims = view["claims"].as_array().unwrap();
    let seen: Vec<(&str, &str, bool)> = claims
        .iter()
        .map(|c| {
            (
                c["source"].as_str().unwrap(),
                c["outcome"].as_str().unwrap(),
                c["winning"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("live_console", "failed", false),
            ("junit_final", "failed", true)
        ],
        "in arrival order; the final report outranks the live console"
    );
    assert!(claims[0]["recorded_at"].is_string());

    let steps: Vec<(i64, &str, &str)> = view["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["index"].as_i64().unwrap(),
                s["label"].as_str().unwrap(),
                s["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        steps,
        [
            (0, "Launch app", "completed"),
            (1, "Tap on \"Pay\"", "completed"),
            (2, "Assert visible \"Done\"", "failed"),
        ]
    );
    assert_eq!(view["steps"][0]["source"], "debug_log");
    assert!(view["steps"][0]["started_at"].is_string() && view["steps"][0]["ended_at"].is_string());

    // The failure names its fingerprint, and that fingerprint is untriaged until a maintainer acts
    // (issue #84).
    let fingerprint = view["failure"]["fingerprint"].as_str().unwrap();
    assert!(fingerprint.starts_with("v1:"), "{fingerprint}");
    assert_eq!(
        view["failure"],
        json!({
            "message": "Element not found: Done",
            "failed_step_index": 2,
            "last_successful_step_index": 1,
            "fingerprint": fingerprint,
            "triage": {
                "fingerprint": fingerprint,
                "state": null,
                "issue_url": null,
                "first_seen_at": view["failure"]["triage"]["first_seen_at"],
                "updated_at": null
            }
        })
    );
    assert!(view["failure"]["triage"]["first_seen_at"].is_string());
    assert_eq!(view["artifacts"], json!([]), "artifacts arrive with M5");
}

#[tokio::test]
async fn a_flow_names_its_ci_run_job_suite_agent_and_device() {
    let rig = rig().await;
    let flow = failed_checkout(&rig, "710").await;

    let view = rig
        .json(&format!("/api/v1/projects/app/flows/{flow}"))
        .await;

    assert_eq!(view["attempt"]["run_key"], "o/r:710");
    assert_eq!(view["job"]["job_key"], "build");
    assert_eq!(view["suite"]["id"], view["flow"]["suite_execution_id"]);
    assert_eq!(view["agent_run"]["runner_name"], "runner-a");
    assert_eq!(view["agent_run"]["liveness"], "disconnected");
    assert_eq!(view["device"]["device"]["serial"], "emulator-5554");
    assert_eq!(
        view["device"]["device"]["id"],
        view["flow"]["device_session_id"]
    );
    assert!(view["started_at"].is_string() && view["ended_at"].is_string());
}

#[tokio::test]
async fn a_passed_flow_has_no_failure_and_links_earlier_executions_of_the_same_flow() {
    let rig = rig().await;
    let earlier = failed_checkout(&rig, "720").await;
    let mut agent = Agent::new("721", "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["checkout"]);
    let checkout = agent.flow_started(&shard, "checkout", 0);
    agent.step(&checkout, 0, "Launch app", StepStatus::Completed);
    agent.flow_ended(&checkout, FlowOutcome::Passed, None);
    rig.publish(&mut agent, vec![]).await;
    let attempts = rig.json("/api/v1/projects/app/attempts").await;
    let latest_attempt = attempts[0]["id"].as_str().unwrap();
    let flow = flow_id(&rig, latest_attempt, "checkout").await;

    let view = rig
        .json(&format!("/api/v1/projects/app/flows/{flow}"))
        .await;

    assert_eq!(view["flow"]["state"], "passed");
    assert_eq!(view["flow"]["outcome_source"], "live_console");
    assert_eq!(view["failure"], json!(null));
    let previous = view["previous"].as_array().unwrap();
    assert_eq!(previous.len(), 1);
    assert_eq!(previous[0]["flow_id"], earlier.as_str());
    assert_eq!(previous[0]["state"], "failed");
    assert_eq!(previous[0]["outcome_source"], "junit_final");
    assert!(previous[0]["pipeline_attempt_id"].is_string());
}

#[tokio::test]
async fn a_flow_without_steps_or_claims_still_reads() {
    let rig = rig().await;
    let mut agent = Agent::new("730", "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["login"]);
    agent.flow_started(&shard, "login", 0);
    rig.publish(&mut agent, vec![]).await;
    let attempt = rig.attempt_id("app").await;
    let flow = flow_id(&rig, &attempt, "login").await;

    let view = rig
        .json(&format!("/api/v1/projects/app/flows/{flow}"))
        .await;

    assert_eq!(view["flow"]["state"], "running");
    assert_eq!(view["claims"], json!([]), "no evidence yet, none invented");
    assert_eq!(view["steps"], json!([]));
    assert_eq!(view["failure"], json!(null));
    assert_eq!(view["previous"], json!([]));
    assert_eq!(view["ended_at"], json!(null));
}

#[tokio::test]
async fn a_flow_of_another_project_or_that_does_not_exist_is_missing() {
    let rig = rig().await;
    let flow = failed_checkout(&rig, "740").await;

    let elsewhere = rig
        .get(&format!("/api/v1/projects/other/flows/{flow}"))
        .await;
    let nowhere = rig
        .get(&format!(
            "/api/v1/projects/app/flows/{}",
            uuid::Uuid::now_v7()
        ))
        .await;

    for res in [elsewhere, nowhere] {
        assert_eq!(res.status_code(), 404);
        assert_eq!(res.json::<Value>(), json!({"error": "no such flow"}));
    }
}

/// Screen D by flow ID alone (the job summary's link): a redirect that names the project to a
/// reader of it, and the same missing answer for a stranger and for a flow that does not exist.
#[tokio::test]
async fn a_flow_known_only_by_its_id_redirects_to_screen_d_for_a_reader_of_its_project() {
    let rig = rig().await;
    let flow = failed_checkout(&rig, "741").await;
    let page = format!("/api/v1/flows/{flow}/page");

    let res = rig.get(&page).await;
    assert_eq!(res.status_code(), 303, "{}", res.text());
    assert_eq!(
        res.header("location").to_str().unwrap(),
        format!("/flows/{flow}?project=app")
    );

    let foreign = rig.get_as("other", Role::Admin, &page).await;
    let nowhere = rig
        .get(&format!("/api/v1/flows/{}/page", uuid::Uuid::now_v7()))
        .await;
    for res in [foreign, nowhere] {
        assert_eq!(res.status_code(), 404);
        assert_eq!(res.json::<Value>(), json!({"error": "no such flow"}));
    }
    let anonymous = rig.app.get(&page).await;
    assert_eq!(
        anonymous.status_code(),
        401,
        "a credential is still required"
    );
}
