// SPDX-License-Identifier: AGPL-3.0-only
//! Gap 1 (issue #148), end to end: a live run whose own flags put Maestro's files elsewhere settles
//! its steps on the server, as the default layout does. The fake Maestro writes the files where the
//! real CLI does (research 01 §4.1); the agent's watcher must follow them.

use crate::support::{Stack, run_agent_with_flags};

#[tokio::test(flavor = "multi_thread")]
async fn a_live_run_whose_flags_move_maestros_files_settles_its_steps_on_the_server() {
    let stack = Stack::start(|_| {}).await;
    let ws = stack.ws_url();
    let key = stack.key.clone();
    // The session log goes under the debug directory's own `.maestro/tests`, and the flow bundles
    // under the test output directory: two trees, as `paths-debug-and-test-output-apart` shows.
    let out = tokio::task::spawn_blocking(move || {
        run_agent_with_flags(
            "pass",
            &[
                ("FLEET_SERVER", &ws),
                ("FLEET_TOKEN", &key),
                ("FLEET_UPLOAD_TIMEOUT", "20"),
            ],
            &[
                "--debug-output",
                "{DIR}/debug",
                "--test-output-dir",
                "{DIR}/tests",
            ],
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(0), "Maestro's own exit status (I1)");

    let snap = stack.await_run_ended("job").await;
    let flow = snap["flows"][0]["id"].as_str().unwrap().to_owned();
    let detail = stack
        .get(&format!("/api/v1/projects/app/flows/{flow}"))
        .await;
    let steps = detail["steps"].as_array().unwrap();
    assert!(!steps.is_empty(), "the flow reported its steps: {detail}");
    let settled: Vec<_> = steps
        .iter()
        .filter(|s| s["source"] == "artifact_file")
        .collect();
    assert_eq!(
        settled.len(),
        steps.len(),
        "every step is settled from commands.json, not left on the live log: {steps:?}"
    );
    assert!(
        settled.iter().all(|s| s["status"] == "completed"),
        "the pass fixture completes every step: {settled:?}"
    );
}
