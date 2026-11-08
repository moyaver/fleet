// SPDX-License-Identifier: AGPL-3.0-only
//! The job summary (`GITHUB_STEP_SUMMARY`): written at run end as its own file, so Maestro's output
//! and exit status never change (I1), and a summary that cannot be written costs one notice (I2).

use std::{fs, net::TcpListener, path::PathBuf};

use super::stream::{fixture, run_agent};

/// A fresh, empty summary file for one test.
fn summary_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fleet-summary-{name}-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("summary.md");
    let _ = fs::remove_file(&path);
    path
}

#[test]
fn the_summary_lists_counts_and_failed_flows_and_maestro_is_unchanged() {
    let path = summary_path("suite");
    let (with, _, _) = run_agent(
        "suite",
        None,
        &[],
        &[("GITHUB_STEP_SUMMARY", path.to_str().unwrap())],
    );
    let (without, _, _) = run_agent("suite", None, &[], &[]);
    assert_eq!(with.stdout, fs::read(fixture("suite/stdout.bin")).unwrap());
    assert_eq!(with.stdout, without.stdout, "stdout differs with a summary");
    assert_eq!(with.status.code(), Some(1));
    assert_eq!(with.status.code(), without.status.code());

    let summary = fs::read_to_string(&path).expect("the summary is written");
    assert!(summary.starts_with("## Fleet\n"), "{summary}");
    assert!(
        summary.contains("Flows: 1 failed, 3 passed (4 in total)."),
        "{summary}"
    );
    assert!(
        summary.contains(
            "| delta-fails | failed | verified (junit_final) | Assertion is false: \"Fleet never shows this\" is visible |"
        ),
        "{summary}"
    );
    assert!(
        !summary.contains("| alpha |"),
        "passed flows are counted, not listed: {summary}"
    );
    assert!(
        !summary.contains("dashboard"),
        "no server, no link: {summary}"
    );
}

#[test]
fn a_configured_server_adds_a_link_to_its_dashboard() {
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let path = summary_path("link");
    let server = format!("http://{closed}");
    let (out, _, _) = run_agent(
        "fail",
        Some(&server),
        &[],
        &[("GITHUB_STEP_SUMMARY", path.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(1));
    let summary = fs::read_to_string(&path).expect("the summary is written");
    assert!(
        summary.contains(&format!("[Open the Fleet dashboard](http://{closed}/)")),
        "{summary}"
    );
    assert!(
        summary.contains("| fail | failed | verified (junit_final) |"),
        "{summary}"
    );
}

#[test]
fn an_unwritable_summary_changes_nothing_and_costs_one_notice() {
    let missing = std::env::temp_dir()
        .join(format!("fleet-summary-missing-{}", std::process::id()))
        .join("no-such-dir")
        .join("summary.md");
    let (out, _, _) = run_agent(
        "fail",
        None,
        &[],
        &[("GITHUB_STEP_SUMMARY", missing.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(1), "exit status");
    assert_eq!(out.stdout, fs::read(fixture("fail/stdout.bin")).unwrap());
    // The fake Maestro writes nothing to stderr, so the notices are all that is there: the
    // offline run's bundle notice (issue #83), and the summary's.
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        stderr.lines().count(),
        2,
        "two notices and nothing else: {stderr}"
    );
    assert!(
        stderr.contains("[fleet-agent] could not write the job summary to GITHUB_STEP_SUMMARY"),
        "{stderr}"
    );
    assert!(!missing.exists());
}
