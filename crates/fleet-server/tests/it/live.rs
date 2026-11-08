// SPDX-License-Identifier: AGPL-3.0-only
//! REST snapshots, the live hub, plans, OpenAPI and the liveness sweeps.

use std::{net::SocketAddr, time::Duration};

use fleet_protocol::{
    frame::{ClientFrame, Sample, SampleUnavailable, Telemetry},
    journal::ClockSample,
};
use futures_util::SinkExt;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio_tungstenite::tungstenite::Message;

use crate::support::{Agent, Client, Harness, VIEWER, Ws, get, live, live_next, post_json};

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
async fn a_new_plan_resyncs_a_subscriber_following_the_attempt() {
    let h = Harness::start().await;
    let agent = Agent::new("plan-live", "e2e");
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    let attempt = attempt_of(&h).await;
    let mut ws = live(h.addr(), Some(VIEWER), None).await.unwrap();
    let subscribe = json!({"op": "subscribe", "project": "app", "attempt": attempt});
    ws.send(Message::text(subscribe.to_string())).await.unwrap();
    let before = live_next(&mut ws).await;
    assert_eq!(before["snapshot"]["attempt"]["planned_total"], Value::Null);
    assert_eq!(live_next(&mut ws).await["op"], "subscribed");

    let plan = json!({
        "manifest": {"planId": "live-plan", "totalFlows": 2,
                     "shards": [{"id": 0, "flows": ["a.yaml", "b.yaml"]}]},
        "pipeline": {"repository": "o/r", "run_id": "plan-live", "run_attempt": 1}
    });
    let (status, body) =
        post_json(h.addr(), "/api/v1/plans", Some(&h.key), &plan.to_string()).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(live_next(&mut ws).await["op"], "resync");

    let mut ws = live(h.addr(), Some(VIEWER), None).await.unwrap();
    ws.send(Message::text(subscribe.to_string())).await.unwrap();
    let after = live_next(&mut ws).await;
    assert_eq!(after["snapshot"]["attempt"]["planned_total"], 2);
    assert_eq!(
        after["snapshot"]["plans"][0]["shards"][0]["flows"][1],
        "b.yaml"
    );
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

/// A second server instance on the harness's database, as a second replica would run.
async fn second_instance(h: &Harness) -> fleet_server::Server {
    fleet_server::start(h.config.clone()).await.unwrap()
}

/// Subscribes to `attempt` on `addr`; returns the socket and the snapshot's version.
async fn follow(addr: SocketAddr, attempt: &str) -> (Ws, u64) {
    let mut ws = live(addr, Some(VIEWER), None).await.unwrap();
    let subscribe = json!({"op": "subscribe", "project": "app", "attempt": attempt});
    ws.send(Message::text(subscribe.to_string())).await.unwrap();
    let snap = live_next(&mut ws).await;
    assert_eq!(snap["op"], "snapshot");
    let version = snap["snapshot"]["attempt"]["version"].as_u64().unwrap();
    assert_eq!(live_next(&mut ws).await["op"], "subscribed");
    (ws, version)
}

/// The next live frame, which must arrive within `within`.
async fn next_within(ws: &mut Ws, within: Duration) -> Value {
    tokio::time::timeout(within, live_next(ws))
        .await
        .expect("a live frame in time")
}

#[tokio::test]
async fn a_commit_on_one_instance_reaches_a_subscriber_on_another() {
    let h = Harness::start().await;
    let other = second_instance(&h).await;
    let mut agent = Agent::new("fanout", "e2e");
    let events = agent.suite(&[("login", true), ("pay", false)]);
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    let attempt = attempt_of(&h).await;
    // The subscriber is on the second instance; the batch commits through the first.
    let (mut ws, version) = follow(other.addr(), &attempt).await;

    c.events(events).await;
    let delta = next_within(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(delta["op"], "delta", "a delta, not a resync: {delta}");
    assert_eq!(delta["version"].as_u64().unwrap(), version + 1);
    assert_ne!(delta["entities"].as_array().unwrap().len(), 0, "{delta}");
    other.stop().await.unwrap();
}

#[tokio::test]
async fn a_lost_listener_connection_resyncs_subscribers_and_deltas_resume() {
    let h = Harness::start().await;
    let other = second_instance(&h).await;
    let mut agent = Agent::new("relisten", "e2e");
    let events = agent.suite(&[("login", true), ("pay", false)]);
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    let attempt = attempt_of(&h).await;
    let (mut ws, version) = follow(other.addr(), &attempt).await;

    // Drop every LISTEN connection on the database, as a network blip would.
    let url = h.config.database.url.as_ref().unwrap().expose();
    let db = sqlx::PgPool::connect(url).await.unwrap();
    let killed: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
          WHERE datname = current_database() AND pid <> pg_backend_pid()
            AND query LIKE 'LISTEN %'",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert!(!killed.is_empty(), "the instances hold LISTEN connections");

    let frame = next_within(&mut ws, Duration::from_secs(10)).await;
    assert_eq!(frame["op"], "resync", "{frame}");

    // The listener is back: a later commit reaches the subscriber as a delta again.
    c.events(events).await;
    let delta = next_within(&mut ws, Duration::from_secs(5)).await;
    assert_eq!(delta["op"], "delta", "{delta}");
    assert_eq!(delta["version"].as_u64().unwrap(), version + 1);
    other.stop().await.unwrap();
}

/// The first agent run's display liveness in an attempt's snapshot, as served by `addr`.
async fn liveness_at(addr: SocketAddr, attempt: &str) -> String {
    let (status, body) = get(
        addr,
        &format!("/api/v1/projects/app/attempts/{attempt}"),
        Some(VIEWER),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    v["agent_runs"][0]["liveness"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn a_run_connected_to_one_instance_is_live_on_another_until_it_goes_silent() {
    // Stale after 3 s without contact. The leader persists contact every 200 ms, and the other
    // instance judges the run from that persisted contact, 3.2 s at most after it.
    let h = Harness::start_with(|c| {
        c.liveness.stale_after_ms = 3_000;
        c.liveness.sweep_interval_ms = 200;
    })
    .await;
    let other = second_instance(&h).await;
    let mut agent = Agent::new("cross-instance", "e2e");
    let events = agent.suite(&[("login", true), ("pay", false)]);
    // The socket stays open after the batch: the run is silent, not disconnected.
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    c.events(events[..4].to_vec()).await;
    let attempt = attempt_of(&h).await;

    // Live on both while the agent keeps talking: a heartbeat every 200 ms keeps its contact well
    // inside the 3 s window, and the test waits for both instances to show it, rather than reading
    // once at an instant a loaded machine may have let the window pass.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        c.send(&ClientFrame::Heartbeat {
            clock: ClockSample {
                wall: OffsetDateTime::now_utc(),
                mono_ms: 1,
            },
            unacked: 0,
        })
        .await;
        let live = (
            liveness_at(h.addr(), &attempt).await,
            liveness_at(other.addr(), &attempt).await,
        );
        if live == ("online".to_owned(), "online".to_owned()) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a run connected to the first instance is live on both: {live:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Silent from here on: stale on both once the threshold passes.
    let shown = both_show(h.addr(), other.addr(), &attempt, "stale").await;
    assert_eq!(
        shown,
        ("stale".to_owned(), "stale".to_owned()),
        "stale on both once the agent goes silent"
    );
    drop(c);
    other.stop().await.unwrap();
}

#[tokio::test]
async fn a_run_taken_over_by_another_instance_is_live_on_both() {
    let h = Harness::start().await;
    let other = second_instance(&h).await;
    let mut agent = Agent::new("takeover", "e2e");
    let events = agent.suite(&[("login", true), ("pay", false)]);
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    c.events(events[..4].to_vec()).await;
    let attempt = attempt_of(&h).await;
    drop(c);
    // Closing the socket detaches the run: both instances show it disconnected.
    let shown = both_show(h.addr(), other.addr(), &attempt, "disconnected").await;
    assert_eq!(
        shown,
        ("disconnected".to_owned(), "disconnected".to_owned())
    );

    // The agent reconnects through the second instance. The first instance has no socket for
    // the run, but the run's persisted state and contact say it is live.
    let mut c = Client::connect(other.addr(), Some(&h.key), true).await;
    c.hello(&agent, Some(1)).await;
    let shown = both_show(h.addr(), other.addr(), &attempt, "online").await;
    assert_eq!(shown, ("online".to_owned(), "online".to_owned()));
    drop(c);
    other.stop().await.unwrap();
}

/// Waits until both instances show `want` for the attempt's run, or ten seconds pass; returns
/// what each shows.
async fn both_show(a: SocketAddr, b: SocketAddr, attempt: &str, want: &str) -> (String, String) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let shown = (liveness_at(a, attempt).await, liveness_at(b, attempt).await);
        if (shown.0 == want && shown.1 == want) || tokio::time::Instant::now() > deadline {
            return shown;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_socket_closing_on_one_instance_reaches_a_subscriber_on_another() {
    let h = Harness::start().await;
    let other = second_instance(&h).await;
    let mut agent = Agent::new("close-elsewhere", "e2e");
    let events = agent.suite(&[("login", true), ("pay", false)]);
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    c.events(events[..4].to_vec()).await;
    let attempt = attempt_of(&h).await;
    // The subscriber is on the second instance; the agent's socket is on the first.
    let (mut ws, _) = follow(other.addr(), &attempt).await;

    drop(c);
    // The close detaches the run; the delta carrying it must reach the subscriber without a
    // resync, and say the run is disconnected.
    let liveness = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let m = live_next(&mut ws).await;
            if m["op"] != "delta" {
                continue;
            }
            for e in m["entities"].as_array().unwrap() {
                if e["kind"] == "agent_run" && e["entity"]["liveness"] == "disconnected" {
                    return e["entity"]["liveness"].clone();
                }
            }
        }
    })
    .await
    .expect("a delta showing the run disconnected, within 5 s");
    assert_eq!(liveness, "disconnected");
    other.stop().await.unwrap();
}

/// Reads frames until one satisfies `want`, which must happen within ten seconds. Frames read on
/// the way are dropped.
async fn until(ws: &mut Ws, what: &str, mut want: impl FnMut(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let m = tokio::time::timeout_at(deadline, live_next(ws))
            .await
            .unwrap_or_else(|_| panic!("{what} within 10 s"));
        if want(&m) {
            return m;
        }
    }
}

/// Subscribes under `id` to `project`, and to `attempt`'s snapshot when given. Returns once the
/// subscription is confirmed.
async fn subscribe_as(ws: &mut Ws, id: &str, project: &str, attempt: Option<&str>) {
    let mut req = json!({"op": "subscribe", "id": id, "project": project});
    if let Some(attempt) = attempt {
        req["attempt"] = json!(attempt);
    }
    ws.send(Message::text(req.to_string())).await.unwrap();
    until(ws, &format!("{id} confirmed"), |m| {
        m["sub"] == id && m["op"] == "subscribed"
    })
    .await;
}

#[tokio::test]
async fn one_socket_carries_several_subscriptions_and_each_gets_its_own_frames() {
    let h = Harness::start().await;
    let mut agent = Agent::new("multi", "e2e");
    let events = agent.suite(&[("login", true), ("pay", false)]);
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    let attempt = attempt_of(&h).await;

    let mut ws = live(h.addr(), Some(VIEWER), None).await.unwrap();
    subscribe_as(&mut ws, "flow", "app", Some(&attempt)).await;
    subscribe_as(&mut ws, "project", "app", None).await;

    // One batch per event: each commit is a version.
    for e in events {
        c.events(vec![e]).await;
    }
    // Both subscriptions see the attempt's deltas, each tagged with its own key and in order.
    let mut versions: std::collections::BTreeMap<String, Vec<u64>> =
        std::collections::BTreeMap::default();
    while !(versions.get("flow").is_some_and(|v| v.len() >= 3)
        && versions.get("project").is_some_and(|v| v.len() >= 3))
    {
        let m = next_within(&mut ws, Duration::from_secs(5)).await;
        if m["op"] == "delta" {
            let key = m["sub"].as_str().expect("a delta names its subscription");
            versions
                .entry(key.to_owned())
                .or_default()
                .push(m["version"].as_u64().unwrap());
        }
    }
    for key in ["flow", "project"] {
        let seen = &versions[key];
        assert!(
            seen.windows(2).all(|w| w[1] == w[0] + 1),
            "{key}: contiguous versions {seen:?}"
        );
    }
    assert_eq!(
        versions["flow"][..3],
        versions["project"][..3],
        "the same changes, in the same order"
    );
}

#[tokio::test]
async fn a_refused_subscription_leaves_the_others_working_and_the_socket_open() {
    let h = Harness::start().await;
    let mut agent = Agent::new("refused", "e2e");
    let events = agent.suite(&[("login", true)]);
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    let mut ws = live(h.addr(), Some(VIEWER), None).await.unwrap();
    subscribe_as(&mut ws, "good", "app", None).await;

    ws.send(Message::text(
        json!({"op": "subscribe", "id": "bad", "project": "nowhere"}).to_string(),
    ))
    .await
    .unwrap();
    let refusal = until(&mut ws, "the refusal", |m| m["op"] == "error").await;
    assert_eq!(refusal["sub"], "bad", "{refusal}");
    assert_eq!(refusal["message"], "no such project nowhere");

    // The socket is still open: the good subscription gets the next change.
    c.events(events).await;
    let delta = until(&mut ws, "a delta under good", |m| m["op"] == "delta").await;
    assert_eq!(delta["sub"], "good", "{delta}");
}

#[tokio::test]
async fn an_unsubscribed_key_gets_no_more_frames_and_the_others_keep_theirs() {
    let h = Harness::start().await;
    let mut agent = Agent::new("unsub", "e2e");
    let events = agent.suite(&[("login", true), ("pay", false)]);
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    let mut ws = live(h.addr(), Some(VIEWER), None).await.unwrap();
    subscribe_as(&mut ws, "a", "app", None).await;
    subscribe_as(&mut ws, "b", "app", None).await;

    ws.send(Message::text(
        json!({"op": "unsubscribe", "id": "b"}).to_string(),
    ))
    .await
    .unwrap();
    until(&mut ws, "b unsubscribed", |m| {
        m["sub"] == "b" && m["op"] == "unsubscribed"
    })
    .await;

    for e in events {
        c.events(vec![e]).await;
    }
    let mut under_a = 0;
    while under_a < 2 {
        let m = next_within(&mut ws, Duration::from_secs(5)).await;
        assert_ne!(m["sub"], "b", "nothing under b after unsubscribed: {m}");
        if m["sub"] == "a" && m["op"] == "delta" {
            under_a += 1;
        }
    }
}

#[tokio::test]
async fn unsubscribing_a_key_the_socket_does_not_hold_is_refused_alone() {
    let h = Harness::start().await;
    let mut ws = live(h.addr(), Some(VIEWER), None).await.unwrap();
    ws.send(Message::text(
        json!({"op": "unsubscribe", "id": "ghost"}).to_string(),
    ))
    .await
    .unwrap();
    let refusal = live_next(&mut ws).await;
    assert_eq!(refusal["op"], "error");
    assert_eq!(refusal["sub"], "ghost", "{refusal}");
    subscribe_as(&mut ws, "after", "app", None).await;
}

#[tokio::test]
async fn the_socket_caps_its_subscriptions_and_a_freed_slot_takes_another() {
    let h = Harness::start().await;
    let mut ws = live(h.addr(), Some(VIEWER), None).await.unwrap();
    for n in 0..32 {
        subscribe_as(&mut ws, &format!("s{n}"), "app", None).await;
    }
    ws.send(Message::text(
        json!({"op": "subscribe", "id": "s32", "project": "app"}).to_string(),
    ))
    .await
    .unwrap();
    let refusal = until(&mut ws, "the cap's refusal", |m| m["op"] == "error").await;
    assert_eq!(refusal["sub"], "s32", "{refusal}");
    assert!(
        refusal["message"].as_str().unwrap().contains("32"),
        "{refusal}"
    );

    ws.send(Message::text(
        json!({"op": "unsubscribe", "id": "s0"}).to_string(),
    ))
    .await
    .unwrap();
    until(&mut ws, "s0 unsubscribed", |m| m["op"] == "unsubscribed").await;
    subscribe_as(&mut ws, "s32", "app", None).await;
}

#[tokio::test]
async fn a_request_that_is_not_a_subscription_is_answered_and_the_socket_stays_open() {
    let h = Harness::start().await;
    let mut ws = live(h.addr(), Some(VIEWER), None).await.unwrap();
    ws.send(Message::text(json!({"op": "dance"}).to_string()))
        .await
        .unwrap();
    let answer = live_next(&mut ws).await;
    assert_eq!(answer["op"], "error");
    assert!(
        answer.get("sub").is_none(),
        "no subscription answers it: {answer}"
    );
    subscribe_as(&mut ws, "still", "app", None).await;
}

/// A telemetry frame with one host sample, taken now.
fn one_sample() -> ClientFrame {
    let at = ClockSample {
        wall: OffsetDateTime::now_utc(),
        mono_ms: 1,
    };
    ClientFrame::Telemetry(Telemetry {
        tseq_from: 1,
        tseq_to: 1,
        dropped_since_last: 0,
        devices: Vec::new(),
        samples: vec![Sample {
            at,
            host_cpu_pct: Some(12.5),
            host_mem_used_bytes: None,
            host_mem_total_bytes: None,
            maestro_cpu_pct: None,
            maestro_rss_bytes: None,
            agent_cpu_pct: None,
            agent_rss_bytes: None,
            host_load_1m: None,
            host_cpu_cores: None,
            host_swap_used_bytes: None,
            host_swap_total_bytes: None,
            host_psi_cpu_pct: None,
            host_psi_memory_pct: None,
            host_psi_io_pct: None,
            unavailable: SampleUnavailable::default(),
        }],
    })
}

#[tokio::test]
async fn a_committed_telemetry_checkpoint_tells_the_project_its_samples_changed() {
    let h = Harness::start().await;
    let mut agent = Agent::new("samples", "e2e");
    agent.suite(&[("login", true)]);
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    let attempt = attempt_of(&h).await;
    let mut ws = live(h.addr(), Some(VIEWER), None).await.unwrap();
    subscribe_as(&mut ws, "project", "app", None).await;
    subscribe_as(&mut ws, "flow", "app", Some(&attempt)).await;

    c.send(&one_sample()).await;
    let samples = until(&mut ws, "the samples frame", |m| m["op"] == "samples").await;
    assert_eq!(samples["sub"], "project", "{samples}");
    assert_eq!(samples["project_id"], json!(project_id(&h).await));

    // An attempt subscription does not follow the project's samples.
    let quiet = tokio::time::timeout(Duration::from_millis(600), async {
        loop {
            let m = live_next(&mut ws).await;
            if m["op"] == "samples" && m["sub"] == "flow" {
                return m;
            }
        }
    })
    .await;
    assert!(quiet.is_err(), "the attempt got samples: {quiet:?}");
}

/// The project's id, read from the list the viewer sees.
async fn project_id(h: &Harness) -> String {
    let (status, body) = get(h.addr(), "/api/v1/projects", Some(VIEWER)).await;
    assert_eq!(status, 200, "{body}");
    let list: Value = serde_json::from_str(&body).unwrap();
    list[0]["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn samples_committed_on_one_instance_reach_a_subscriber_on_another() {
    let h = Harness::start().await;
    let other = second_instance(&h).await;
    let agent = Agent::new("samples-elsewhere", "e2e");
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    let mut ws = live(other.addr(), Some(VIEWER), None).await.unwrap();
    subscribe_as(&mut ws, "project", "app", None).await;

    c.send(&one_sample()).await;
    let samples = until(&mut ws, "the samples frame from the other instance", |m| {
        m["op"] == "samples"
    })
    .await;
    assert_eq!(samples["sub"], "project", "{samples}");
    other.stop().await.unwrap();
}
