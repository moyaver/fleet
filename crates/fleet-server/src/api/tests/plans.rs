// SPDX-License-Identifier: AGPL-3.0-only
//! Screen B's plan table: `GET /api/v1/projects/{project}/attempts/{attempt}/plans` is the
//! attempt's plans alone, each shard planned against its actual span, without the rest of the
//! snapshot.

use fleet_protocol::event::FlowOutcome;
use serde_json::{Value, json};

use super::rig::{Agent, rig};

#[tokio::test]
async fn an_attempts_plans_give_each_shards_planned_time_and_its_actual_span() {
    let rig = rig().await;
    let plan = json!({
        "manifest": {"planId": "nightly-7", "totalFlows": 2, "equalCountEstimatedSeconds": 90,
                     "shards": [{"id": 1, "estimatedSeconds": 60,
                                 "flows": ["login.yaml", "pay.yaml"]}]},
        "pipeline": {"repository": "o/r", "run_id": "700", "run_attempt": 1}
    });
    let stored = rig
        .app
        .post("/api/v1/plans")
        .authorization_bearer(&rig.key)
        .json(&plan)
        .await;
    assert_eq!(stored.status_code(), 201, "{}", stored.text());

    let mut agent = Agent::new("700", "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["login", "pay"]);
    agent.plan_assigned("nightly-7", 1);
    let login = agent.flow_started(&shard, "login", 0);
    agent.flow_ended(&login, FlowOutcome::Passed, None);
    let pay = agent.flow_started(&shard, "pay", 1);
    rig.publish(&mut agent, vec![]).await;
    let attempt = rig.attempt_id("app").await;
    let path = format!("/api/v1/projects/app/attempts/{attempt}/plans");

    let plans = rig.json(&path).await;
    assert_eq!(plans.as_array().unwrap().len(), 1, "{plans}");
    let plan = &plans[0];
    assert_eq!(plan["plan_id"], "nightly-7");
    assert_eq!(plan["planned_total"], 2);
    assert_eq!(plan["equal_count_estimate_ms"], 90_000);
    let shard = &plan["shards"][0];
    assert_eq!(shard["shard_id"], 1);
    assert_eq!(shard["flows"], json!(["login.yaml", "pay.yaml"]));
    assert_eq!(shard["estimate_ms"], 60_000);
    assert_eq!(shard["agent_runs"], json!([agent.id]));
    assert_eq!(shard["executed"], 2);
    assert_eq!(shard["actual_ms"], Value::Null, "pay has not ended");

    agent.flow_ended(&pay, FlowOutcome::Passed, None);
    rig.publish(&mut agent, vec![]).await;
    let plans = rig.json(&path).await;
    assert!(
        plans[0]["shards"][0]["actual_ms"].is_number(),
        "every flow of the shard has ended: {plans}"
    );

    let doc: Value = rig.app.get("/api/v1/openapi.json").await.json();
    let get = &doc["paths"]["/api/v1/projects/{project}/attempts/{attempt}/plans"]["get"];
    assert_eq!(
        get["responses"]["200"]["content"]["application/json"]["schema"]["items"]["$ref"],
        "#/components/schemas/PlanView",
        "the route is documented"
    );
}

#[tokio::test]
async fn the_plans_of_an_unknown_attempt_are_missing() {
    let rig = rig().await;

    let res = rig
        .get(&format!(
            "/api/v1/projects/app/attempts/{}/plans",
            uuid::Uuid::now_v7()
        ))
        .await;

    assert_eq!(res.status_code(), 404);
    assert_eq!(res.json::<Value>(), json!({"error": "no such attempt"}));
}
