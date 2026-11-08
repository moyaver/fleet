// SPDX-License-Identifier: AGPL-3.0-only
//! Screens C (worker and device detail) and D (flow investigation) in a real browser against the
//! real server and simulated agents (`fleet-loadgen`), in the style of [`super::dashboard_ab`].
//! Every wait is on a DOM condition with a timeout, and every scenario ends by asserting the page
//! reported no CSP violation.

use fleet_loadgen::Options;
use serde_json::Value;

use crate::{
    browser::Browser,
    dashboard_ab::{
        Page, before_report, count_of, first_attempt, has_text, held_run_options, q, run_options,
        stack, start_run,
    },
    support::Stack,
};

/// A finished run: one agent, two devices, three flows each. The script fails exactly the first
/// flow of the first device (`flow-0-0`), and the agent reports host telemetry while it runs.
async fn finished_run(stack: &Stack, run_id: &str) -> Value {
    let opts = Options {
        pace: std::time::Duration::ZERO,
        ..run_options(stack, run_id)
    };
    fleet_loadgen::run(opts).await.unwrap();
    let attempt = first_attempt(stack).await;
    stack
        .get(&format!("/api/v1/projects/app/attempts/{attempt}"))
        .await
}

/// The snapshot's flow of this name.
fn flow_named<'a>(snapshot: &'a Value, name: &str) -> &'a Value {
    snapshot["flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .unwrap_or_else(|| panic!("no flow {name}"))
}

/// The worker the run's agent reported on.
pub(crate) async fn worker_of(stack: &Stack, snapshot: &Value) -> String {
    // The worker row is written with the first telemetry flush; the snapshot may predate it.
    let attempt = snapshot["attempt"]["id"].as_str().unwrap();
    let deadline = tokio::time::Instant::now() + crate::dashboard_ab::PATIENCE;
    loop {
        let fresh = stack
            .get(&format!("/api/v1/projects/app/attempts/{attempt}"))
            .await;
        if let Some(id) = fresh["agent_runs"][0]["worker_id"].as_str() {
            return id.to_owned();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the agent run has no worker"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn the_worker_page_labels_metrics_with_units_and_leaves_out_the_sensor_it_never_reports() {
    let stack = stack(|_| {}).await;
    let snapshot = finished_run(&stack, "worker-page").await;
    let worker = worker_of(&stack, &snapshot).await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!(
            "http://{}/workers/{worker}?project=app",
            stack.addr()
        ))
        .await
        .unwrap();
    page.mark_loaded().await;

    let chart = |metric: &str| format!("{}[data-metric=\"{metric}\"]", q("metric-chart"));
    let tile = |metric: &str| format!("{}[data-metric=\"{metric}\"]", q("health-tile"));
    page.wait("the host CPU chart", &count_of(&chart("host_cpu_pct")))
        .await;

    // AC4: every chart names its metric and says what it is measured in, and the latest values
    // read with the same units.
    let caption = |metric: &str| format!("{} figcaption", chart(metric));
    for (metric, label, unit) in [
        ("host_cpu_pct", "Host CPU", "(%)"),
        ("maestro_cpu_pct", "Maestro CPU", "(% of one core)"),
        ("host_mem_used_bytes", "Host memory used", "(bytes)"),
    ] {
        let text = page.texts(&caption(metric)).await.concat();
        assert!(
            text.contains(label) && text.contains(unit),
            "{metric}: the caption names the metric and its unit, got {text:?}"
        );
    }
    let value = |metric: &str| format!("{} {}", tile(metric), q("health-value"));
    // The health readout fills from the same fetch as the charts' windows: wait for it.
    page.wait(
        "the host CPU tile's value",
        &format!("{} === 1", count_of(&value("host_cpu_pct"))),
    )
    .await;
    assert_eq!(
        page.texts(&value("host_cpu_pct")).await,
        ["25%"],
        "the agent reported 25 % host CPU"
    );
    assert_eq!(page.texts(&value("host_mem_used_bytes")).await, ["4 GiB"]);
    // The health card is host-only: a process reading belongs to one run, so the worker has no
    // Maestro tile. Maestro's CPU is one chart per run, and the chart's accessible summary ends
    // with its latest reading in the same unit.
    assert_eq!(page.count(&tile("maestro_cpu_pct")).await, 0);
    assert_eq!(
        page.count(&chart("maestro_cpu_pct")).await,
        1,
        "one run, one chart"
    );
    page.wait(
        "the Maestro chart's latest reading",
        &format!(
            "Array.from(document.querySelectorAll('{} svg')).some(e => (e.getAttribute('aria-label') || '').includes('latest 80% of one core'))",
            chart("maestro_cpu_pct")
        ),
    )
    .await;

    // AC5: the agent never reports a thermal sensor, so the runner page has no cell and no chart
    // for it: a permanent absence is left out, not shown as a gap.
    assert_eq!(page.count(&tile("host_thermal_c")).await, 0);
    assert_eq!(page.count(&chart("host_thermal_c")).await, 0);

    assert!(page.never_reloaded().await);
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

#[tokio::test]
async fn a_failed_flows_page_shows_the_failure_and_every_claim_with_its_provenance() {
    let stack = stack(|_| {}).await;
    let snapshot = finished_run(&stack, "flow-page").await;
    let flow = flow_named(&snapshot, "flow-0-0");
    assert_eq!(flow["state"], "failed", "the script fails flow-0-0");
    let id = flow["id"].as_str().unwrap();
    let detail = stack.get(&format!("/api/v1/projects/app/flows/{id}")).await;
    let claims = detail["claims"].as_array().unwrap();
    assert!(claims.len() >= 2, "a live claim and the final report");
    let message = detail["failure"]["message"].as_str().unwrap().to_owned();

    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://{}/flows/{id}?project=app", stack.addr()))
        .await
        .unwrap();
    page.mark_loaded().await;

    page.wait("the flow's name", &has_text(&q("flow-title"), "flow-0-0"))
        .await;
    page.wait("its failure message", &count_of(&q("failure-message")))
        .await;
    assert_eq!(page.texts(&q("failure-message")).await, [message.as_str()]);

    // I4: every claim is listed, and each claim that is not settled says so in a badge. A verified
    // claim carries none of its own: the flow says Verified once (issue #149).
    page.wait(
        "every claim",
        &format!("{} === {}", count_of(&q("claim")), claims.len()),
    )
    .await;
    // A claim from the live console is provisional; the report's and the artifacts' are settled.
    let unsettled = claims
        .iter()
        .filter(|c| c["source"] == "live_console")
        .count();
    assert_eq!(
        page.count(&format!(
            "{} {} [role=img]",
            q("claim"),
            q("claim-provenance")
        ))
        .await,
        unsettled,
        "a badge on each claim that is not settled, and none on a settled one"
    );
    assert_eq!(
        page.count(&format!("{}[data-winning]", q("claim"))).await,
        1
    );
    let badges = page
        .eval(
            &format!(
                "return Array.from(document.querySelectorAll('{} {} [role=img]')).map(e => e.getAttribute('aria-label'));",
                q("claim"),
                q("claim-provenance")
            ),
            vec![],
        )
        .await;
    let badges: Vec<String> = serde_json::from_value(badges).unwrap();
    assert!(
        badges.contains(&"Provenance: provisional".to_owned()),
        "the live console's claim is provisional, got {badges:?}"
    );
    assert!(
        !badges.contains(&"Provenance: verified".to_owned()),
        "the final report's claim is settled and carries no badge, got {badges:?}"
    );
    // The header carries the winner's provenance too, and the steps are there in order.
    assert!(
        page.count(&format!("{} [role=img]", q("flow-header")))
            .await
            >= 1
    );
    page.wait(
        "the steps",
        &format!("{} >= 2", count_of("[role=treeitem]")),
    )
    .await;
    // The loadgen agents upload no evidence: the page says so instead of showing nothing. The card
    // waits for the artifact list, a separate request from the flow the steps come from.
    page.wait(
        "the evidence's empty state",
        &format!("{} === 1", count_of(&q("evidence-empty"))),
    )
    .await;
    let empty = page.texts(&q("evidence-empty")).await;
    assert!(
        empty
            .iter()
            .all(|t| t.starts_with("No evidence was uploaded for this flow")),
        "an honest empty state, got {empty:?}"
    );
    assert_eq!(empty.len(), 1);

    assert!(page.never_reloaded().await);
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

#[tokio::test]
async fn clicking_a_flow_on_the_pipeline_page_opens_its_investigation_page() {
    let stack = stack(|_| {}).await;
    let run = start_run(run_options(&stack, "click-through"));
    let attempt = first_attempt(&stack).await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!(
            "http://{}/p/app/pipelines/{attempt}",
            stack.addr()
        ))
        .await
        .unwrap();
    page.mark_loaded().await;
    page.wait("a flow row", &count_of(&format!("{} a", q("flow-row"))))
        .await;
    let name = page.texts(&format!("{} a", q("flow-row"))).await[0].clone();
    page.eval(
        &format!("document.querySelector('{} a').click();", q("flow-row")),
        vec![],
    )
    .await;

    page.wait("screen D", &has_text(&q("flow-title"), &name))
        .await;
    let url = browser.client.current_url().await.unwrap();
    assert!(
        url.path().starts_with("/flows/") && url.query() == Some("project=app"),
        "the address is the flow's page: {url}"
    );
    assert!(
        page.never_reloaded().await,
        "navigating is client-side: the page was never reloaded"
    );
    run.await.unwrap();
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

#[tokio::test]
async fn a_device_page_lists_its_flows_with_links_and_leaves_out_the_sensor_it_never_reports() {
    let stack = stack(|_| {}).await;
    let snapshot = finished_run(&stack, "device-page").await;
    let device = snapshot["devices"][0]["id"].as_str().unwrap().to_owned();
    let serial = snapshot["devices"][0]["serial"]
        .as_str()
        .unwrap()
        .to_owned();
    let on_device = snapshot["flows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["device_session_id"] == device.as_str())
        .count();
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!(
            "http://{}/devices/{device}?project=app",
            stack.addr()
        ))
        .await
        .unwrap();
    page.mark_loaded().await;

    page.wait("the device's name", &has_text(&q("device-title"), &serial))
        .await;
    page.wait(
        "its flows",
        &format!("{} === {on_device}", count_of(&q("device-flow"))),
    )
    .await;
    // AC5: no device reports a temperature (the agent has no thermal collector), so the device
    // page has no chart or tile for it, the same as the runner page.
    assert_eq!(
        page.count(&format!(
            "{}[data-metric=\"device_thermal_c\"]",
            q("metric-chart")
        ))
        .await,
        0
    );
    // Every flow row links to its investigation page, client-side.
    page.eval(
        &format!("document.querySelector('{} a').click();", q("device-flow")),
        vec![],
    )
    .await;
    page.wait("screen D", &count_of(&q("flow-title"))).await;
    assert!(page.never_reloaded().await);
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

#[tokio::test]
async fn a_flow_page_follows_its_run_live_from_provisional_to_verified() {
    let stack = stack(|_| {}).await;
    let hold = before_report();
    let run = start_run(held_run_options(&stack, "flow-live", &hold));
    let attempt = first_attempt(&stack).await;
    // flow-0-0 is the scripted failure; find its id as soon as the server has it.
    let deadline = tokio::time::Instant::now() + crate::dashboard_ab::PATIENCE;
    let id = loop {
        let snapshot = stack
            .get(&format!("/api/v1/projects/app/attempts/{attempt}"))
            .await;
        let found = snapshot["flows"]
            .as_array()
            .and_then(|f| f.iter().find(|f| f["name"] == "flow-0-0"))
            .and_then(|f| f["id"].as_str().map(str::to_owned));
        if let Some(id) = found {
            break id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "flow-0-0 never appeared"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://{}/flows/{id}?project=app", stack.addr()))
        .await
        .unwrap();
    page.mark_loaded().await;

    let header_badge = format!("{} [role=img]", q("flow-header"));
    let badge_is = |label: &str| {
        format!(
            "Array.from(document.querySelectorAll('{header_badge}')).some(e => e.getAttribute('aria-label') === 'Provenance: {label}')"
        )
    };
    // While the run is open the page shows the live console's claim: provisional, one claim.
    page.wait("a provisional outcome", &badge_is("provisional"))
        .await;
    // The run is released: the final report arrives, and the same page, without a reload, now
    // lists both claims and shows the outcome as verified.
    assert_eq!(
        page.count(&q("claim")).await,
        1,
        "only the live console's claim while the run is open"
    );
    hold.release();
    page.wait(
        "the final report's claim",
        &format!("{} === 2", count_of(&q("claim"))),
    )
    .await;
    page.wait("a verified outcome", &badge_is("verified")).await;
    run.await.unwrap();
    assert!(
        page.never_reloaded().await,
        "the page followed the run by itself"
    );
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// A finished run's failed flow (issue #148): its steps were settled from the final `commands.json`,
/// so the steps section reads verified, and the failed step is in the tree as failed.
#[tokio::test]
async fn a_failed_flows_steps_are_verified_once_its_run_has_ended() {
    let stack = stack(|_| {}).await;
    let snapshot = finished_run(&stack, "steps-settle").await;
    let id = flow_named(&snapshot, "flow-0-0")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://{}/flows/{id}?project=app", stack.addr()))
        .await
        .unwrap();
    page.mark_loaded().await;
    page.wait(
        "the steps",
        &format!("{} >= 2", count_of("[role=treeitem]")),
    )
    .await;
    let section_badge =
        "document.querySelector('.flow-steps .card-h [role=img]')?.getAttribute('aria-label')";
    page.wait(
        "the steps section is verified",
        &format!("{section_badge} === 'Provenance: verified'"),
    )
    .await;
    let failed = page
        .eval(
            "return Array.from(document.querySelectorAll('[role=treeitem]')).some(e => e.textContent.includes('Failed'));",
            vec![],
        )
        .await;
    assert_eq!(failed, Value::Bool(true), "the failed step is in the tree");
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}
