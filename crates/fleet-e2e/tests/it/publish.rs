// SPDX-License-Identifier: AGPL-3.0-only
//! `fleet-agent publish` against the real server (issue #83): a Maestro output directory, and an
//! offline run's bundle, reach the server as the run they describe, with their artifacts.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::{
    browser::Browser,
    dashboard_ab::{Page, first_attempt, stack as dashboard_stack},
    fixtures::inspected,
    support::{Stack, agent_bin, fixture, flows, run_agent_keeping_run_dir},
};

/// When the captured `fail` run started, in Unix milliseconds (its log's first record).
const FAIL_STARTED_MS: i64 = 1_793_751_906_337;
const DAY_MS: i64 = 86_400_000;

/// A copy of the `fail` run's Maestro debug directory with every recorded time moved `days` back,
/// as if the run had happened that many days ago. The directory has no `meta.json`: publish reads
/// its times from the files, which is the case this checks.
fn run_days_ago(days: i64) -> PathBuf {
    let src = fixture("fail").join("run/debug");
    let dst = temp(&format!("ago{days}"));
    let shift = days * DAY_MS;
    copy_tree(&src, &dst, shift);
    dst
}

fn copy_tree(src: &Path, dst: &Path, shift: i64) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if from.is_dir() {
            copy_tree(&from, &to, shift);
        } else if entry.file_name() == "maestro.log" {
            let text = fs::read_to_string(&from).unwrap();
            let moved: Vec<String> = text
                .lines()
                .map(|line| match line.strip_prefix("MFLT1|") {
                    Some(rest) => {
                        let (ms, tail) = rest.split_once('|').unwrap();
                        let ms: i64 = ms.parse().unwrap();
                        format!("MFLT1|{}|{tail}", ms - shift)
                    }
                    None => line.to_owned(),
                })
                .collect();
            fs::write(&to, moved.join("\n") + "\n").unwrap();
        } else if entry.file_name() == "commands.json" {
            let mut commands: Value = serde_json::from_slice(&fs::read(&from).unwrap()).unwrap();
            for c in commands.as_array_mut().unwrap() {
                let ts = &mut c["metadata"]["timestamp"];
                *ts = Value::from(ts.as_i64().unwrap() - shift);
            }
            fs::write(&to, serde_json::to_vec_pretty(&commands).unwrap()).unwrap();
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

/// The UTC date (`YYYY-MM-DD`) of a Unix-millisecond instant.
fn utc_date(ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).unwrap();
    t.date().to_string()
}

/// `fleet-agent publish <path>` in a cleared environment (CI sets `GITHUB_*`, which the agent would
/// read), plus `env`.
pub(crate) fn publish(path: &Path, env: &[(&str, &str)]) -> Output {
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
    // The live run is live; the published one says it was imported (I4).
    assert_eq!(live_snap["agent_runs"][0]["origin"], "live", "{live_snap}");
    assert_eq!(snap["agent_runs"][0]["origin"], "published", "{snap}");
    assert_eq!(snap["flows"][0]["origin"], "published", "{snap}");
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

#[tokio::test(flavor = "multi_thread")]
async fn a_published_run_keeps_the_day_it_ran_and_says_it_was_published() {
    let stack = stack().await;
    let (ws, key) = (stack.ws_url(), stack.key.clone());
    let dir = run_days_ago(3);
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
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snap = stack.await_run_ended("job").await;
    assert_eq!(snap["agent_runs"][0]["origin"], "published", "{snap}");

    // The run's events and flows carry the times the directory records, three days back, not the
    // time it was published (and not the replay epoch).
    let then = utc_date(FAIL_STARTED_MS - 3 * DAY_MS);
    let flow = snap["flows"][0]["id"].as_str().unwrap().to_owned();
    let detail = stack
        .get(&format!("/api/v1/projects/app/flows/{flow}"))
        .await;
    let started = detail["started_at"].as_str().unwrap();
    assert!(started.starts_with(&then), "started {started}, want {then}");
    assert_eq!(detail["flow"]["origin"], "published", "{detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_published_run_older_than_the_journal_keeps_is_refused_with_the_reason() {
    let stack = stack().await;
    let (ws, key) = (stack.ws_url(), stack.key.clone());
    let dir = run_days_ago(30);
    // The upload budget is long on purpose: a refused run's uploads never finish, and the
    // refusal must end the publish long before that budget runs out.
    let budget = Duration::from_secs(60);
    let started = Instant::now();
    let out = tokio::task::spawn_blocking(move || {
        publish(
            &dir,
            &[
                ("FLEET_SERVER", &ws),
                ("FLEET_TOKEN", &key),
                ("FLEET_UPLOAD_TIMEOUT", &budget.as_secs().to_string()),
                ("FLEET_FLUSH_TIMEOUT", "15"),
            ],
        )
    })
    .await
    .unwrap();
    let elapsed = started.elapsed();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("removed at once"), "{stderr}");
    assert!(
        elapsed < Duration::from_secs(10),
        "a refused publish took {elapsed:?}, the upload budget is {budget:?}: {stderr}"
    );
    // The refused run never got an attempt.
    let attempts = stack.get("/api/v1/projects/app/attempts").await;
    assert_eq!(attempts, serde_json::json!([]), "nothing was kept");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_published_run_shows_the_published_label_on_its_pipeline_page() {
    // Anonymous reads, as the other dashboard tests have, so the page shows its data.
    let stack = dashboard_stack(|_| {}).await;
    let (ws, key) = (stack.ws_url(), stack.key.clone());
    let dir = run_days_ago(3);
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
    assert_eq!(out.status.code(), Some(0));
    stack.await_run_ended("job").await;

    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    let attempt = first_attempt(&stack).await;
    browser
        .client
        .goto(&format!(
            "http://{}/p/app/pipelines/{attempt}",
            stack.addr()
        ))
        .await
        .unwrap();
    page.wait(
        "the published label on a flow",
        "document.querySelectorAll('[data-testid=\"flow-row\"] [data-testid=\"origin-badge\"]').length > 0",
    )
    .await;
    let label = page
        .eval(
            "return document.querySelector('[data-testid=\"origin-badge\"]').getAttribute('aria-label');",
            vec![],
        )
        .await;
    assert_eq!(label, "Origin: published run");
    browser.quit().await;
}
