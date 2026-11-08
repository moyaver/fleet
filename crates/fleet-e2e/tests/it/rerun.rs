// SPDX-License-Identifier: AGPL-3.0-only
//! Rerunning failed flows (issue #82), end to end. A live run whose flow failed shows the same
//! copyable Maestro command on screen D (the flow) and screen E (the history), with a link to the
//! attempt's failed-flows manifest. The downloaded manifest is one the agent reads as a plan: a
//! rerun from it passes exactly the failed flow to Maestro.

use serde_json::{Value, json};

use crate::{
    browser::Browser,
    dashboard_ab::{Page, stack},
    support::{VIEWER, http, run_agent_planned, run_agent_planned_keeping_run_dir},
};

/// The flow's own path, as the run's log named it. The captured fixture names the flow by the
/// placeholder it was captured with, and Fleet keeps the path Maestro reported.
const FLOW: &str = "<REPO>/examples/flows/phase0/fail.yaml";

/// The command a user copies for the failed flow: the run's words, then the flow's own path, quoted.
const COMMAND: &str = "maestro test '<REPO>/examples/flows/phase0/fail.yaml'";

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario, read top to bottom: screen D, screen E, the run page, then the download"
)]
async fn a_failed_flow_offers_its_rerun_command_and_its_attempts_manifest_on_screens_d_and_e() {
    let stack = stack(|_| {}).await;
    let (ws, key) = (stack.ws_url(), stack.key.clone());
    let plan = json!({"planId": "e2e-rerun", "shards": [{"id": 0, "flows": ["flows/fail.yaml"]}]});
    let planned = plan.clone();
    let out = tokio::task::spawn_blocking(move || {
        run_agent_planned(
            "fail",
            &[
                ("FLEET_SERVER", &ws),
                ("FLEET_TOKEN", &key),
                ("FLEET_UPLOAD_TIMEOUT", "20"),
            ],
            &planned,
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(1), "Maestro's own exit status (I1)");

    let snap = stack.await_run_ended("job").await;
    let attempt = stack.get("/api/v1/projects/app/attempts").await[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let failed = snap["flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "fail")
        .unwrap_or_else(|| panic!("the fail flow ran: {snap}"));
    assert_eq!(failed["state"], "failed", "{failed}");
    let flow = failed["id"].as_str().unwrap().to_owned();
    let manifest_path = format!("/api/v1/projects/app/attempts/{attempt}/failed-flows");

    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };

    // Screen D: the command in selectable text, and the manifest link.
    browser
        .client
        .goto(&format!("http://{}/flows/{flow}?project=app", stack.addr()))
        .await
        .unwrap();
    page.wait(
        "the rerun command on screen D",
        "document.querySelector('[data-testid=\"rerun-command\"] code')",
    )
    .await;
    let shown = page
        .eval(
            "return document.querySelector('[data-testid=\"rerun-command\"] code').textContent;",
            vec![],
        )
        .await;
    assert_eq!(shown, COMMAND);
    let href = page
        .eval(
            "return document.querySelector('[data-testid=\"failed-flows-link\"]').getAttribute('href');",
            vec![],
        )
        .await;
    assert_eq!(href, manifest_path);
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());

    // Screen E: the same command, behind the row's rerun disclosure, with the same link.
    browser
        .client
        .goto(&format!("http://{}/history?project=app", stack.addr()))
        .await
        .unwrap();
    page.wait(
        "the rerun disclosure on screen E",
        "document.querySelector('[data-testid=\"rerun-details\"] code')",
    )
    .await;
    let row = page
        .eval(
            "return document.querySelector('[data-testid=\"rerun-details\"] code').textContent;",
            vec![],
        )
        .await;
    assert_eq!(row, COMMAND);
    let href = page
        .eval(
            "return document.querySelector('[data-testid=\"rerun-details\"] [data-testid=\"failed-flows-link\"]').getAttribute('href');",
            vec![],
        )
        .await;
    assert_eq!(href, manifest_path);
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());

    // The run page (screen B) lists the failures and links the same manifest.
    browser
        .client
        .goto(&format!(
            "http://{}/p/app/pipelines/{attempt}",
            stack.addr()
        ))
        .await
        .unwrap();
    page.wait(
        "the failed-flows link on the run page",
        "document.querySelector('[data-testid=\"failed-flows-link\"]')",
    )
    .await;
    let href = page
        .eval(
            "return document.querySelector('[data-testid=\"failed-flows-link\"]').getAttribute('href');",
            vec![],
        )
        .await;
    assert_eq!(href, manifest_path);
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;

    // The download parses as a plan, and reruns exactly the failed flow.
    let (status, body) = http(stack.addr(), "GET", &manifest_path, Some(VIEWER), "").await;
    assert_eq!(status, 200, "{body}");
    let downloaded: Value = serde_json::from_str(&body).unwrap();
    let flows: Vec<&str> = downloaded["shards"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| {
            s["flows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f.as_str().unwrap())
        })
        .collect();
    assert_eq!(flows, [FLOW], "{body}");

    let value = downloaded;
    let (rerun, run_dir) =
        tokio::task::spawn_blocking(move || run_agent_planned_keeping_run_dir("fail", &[], &value))
            .await
            .unwrap();
    assert_eq!(
        rerun.status.code(),
        Some(1),
        "the agent accepted the manifest as a plan (a refused plan exits 2)"
    );
    let journal = std::fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    let assigned: Vec<String> = journal
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["type"] == "shard.assigned")
        .flat_map(|event| {
            event["data"]["planned"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| f["path"].as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        assigned,
        [FLOW],
        "the rerun's shard is the failed flow alone"
    );
}
