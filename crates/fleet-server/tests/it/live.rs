// SPDX-License-Identifier: AGPL-3.0-only
//! REST snapshots, the live hub, plans, OpenAPI and the liveness sweeps.

use std::time::Duration;

use futures_util::SinkExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::support::{Agent, Harness, VIEWER, get, live, live_next, post_json};

async fn attempt_of(h: &Harness) -> String {
    let (status, body) = get(h.addr(), "/api/v1/projects/app/attempts", Some(VIEWER)).await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    v[0]["id"].as_str().unwrap().to_owned()
}

async fn snapshot(h: &Harness, attempt: &str) -> Value {
    let (status, body) = get(
        h.addr(),
        &format!("/api/v1/projects/app/attempts/{attempt}"),
        Some(VIEWER),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn reads_need_a_viewer_and_agent_keys_are_write_only() {
    let h = Harness::start().await;
    assert_eq!(get(h.addr(), "/api/v1/projects", None).await.0, 401);
    assert_eq!(
        get(h.addr(), "/api/v1/projects", Some(&h.key)).await.0,
        401,
        "agent keys can't read"
    );
    let (status, body) = get(h.addr(), "/api/v1/projects", Some(VIEWER)).await;
    assert_eq!(status, 200);
    assert!(body.contains("\"slug\":\"app\""), "{body}");
    assert_eq!(
        get(h.addr(), "/api/v1/projects/nope/attempts", Some(VIEWER))
            .await
            .0,
        404
    );
    assert!(
        live(h.addr(), Some(&h.key), None).await.is_err(),
        "nor watch live"
    );
    assert!(
        live(h.addr(), Some(VIEWER), Some("https://evil.example"))
            .await
            .is_err(),
        "cross-site sockets are refused"
    );
    let (status, body) = get(h.addr(), "/api/v1/openapi.json", None).await;
    assert_eq!(status, 200);
    let doc: Value = serde_json::from_str(&body).unwrap();
    for path in [
        "/api/v1/projects",
        "/api/v1/projects/{project}/attempts/{attempt}",
        "/api/v1/plans",
    ] {
        assert!(doc["paths"].get(path).is_some(), "{path} documented");
    }
    assert!(doc["components"]["schemas"]["FlowView"]["properties"]["finality"].is_object());
}

#[tokio::test]
async fn a_subscriber_gets_a_snapshot_then_every_version_in_order() {
    let h = Harness::start().await;
    let mut agent = Agent::new("10", "e2e");
    let events = agent.suite(&[("login", true), ("pay", false)]);
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    let attempt = attempt_of(&h).await;

    let mut ws = live(h.addr(), Some(VIEWER), None).await.unwrap();
    ws.send(Message::text(
        json!({"op": "subscribe", "project": "app", "attempt": attempt}).to_string(),
    ))
    .await
    .unwrap();
    let snap = live_next(&mut ws).await;
    assert_eq!(snap["op"], "snapshot");
    let mut version = snap["snapshot"]["attempt"]["version"].as_u64().unwrap();
    assert_eq!(live_next(&mut ws).await["op"], "subscribed");

    // One batch per event: many commits, each a version.
    for e in events {
        c.events(vec![e]).await;
    }
    let mut flows = std::collections::BTreeMap::new();
    let mut job = Value::Null;
    while job["lifecycle"] != "closed" {
        let m = live_next(&mut ws).await;
        if m["op"] != "delta" {
            continue;
        }
        let v = m["version"].as_u64().unwrap();
        assert_eq!(v, version + 1, "contiguous versions");
        version = v;
        for e in m["entities"].as_array().unwrap() {
            match e["kind"].as_str().unwrap() {
                "flow" => {
                    flows.insert(
                        e["entity"]["name"].as_str().unwrap().to_owned(),
                        e["entity"].clone(),
                    );
                }
                "job" => job = e["entity"].clone(),
                _ => {}
            }
        }
    }
    assert_eq!(job["outcome"], "failed");
    assert_eq!(job["outcome_source"], "process_exit");
    // Live result, the suite closed without a report: unverified, never silently final (I4).
    assert_eq!(flows["login"]["state"], "passed");
    assert_eq!(flows["login"]["finality"], "unverified_final");
    assert_eq!(flows["pay"]["state"], "failed");

    // The REST snapshot agrees with what the deltas built.
    let snap = snapshot(&h, &attempt).await;
    assert_eq!(snap["attempt"]["version"].as_u64().unwrap(), version);
    for f in snap["flows"].as_array().unwrap() {
        let live = &flows[f["name"].as_str().unwrap()];
        assert_eq!(
            (&f["state"], &f["finality"], &f["rev"]),
            (&live["state"], &live["finality"], &live["rev"])
        );
    }
    assert_eq!(
        snap["agent_runs"][0]["liveness"], "completed",
        "the run said run.ended"
    );
}

#[tokio::test]
async fn a_superseded_socket_closing_does_not_disconnect_the_active_connection() {
    let h = Harness::start().await;
    let agent = Agent::new("superseded", "e2e");
    let mut old = h.connect(None).await;
    old.hello(&agent, Some(1)).await;
    let attempt = attempt_of(&h).await;
    let mut new = h.connect(None).await;
    new.hello(&agent, Some(1)).await;

    drop(old);
    // The old socket's close is processed asynchronously. Observe the public snapshot over
    // several liveness sweeps while the replacement stays connected and otherwise silent.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        let snap = snapshot(&h, &attempt).await;
        assert_eq!(snap["agent_runs"][0]["liveness"], "online");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    drop(new);
}

#[tokio::test]
async fn after_a_restart_the_hub_rebuilds_from_postgres() {
    let mut h = Harness::start().await;
    let mut agent = Agent::new("11", "e2e");
    let events = agent.suite(&[("login", true)]);
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    c.events(events[..5].to_vec()).await;
    let attempt = attempt_of(&h).await;
    let before = snapshot(&h, &attempt).await;

    h.restart().await;
    let mut after = snapshot(&h, &attempt).await;
    // Liveness is the one thing a restart changes: the run shows stale, not disconnected.
    assert_eq!(after["agent_runs"][0]["liveness"], "stale");
    after["agent_runs"][0]["liveness"] = before["agent_runs"][0]["liveness"].clone();
    after["agent_runs"][0]["state"] = before["agent_runs"][0]["state"].clone();
    after["agent_runs"][0]["rev"] = before["agent_runs"][0]["rev"].clone();
    after["agent_runs"][0]["last_frame_at"] = before["agent_runs"][0]["last_frame_at"].clone();
    let (mut was, mut now) = (before.clone(), after.clone());
    was["attempt"]["version"] = Value::Null;
    now["attempt"]["version"] = Value::Null;
    assert_eq!(now, was, "same state after the restart");

    // Versions continue: a subscriber sees the next delta right after the snapshot's version.
    let mut ws = live(h.addr(), Some(VIEWER), None).await.unwrap();
    ws.send(Message::text(
        json!({"op": "subscribe", "project": "app", "attempt": attempt}).to_string(),
    ))
    .await
    .unwrap();
    let snap = live_next(&mut ws).await;
    let version = snap["snapshot"]["attempt"]["version"].as_u64().unwrap();
    assert!(version >= before["attempt"]["version"].as_u64().unwrap());
    live_next(&mut ws).await;
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    c.events(events[5..].to_vec()).await;
    loop {
        let m = live_next(&mut ws).await;
        if m["op"] == "delta" {
            assert_eq!(m["version"].as_u64().unwrap(), version + 1);
            break;
        }
    }
}

#[tokio::test]
async fn plans_give_planned_totals_before_any_job_starts() {
    let h = Harness::start().await;
    let plan = json!({
        "manifest": {"planId": "nightly-42", "totalFlows": 3,
                     "shards": [{"id": 0, "estimatedSeconds": 60, "flows": ["a.yaml", "b.yaml"]},
                                {"id": 1, "flows": ["c.yaml"]}]},
        "pipeline": {"repository": "o/r", "run_id": "12", "run_attempt": 1}
    })
    .to_string();
    assert_eq!(
        post_json(h.addr(), "/api/v1/plans", Some(VIEWER), &plan)
            .await
            .0,
        401,
        "viewers can't publish"
    );
    let (status, body) = post_json(h.addr(), "/api/v1/plans", Some(&h.key), &plan).await;
    assert_eq!(status, 201, "{body}");
    let (status, _) = post_json(h.addr(), "/api/v1/plans", Some(&h.key), &plan).await;
    assert_eq!(status, 200, "idempotent");
    let other = plan.replace("c.yaml", "d.yaml");
    assert_eq!(
        post_json(h.addr(), "/api/v1/plans", Some(&h.key), &other)
            .await
            .0,
        409
    );

    let attempt = attempt_of(&h).await;
    let snap = snapshot(&h, &attempt).await;
    assert_eq!(snap["attempt"]["planned_total"], 3);
    assert_eq!(
        snap["jobs"].as_array().unwrap().len(),
        0,
        "no job has started"
    );
    assert_eq!(snap["plans"][0]["shards"][0]["flows"][1], "b.yaml");
}

#[tokio::test]
async fn an_agent_that_never_returns_is_abandoned_and_its_job_never_green() {
    let h = Harness::start_with(|c| {
        c.liveness.resume_grace_ms = 300;
        c.liveness.sweep_interval_ms = 200;
    })
    .await;
    let mut agent = Agent::new("13", "e2e");
    let events = agent.suite(&[("login", true)]);
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    // Up to the first flow's start; then the runner disappears.
    c.events(events[..4].to_vec()).await;
    drop(c);
    let attempt = attempt_of(&h).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let snap = loop {
        let s = snapshot(&h, &attempt).await;
        if s["agent_runs"][0]["state"] == "abandoned" || tokio::time::Instant::now() > deadline {
            break s;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(snap["agent_runs"][0]["state"], "abandoned");
    assert_eq!(snap["agent_runs"][0]["liveness"], "disconnected");
    assert_eq!(snap["flows"][0]["state"], "unknown");
    assert_eq!(snap["flows"][0]["outcome_source"], "server_inferred");
    assert_eq!(snap["jobs"][0]["outcome"], "interrupted");
}
