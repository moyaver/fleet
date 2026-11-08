// SPDX-License-Identifier: AGPL-3.0-only
//! The flow catalog and the richer history filters on screen E, end to end (issue #85). A run's
//! workspace names flows that have not run, with their tags, and its devices and CI carry the
//! facts the filters narrow on. The filters live in the URL: opening it sets them, changing one
//! rewrites it, and a never-run flow is listed as never run.

use fleet_loadgen::Options;

use crate::{
    browser::Browser,
    dashboard_ab::{Page, run_options, stack},
};

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario, read top to bottom: the catalog, then the filters in the address"
)]
async fn screen_e_lists_never_run_flows_and_its_filters_round_trip_through_the_url() {
    let stack = stack(|_| {}).await;
    // A run whose workspace catalog names `flows/corpus-0.yaml` (tag `smoke`) and
    // `flows/corpus-1.yaml` (tag `regression`), and whose devices report those strings as their
    // model. Its flows fail, so the history has executions to filter.
    fleet_loadgen::run(Options {
        corpus: vec!["smoke".into(), "regression".into()],
        devices: 2,
        flows: 2,
        ..run_options(&stack, "catalog-run")
    })
    .await
    .unwrap();
    stack.await_run_ended("job-0").await;

    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };

    // Flow and tag narrow the flows, so the flow that has never run is still listed, as never run.
    browser
        .client
        .goto(&format!(
            "http://{}/history?project=app&flow=flows%2F&tag=smoke",
            stack.addr()
        ))
        .await
        .unwrap();
    page.wait(
        "the flow catalog on screen E",
        "document.querySelector('[data-testid=\"catalog-row\"][data-flow=\"flows/corpus-0.yaml\"]')",
    )
    .await;
    let values = page
        .eval(
            "return [document.getElementById('filter-flow').value, document.getElementById('filter-tag').value];",
            vec![],
        )
        .await;
    assert_eq!(
        values,
        serde_json::json!(["flows/", "smoke"]),
        "the URL's filters are set in the controls"
    );
    let never = page
        .eval(
            "const row = document.querySelector('[data-testid=\"catalog-row\"][data-flow=\"flows/corpus-0.yaml\"]'); return [row.dataset.neverRun, row.querySelector('[data-testid=\"never-run\"]').textContent];",
            vec![],
        )
        .await;
    assert_eq!(
        never,
        serde_json::json!(["true", "Never run"]),
        "a never-run flow reads as never run"
    );
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());

    // Changing the tag rewrites the URL, and the catalog follows it.
    page.eval(
        "const el = document.getElementById('filter-tag'); el.value = 'regression'; el.dispatchEvent(new Event('change', { bubbles: true })); return null;",
        vec![],
    )
    .await;
    page.wait(
        "the URL to carry the new tag",
        "location.search.includes('tag=regression')",
    )
    .await;
    page.wait(
        "the catalog to follow the new tag",
        "document.querySelector('[data-testid=\"catalog-row\"][data-flow=\"flows/corpus-1.yaml\"]')",
    )
    .await;
    assert_eq!(
        page.eval("return document.querySelectorAll('[data-testid=\"catalog-row\"][data-flow=\"flows/corpus-0.yaml\"]').length;", vec![]).await,
        serde_json::json!(0),
        "the other tag's flow is no longer listed"
    );

    // Execution filters narrow the history by what ran on the device, and they are shareable.
    browser
        .client
        .goto(&format!(
            "http://{}/history?project=app&ci_provider=generic&device_model=smoke&outcome=failed",
            stack.addr()
        ))
        .await
        .unwrap();
    page.wait(
        "the history rows on screen E",
        "document.querySelector('[data-testid=\"history-row\"]')",
    )
    .await;
    let values = page
        .eval(
            "return [document.getElementById('filter-ci_provider').value, document.getElementById('filter-device_model').value, document.getElementById('filter-outcome').value];",
            vec![],
        )
        .await;
    assert_eq!(values, serde_json::json!(["generic", "smoke", "failed"]));
    let facts = page
        .eval(
            "return Array.from(document.querySelectorAll('[data-testid=\"run-fact\"]')).map(e => e.textContent).filter(t => t.startsWith('Device model'));",
            vec![],
        )
        .await;
    // Two runs of the flows reached the device, so the model shows on each run it ran on.
    let shown = facts.as_array().cloned().unwrap_or_default();
    assert!(
        !shown.is_empty() && shown.iter().all(|t| t == "Device model smoke"),
        "the device model is shown on the runs: {facts}"
    );
    page.eval(
        "const el = document.getElementById('filter-device_model'); el.value = 'regression'; el.dispatchEvent(new Event('change', { bubbles: true })); return null;",
        vec![],
    )
    .await;
    page.wait(
        "the URL to carry the new device model",
        "location.search.includes('device_model=regression')",
    )
    .await;
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}
