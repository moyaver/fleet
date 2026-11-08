// SPDX-License-Identifier: AGPL-3.0-only
//! Each exception mark on a step row (issues #148, #149): a value still read from the live log is
//! Stale once its run has gone quiet, and Inferred once the server abandons the run, and every open
//! row of the flow says so. Settled rows carry no mark (`dashboard_cd`), and Provisional rows are
//! covered by `dashboard_cd::steps_the_file_does_not_settle_stay_provisional_and_each_row_says_so`.
//! Missing has no step row today: nothing in the pipeline records a step as having no value.

use crate::{
    browser::Browser,
    dashboard_ab::{Page, count_of, first_attempt, run_options, stack},
};

/// The steps of the attempt's first flow, which the agent has reported by the time `quiet` returns.
async fn first_flow(stack: &crate::support::Stack) -> String {
    let attempt = first_attempt(stack).await;
    let snapshot = stack
        .get(&format!("/api/v1/projects/app/attempts/{attempt}"))
        .await;
    snapshot["flows"][0]["id"].as_str().unwrap().to_owned()
}

/// JS: every step row of the page carries the given provenance mark, and there is at least one row.
fn every_row_marked(label: &str) -> String {
    format!(
        "(() => {{ const rows = Array.from(document.querySelectorAll('[role=treeitem]')); \
         return rows.length >= 1 && rows.every(e => e.querySelector('[role=img]')?.getAttribute('aria-label') === '{label}'); }})()"
    )
}

#[tokio::test]
async fn a_quiet_runs_open_steps_are_stale_and_each_row_says_so() {
    let stack = stack(|c| {
        c.liveness.stale_after_ms = 4_000;
        c.liveness.sweep_interval_ms = 500;
    })
    .await;
    // The first flow starts, its steps are read from the live log, and then the agent says nothing.
    let agent = fleet_loadgen::quiet(&run_options(&stack, "stale-steps"), 6)
        .await
        .unwrap();
    let flow = first_flow(&stack).await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://{}/flows/{flow}?project=app", stack.addr()))
        .await
        .unwrap();
    page.mark_loaded().await;
    page.wait(
        "the steps",
        &format!("{} >= 1", count_of("[role=treeitem]")),
    )
    .await;
    page.wait(
        "every open step to turn stale",
        &every_row_marked("Provenance: stale"),
    )
    .await;
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
    drop(agent);
}

#[tokio::test]
async fn an_abandoned_runs_open_steps_are_inferred_and_each_row_says_so() {
    let stack = stack(|c| {
        // Once the agent has gone, the server abandons its run after one second (the sweep runs
        // every half second).
        c.liveness.resume_grace_ms = 1_000;
        c.liveness.sweep_interval_ms = 500;
    })
    .await;
    let agent = fleet_loadgen::quiet(&run_options(&stack, "inferred-steps"), 6)
        .await
        .unwrap();
    let flow = first_flow(&stack).await;
    // The connection closes without a bye: the run is detached, and then abandoned.
    agent.disconnect().await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://{}/flows/{flow}?project=app", stack.addr()))
        .await
        .unwrap();
    page.mark_loaded().await;
    page.wait(
        "the steps",
        &format!("{} >= 1", count_of("[role=treeitem]")),
    )
    .await;
    page.wait(
        "every open step to be inferred",
        &every_row_marked("Provenance: inferred"),
    )
    .await;
    let statuses = page
        .eval(
            "return Array.from(document.querySelectorAll('[role=treeitem]')).map(e => e.getAttribute('data-status'));",
            vec![],
        )
        .await;
    assert!(
        statuses.as_array().unwrap().iter().all(|s| s == "Unknown"),
        "an inferred step was never observed to end, got {statuses}"
    );
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}
