// SPDX-License-Identifier: AGPL-3.0-only
//! `fleet-agent publish` against the real server (issue #83): a Maestro output directory, and an
//! offline run's bundle, reach the server as the run they describe, with their artifacts.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use crate::{
    fixtures::inspected,
    support::{Stack, agent_bin, fixture, flows, run_agent_keeping_run_dir},
};

/// `fleet-agent publish <path>` in a cleared environment (CI sets `GITHUB_*`, which the agent would
/// read), plus `env`.
fn publish(path: &Path, env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(agent_bin());
    cmd.env_clear();
    for keep in ["PATH", "HOME", "TMPDIR"] {
        if let Some(v) = std::env::var_os(keep) {
            cmd.env(keep, v);
        }
    }
    cmd.arg("publish").arg(path).stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fleet-e2e-publish-{name}-{}", uuid::Uuid::now_v7()))
}

async fn stack() -> Stack {
    Stack::start(|c| {
        c.storage.path = temp("blobs");
        c.artifacts.spool_dir = temp("spool");
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_maestro_output_directory_publishes_as_the_run_it_describes() {
    let stack = stack().await;
    let (ws, key) = (stack.ws_url(), stack.key.clone());
    let dir = fixture("fail");
    let out = tokio::task::spawn_blocking(move || {
        publish(
            &dir,
            &[
                ("FLEET_SERVER", &ws),
                ("FLEET_TOKEN", &key),
                ("FLEET_UPLOAD_TIMEOUT", "20"),
            ],
        )
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.starts_with("published run "), "{stdout}");

    let snap = stack.await_run_ended("job").await;
    // The server holds what `inspect` makes of the directory, as a live run's outcomes are (the
    // provenance is the report's, never live console output).
    assert_eq!(flows(&snap), inspected("fail"));

    let flow = snap["flows"][0]["id"].as_str().unwrap().to_owned();
    let listed = stack
        .get(&format!("/api/v1/projects/app/flows/{flow}/artifacts"))
        .await;
    let artifacts = listed["artifacts"].as_array().unwrap();
    for class in ["failure_screenshot", "commands", "maestro_log"] {
        assert!(
            artifacts.iter().any(|a| a["class"] == class),
            "no {class} in {listed}"
        );
    }
    assert!(
        artifacts.iter().all(|a| a["state"] == "available"),
        "{listed}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_offline_run_publishes_as_the_same_run_as_the_live_one() {
    // The live run, on its own server.
    let live = stack().await;
    let (ws, key) = (live.ws_url(), live.key.clone());
    let live_out = tokio::task::spawn_blocking(move || {
        run_agent_keeping_run_dir(
            "fail",
            &[
                ("FLEET_SERVER", &ws),
                ("FLEET_TOKEN", &key),
                ("FLEET_FLUSH_TIMEOUT", "15"),
            ],
        )
        .0
    })
    .await
    .unwrap();
    assert_eq!(live_out.status.code(), Some(1), "Maestro's own status (I1)");
    let live_snap = live.await_run_ended("job").await;

    // The offline run, on a server that never sees it. Then its bundle is published to another.
    let offline = stack().await;
    let (out, run_dir) = tokio::task::spawn_blocking(|| run_agent_keeping_run_dir("fail", &[]))
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "Maestro's own status (I1)");
    let bundle = run_dir.join("fleet-bundle.tar.zst");
    assert!(
        bundle.is_file(),
        "the offline run kept {}",
        bundle.display()
    );

    let (ws, key) = (offline.ws_url(), offline.key.clone());
    let published = tokio::task::spawn_blocking(move || {
        publish(
            &bundle,
            &[
                ("FLEET_SERVER", &ws),
                ("FLEET_TOKEN", &key),
                ("FLEET_UPLOAD_TIMEOUT", "20"),
            ],
        )
    })
    .await
    .unwrap();
    assert_eq!(
        published.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&published.stderr)
    );
    let snap = offline.await_run_ended("job").await;

    assert_eq!(
        flows(&snap),
        flows(&live_snap),
        "the same flows, outcomes and provenance"
    );
    let flow = snap["flows"][0]["id"].as_str().unwrap().to_owned();
    let listed = offline
        .get(&format!("/api/v1/projects/app/flows/{flow}/artifacts"))
        .await;
    assert!(
        listed["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["class"] == "failure_screenshot" && a["state"] == "available"),
        "{listed}"
    );
}
