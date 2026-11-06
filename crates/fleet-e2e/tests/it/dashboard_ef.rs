// SPDX-License-Identifier: AGPL-3.0-only
//! Screens E (history) and F (compare) in a real browser against the real server and simulated
//! agents (`fleet-loadgen`), in the style of [`super::dashboard_ab`]. Every wait is on a DOM
//! condition with a timeout, and every scenario ends by asserting the page reported no CSP
//! violation.
//!
//! The scripted failures come from `fleet_loadgen::fails`: with one device and two flows, the
//! agent at index 0 fails `flow-0-0` and the agent at index 1 passes everything. So two runs on
//! one commit that start at indices 1 and 0 pass and then fail the same flow.

use std::time::Duration;

use fleet_loadgen::Options;
use serde_json::{Value, json};

use crate::{
    browser::Browser,
    dashboard_ab::{PATIENCE, Page, count_of, has_text, q, stack},
    support::Stack,
};

/// A stack that stores no telemetry while a scenario runs: the checkpoint period is an hour, so
/// the first checkpoint (at start-up, before any sample) is the only one. A flow's condition
/// fingerprint includes the host's memory, which comes from stored telemetry; with none stored,
/// every run reads it as `unknown`, so runs of one machine class share a condition however the
/// agents' frames and the server's checkpoints interleave. There is no timing to get right.
async fn stack_without_host_telemetry() -> Stack {
    stack(|c| c.ingest.telemetry_flush_ms = 3_600_000).await
}

/// One finished CI run on `commit`: one agent, one device, `flows` flows. `first_agent` decides
/// which flows fail (see the module docs).
async fn run(
    stack: &Stack,
    run_id: &str,
    commit: &str,
    first_agent: usize,
    flows: u32,
    arch: &str,
) {
    fleet_loadgen::run(Options {
        server: stack.ws_url(),
        token: stack.key.clone(),
        run_id: run_id.into(),
        agents: 1,
        devices: 1,
        flows,
        job_keys: 1,
        batch: 4,
        pace: Duration::ZERO,
        deadline: Duration::from_secs(120),
        commit: Some(commit.into()),
        git_ref: Some("refs/heads/feat/pay".into()),
        arch: arch.into(),
        first_agent,
        ..Options::default()
    })
    .await
    .unwrap();
}

/// Polls the history until `done` holds for it (the server finalises on the last batch).
async fn history_until(stack: &Stack, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let h = stack.get("/api/v1/projects/app/history").await;
        if done(&h) {
            return h;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the history never got there: {h}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Sets a form control to `value` the way a person does: its value changes, then `change` fires.
async fn choose(page: &Page<'_>, selector: &str, value: &str) {
    page.eval(
        "const el = document.querySelector(arguments[0]); el.value = arguments[1];\
         el.dispatchEvent(new Event('change', { bubbles: true }));",
        vec![json!(selector), json!(value)],
    )
    .await;
}

/// The attempt of CI run `run_id` as the server lists it.
async fn attempt_of(stack: &Stack, run_id: &str) -> String {
    let attempts = stack.get("/api/v1/projects/app/attempts").await;
    attempts
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_key"] == format!("loadgen/app:{run_id}"))
        .unwrap_or_else(|| panic!("no attempt for {run_id}: {attempts}"))["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn history_marks_a_flow_that_passed_then_failed_on_one_commit_as_flaky_with_its_evidence() {
    let stack = stack_without_host_telemetry().await;
    run(&stack, "hist-1", "abc1234def5678", 1, 2, "x86_64").await;
    run(&stack, "hist-2", "abc1234def5678", 0, 2, "x86_64").await;
    // The server's verdict is the precondition: same commit, same conditions, both outcomes.
    let history = history_until(&stack, |h| {
        h["flows"]
            .as_array()
            .is_some_and(|f| f.iter().any(|v| v["flakiness"] == "confirmed"))
    })
    .await;
    let verdict = history["flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["flakiness"] == "confirmed")
        .unwrap();
    assert_eq!(verdict["flow"], "flows/flow-0-0.yaml");
    let passed = verdict["evidence"][0]["passed"][0].as_str().unwrap();
    let failed = verdict["evidence"][0]["failed"][0].as_str().unwrap();

    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://{}/history?project=app", stack.addr()))
        .await
        .unwrap();
    page.mark_loaded().await;

    // Four executions: two flows in two runs, newest first, each with its provenance (I4).
    page.wait(
        "four history rows",
        &format!("{} === 4", count_of(&q("history-row"))),
    )
    .await;
    let provenance = page
        .eval(
            &format!(
                "return Array.from(document.querySelectorAll('{} [role=img]')).map(e => e.getAttribute('aria-label'));",
                q("history-row")
            ),
            vec![],
        )
        .await;
    let provenance: Vec<String> = serde_json::from_value(provenance).unwrap();
    assert_eq!(provenance.len(), 4, "a provenance badge on every row");
    assert!(
        provenance.iter().all(|p| p == "Provenance: verified"),
        "the final report settled every one: {provenance:?}"
    );

    // The flaky flow is named with the two runs that show it, as links to their pages.
    let card = format!("{}[data-flow=\"flows/flow-0-0.yaml\"]", q("flaky-flow"));
    assert_eq!(page.count(&card).await, 1);
    assert_eq!(page.count(&q("flaky-flow")).await, 1, "only that flow");
    let card_text = page.texts(&card).await.concat();
    assert!(
        card_text.contains("Flaky (confirmed)")
            && card_text.contains("Passed and failed on commit abc1234 under the same conditions"),
        "{card_text}"
    );
    let links = page
        .eval(
            &format!(
                "return Array.from(document.querySelectorAll('{}')).map(a => [a.textContent.trim(), a.getAttribute('href')]);",
                q("flaky-evidence")
            ),
            vec![],
        )
        .await;
    assert_eq!(
        links,
        json!([
            ["Passed on abc1234", format!("/flows/{passed}?project=app")],
            ["Failed on abc1234", format!("/flows/{failed}?project=app")],
        ])
    );
    // Its two rows carry the marker; the steady flow's rows do not.
    let marked = |flow: &str| {
        format!(
            "{}[data-flow=\"{flow}\"] {}",
            q("history-row"),
            q("flaky-badge")
        )
    };
    assert_eq!(page.count(&marked("flows/flow-0-0.yaml")).await, 2);
    assert_eq!(page.count(&marked("flows/flow-0-1.yaml")).await, 0);
    // The failed execution carries its failure fingerprint (ADR-0009).
    let fingerprints = page.texts(&q("failure-fingerprint")).await;
    assert_eq!(fingerprints.len(), 1);
    assert!(fingerprints[0].starts_with("v1:"), "{fingerprints:?}");
    assert_eq!(page.count(&q("condition")).await, 4);

    // The evidence link opens the failed run's investigation page without a reload.
    page.eval(
        &format!(
            "document.querySelectorAll('{}')[1].click();",
            q("flaky-evidence")
        ),
        vec![],
    )
    .await;
    page.wait("screen D", &has_text(&q("flow-title"), "flow-0-0"))
        .await;
    assert!(page.never_reloaded().await);
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// Opens the only row's stats panel and checks its figures and its place in the address.
async fn open_and_check_stats(browser: &Browser, page: &Page<'_>) {
    // The flow's stats open beside the filters, and survive a reload too.
    page.eval(
        &format!("document.querySelector('{}').click();", q("open-stats")),
        vec![],
    )
    .await;
    page.wait("the stats panel", &count_of(&q("flow-stats")))
        .await;
    let stat = |id: &str| format!("{}[data-stat=\"{id}\"]", q("stat-tile"));
    page.wait(
        "the stats' counts",
        &format!("{} > 0", count_of(&stat("passed"))),
    )
    .await;
    assert_eq!(
        page.texts(&format!("{} span", stat("passed")))
            .await
            .first()
            .map(String::as_str),
        Some("1"),
        "flow-0-0 passed once"
    );
    assert_eq!(
        page.texts(&format!("{} span", stat("failed")))
            .await
            .first()
            .map(String::as_str),
        Some("1"),
        "and failed once (the outcome filter doesn't change the flow's stats)"
    );
    assert!(
        page.count(&format!("{} svg", q("duration-chart"))).await >= 1,
        "the duration chart is drawn"
    );
    assert_eq!(page.count(&q("failure-group")).await, 1);
    let query = browser
        .client
        .current_url()
        .await
        .unwrap()
        .query()
        .unwrap_or_default()
        .to_owned();
    assert!(query.contains("stats=flows%2Fflow-0-0.yaml"), "{query}");
    assert!(
        query.contains("outcome=failed"),
        "the filters stay: {query}"
    );
}

#[tokio::test]
async fn history_filters_live_in_the_url_and_survive_a_reload_and_a_flows_stats_open_beside_them() {
    let stack = stack_without_host_telemetry().await;
    run(&stack, "filt-1", "abc1234def5678", 1, 2, "x86_64").await;
    run(&stack, "filt-2", "abc1234def5678", 0, 2, "x86_64").await;
    history_until(&stack, |h| {
        h["items"].as_array().is_some_and(|i| i.len() == 4)
    })
    .await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://{}/history?project=app", stack.addr()))
        .await
        .unwrap();
    page.wait(
        "four history rows",
        &format!("{} === 4", count_of(&q("history-row"))),
    )
    .await;
    page.mark_loaded().await;

    // Filtering by outcome and by flow narrows the table and rewrites the address.
    choose(&page, "#filter-outcome", "failed").await;
    page.wait(
        "only the failure",
        &format!("{} === 1", count_of(&q("history-row"))),
    )
    .await;
    choose(&page, "#filter-flow", "flow-0-0").await;
    page.wait("still the failure", &count_of(&q("history-row")))
        .await;
    let url = browser.client.current_url().await.unwrap();
    let query = url.query().unwrap_or_default().to_owned();
    assert!(
        query.contains("project=app")
            && query.contains("outcome=failed")
            && query.contains("flow=flow-0-0"),
        "the filters are in the address: {query}"
    );
    assert!(page.never_reloaded().await, "editing filters never reloads");

    // A reload shows the same filters and the same rows.
    browser.client.refresh().await.unwrap();
    page.wait(
        "the filtered rows after the reload",
        &format!("{} === 1", count_of(&q("history-row"))),
    )
    .await;
    let controls = page
        .eval(
            "return [document.querySelector('#filter-outcome').value, document.querySelector('#filter-flow').value];",
            vec![],
        )
        .await;
    assert_eq!(controls, json!(["failed", "flow-0-0"]));
    assert_eq!(
        page.texts(&format!(
            "{} {}",
            q("history-row"),
            q("failure-fingerprint")
        ))
        .await
        .len(),
        1
    );
    page.mark_loaded().await;

    open_and_check_stats(&browser, &page).await;
    browser.client.refresh().await.unwrap();
    page.wait(
        "the stats panel after the reload",
        &count_of(&q("flow-stats")),
    )
    .await;
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// The flow only B ran is in its own section with the fact that explains it, and the summary
/// agrees with the sections.
async fn check_only_in_b_and_summary(page: &Page<'_>, section: &dyn Fn(&str) -> String) {
    let only_b = format!("{} {}", section("only-in-b"), q("compare-row"));
    assert_eq!(
        page.texts(&format!("{only_b} td:first-child")).await,
        ["flows/flow-0-2.yaml"]
    );
    assert_eq!(
        page.texts(&format!("{only_b} {}", q("explanation"))).await,
        ["Not executed in A."]
    );
    assert_eq!(
        page.count(&section("only-in-a")).await,
        0,
        "A ran nothing B didn't"
    );
    // The summary counts agree with the sections, and the flow counts show the changed size.
    let count = |id: &str| format!("{}[data-count=\"{id}\"] span", q("compare-count"));
    assert_eq!(
        page.texts(&count("newly-failing"))
            .await
            .first()
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        page.texts(&count("only-in-b"))
            .await
            .first()
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        page.texts(&q("compare-side-flows")).await,
        ["2 flows: 2 passed", "3 flows: 2 passed, 1 failed"]
    );
}

#[tokio::test]
async fn compare_explains_a_new_failure_by_the_changed_condition_and_lists_flows_on_one_side_only()
{
    let stack = stack_without_host_telemetry().await;
    // The same commit on an x86_64 runner (A), then on an aarch64 runner (B) with one more flow.
    run(&stack, "cmp-a", "abc1234def5678", 1, 2, "x86_64").await;
    run(&stack, "cmp-b", "abc1234def5678", 0, 3, "aarch64").await;
    let (a, b) = (
        attempt_of(&stack, "cmp-a").await,
        attempt_of(&stack, "cmp-b").await,
    );

    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://{}/compare?project=app", stack.addr()))
        .await
        .unwrap();
    page.wait("the attempt pickers", &count_of("#compare-b option[value]"))
        .await;
    page.mark_loaded().await;
    page.wait(
        "both attempts to be offered",
        "document.querySelectorAll('#compare-a option').length === 3",
    )
    .await;

    // Picking both attempts compares them, and the choice is in the address.
    choose(&page, "#compare-a", &a).await;
    choose(&page, "#compare-b", &b).await;
    page.wait("the comparison", &count_of(&q("comparison")))
        .await;
    let url = browser.client.current_url().await.unwrap();
    let query = url.query().unwrap_or_default().to_owned();
    assert!(
        query.contains(&format!("a={a}")) && query.contains(&format!("b={b}")),
        "{query}"
    );

    // flow-0-0 passed on A and failed on B: newly failing, and the evidence says why the runs
    // are not alike: the CPU architecture differs. Nothing else is claimed.
    let section = |id: &str| format!("{}[data-section=\"{id}\"]", q("compare-section"));
    let newly = section("newly-failing");
    assert_eq!(page.count(&newly).await, 1);
    let row = format!(
        "{newly} {}[data-flow=\"flows/flow-0-0.yaml\"]",
        q("compare-row")
    );
    assert_eq!(page.count(&row).await, 1);
    let why = page.texts(&format!("{row} {}", q("explanation"))).await;
    assert_eq!(
        why,
        ["Ran under different conditions: CPU architecture x86_64 in A, aarch64 in B."]
    );
    assert_eq!(
        page.texts(&format!("{row} {}", q("duration-caveat"))).await,
        ["The conditions differ, so this is not a like-for-like comparison."],
        "a duration is never read as a regression across different conditions"
    );
    // Each side shows its outcome with its provenance (I4).
    for side in ["A", "B"] {
        assert_eq!(
            page.count(&format!("{row} td[data-side=\"{side}\"] [role=img]"))
                .await,
            1,
            "{side}"
        );
    }

    check_only_in_b_and_summary(&page, &section).await;

    // A reload keeps both pickers and the comparison.
    browser.client.refresh().await.unwrap();
    page.wait(
        "the comparison after the reload",
        &count_of(&q("comparison")),
    )
    .await;
    let picked = page
        .eval(
            "return [document.querySelector('#compare-a').value, document.querySelector('#compare-b').value];",
            vec![],
        )
        .await;
    assert_eq!(picked, json!([a, b]));
    page.wait("the explanation again", &count_of(&q("explanation")))
        .await;
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    let _ = has_text;
    browser.quit().await;
}

#[tokio::test]
async fn history_discards_an_inflight_page_after_filters_change() {
    let stack = stack_without_host_telemetry().await;
    run(&stack, "page-race", "abc1234def5678", 0, 55, "x86_64").await;
    history_until(&stack, |h| h["next_cursor"].is_string()).await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://{}/history?project=app", stack.addr()))
        .await
        .unwrap();
    page.wait("first page and load more", "Array.from(document.querySelectorAll('button')).some(b => b.textContent.includes('Load more'))").await;
    page.eval("const original = window.fetch; window.__held = false; window.__firstHeld = false; window.fetch = async (...args) => { const url = String(args[0].url || args[0]); const response = await original(...args); if (url.includes('/history?') && url.includes('cursor=')) { window.__held = true; await new Promise(resolve => window.__release = resolve); window.__released = true; const text = response.text.bind(response); response.text = async () => { const body = await text(); window.__bodyRead = true; return body; }; } else if (!window.__firstHeld && url.includes('/history?') && url.includes('outcome=failed')) { window.__firstHeld = true; await new Promise(resolve => window.__releaseFirst = resolve); } return response; };", vec![]).await;
    page.eval("Array.from(document.querySelectorAll('button')).find(b => b.textContent.includes('Load more')).click();", vec![]).await;
    page.wait("old page held", "window.__held === true").await;
    choose(&page, "#filter-outcome", "failed").await;
    page.wait("filtered first page held", "window.__firstHeld === true")
        .await;
    page.eval("window.__releaseFirst();", vec![]).await;
    page.wait(
        "filtered rows",
        &format!(
            "{} > 0 && {} < 50",
            count_of(&q("history-row")),
            count_of(&q("history-row"))
        ),
    )
    .await;
    let before = page
        .eval(&format!("return {};", count_of(&q("history-row"))), vec![])
        .await;
    page.eval("window.__release();", vec![]).await;
    page.wait("old response delivered", "window.__released === true")
        .await;
    page.wait("old response body consumed", "window.__bodyRead === true")
        .await;
    browser.client.execute_async("const done = arguments[arguments.length - 1]; requestAnimationFrame(() => requestAnimationFrame(done));", vec![]).await.unwrap();
    let after = page
        .eval(&format!("return {};", count_of(&q("history-row"))), vec![])
        .await;
    assert_eq!(after, before, "old page must not be appended to new filter");
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}
