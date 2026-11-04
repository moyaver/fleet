// SPDX-License-Identifier: AGPL-3.0-only
//! A whole run produced by `fleet-loadgen` (simulated agents on the real protocol), read through
//! every screen: every outcome carries its provenance (I4) and the numbers agree across screens.

use std::time::Duration;

use serde_json::Value;

use super::rig::{Rig, rig};

const TERMINAL: [&str; 5] = ["passed", "failed", "skipped", "interrupted", "unknown"];

async fn loadgen(rig: &Rig) -> fleet_loadgen::Report {
    let address = rig.app.server_address().unwrap();
    fleet_loadgen::run(fleet_loadgen::Options {
        server: format!(
            "ws://{}:{}",
            address.host_str().unwrap(),
            address.port().unwrap()
        ),
        token: rig.key.clone(),
        run_id: "lg".into(),
        agents: 2,
        devices: 2,
        flows: 3,
        job_keys: 2,
        deadline: Duration::from_secs(60),
        ..fleet_loadgen::Options::default()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn every_outcome_on_every_screen_carries_its_provenance_and_the_screens_agree() {
    let rig = rig().await;
    let report = loadgen(&rig).await;
    let failed: u32 = report.agents.iter().map(|a| a.failed_flows).sum();
    let attempt = rig.attempt_id("app").await;

    // A: one finished attempt, two closed jobs with outcomes and sources, 12 flows in all.
    let overview = rig.json("/api/v1/projects/app/overview").await;
    assert_eq!(overview["active"].as_array().unwrap().len(), 0);
    let recent = &overview["recent"][0];
    assert_eq!(recent["attempt"]["id"], attempt.as_str());
    let jobs = recent["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2);
    for job in jobs {
        assert_eq!(job["lifecycle"], "closed");
        assert!(job["outcome"].is_string(), "{job}");
        assert!(job["outcome_source"].is_string(), "{job}");
    }
    let flows = &recent["flows"];
    assert_eq!(flows["failed"], i64::from(failed));
    assert_eq!(flows["passed"], i64::from(12 - failed));
    assert_eq!(
        overview["worker_counts"]["completed"], 2,
        "both agents said goodbye"
    );
    assert_eq!(overview["workers"].as_array().unwrap().len(), 2);

    // B: four device lanes, twelve bars, each with the source of its outcome.
    let timeline = rig
        .json(&format!("/api/v1/projects/app/attempts/{attempt}/timeline"))
        .await;
    assert_eq!(timeline["lanes"].as_array().unwrap().len(), 4);
    let bars = timeline["bars"].as_array().unwrap();
    assert_eq!(bars.len(), 12);
    for bar in bars {
        assert!(TERMINAL.contains(&bar["state"].as_str().unwrap()), "{bar}");
        assert!(bar["outcome_source"].is_string(), "{bar}");
        assert_eq!(
            bar["finality"], "verified",
            "the JUnit report settled it: {bar}"
        );
        assert!(bar["lane_id"].is_string() && bar["ended_at"].is_string());
    }

    // D: each flow's winning claim is one of its claims and agrees with its state.
    for bar in bars {
        let id = bar["flow_id"].as_str().unwrap();
        let view = rig.json(&format!("/api/v1/projects/app/flows/{id}")).await;
        let claims = view["claims"].as_array().unwrap();
        let sources: Vec<&str> = claims
            .iter()
            .map(|c| c["source"].as_str().unwrap())
            .collect();
        assert_eq!(sources, ["live_console", "junit_final"], "{id}");
        let winners: Vec<&Value> = claims.iter().filter(|c| c["winning"] == true).collect();
        assert_eq!(winners.len(), 1, "exactly one winner: {claims:?}");
        assert_eq!(winners[0]["source"], view["flow"]["outcome_source"]);
        assert_eq!(winners[0]["outcome"], view["flow"]["state"]);
        assert_eq!(view["steps"].as_array().unwrap().len(), 2);
        let failed_flow = view["flow"]["state"] == "failed";
        assert_eq!(view["failure"].is_object(), failed_flow, "{id}");
    }
}

#[tokio::test]
async fn a_loadgen_worker_reports_the_host_metrics_it_sent_with_their_units() {
    let rig = rig().await;
    loadgen(&rig).await;
    let overview = rig.json("/api/v1/projects/app/overview").await;
    let worker = overview["workers"][0]["id"].as_str().unwrap();
    let path = format!(
        "/api/v1/projects/app/workers/{worker}/metrics?metrics=host_cpu_pct,host_mem_total_bytes"
    );

    // Telemetry is written at the next checkpoint (50 ms in tests).
    let view = rig
        .until(&path, |v| v["series"][0]["available"] == true)
        .await;

    let cpu = &view["series"][0];
    assert_eq!(
        (cpu["unit"].as_str(), cpu["scope"].as_str()),
        (Some("percent"), Some("host"))
    );
    assert!(
        cpu["points"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["value"] == 25.0)
    );
    let total = &view["series"][1];
    assert_eq!(total["unit"], "bytes");
    assert_eq!(
        total["points"][0]["value"].as_f64(),
        Some(16.0 * 1024.0 * 1024.0 * 1024.0)
    );
}
