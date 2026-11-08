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
    publish::publish,
    support::{Stack, fixture},
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

    // The newest host sample: each host reading the agent reports, with its value or the Missing
    // mark and its reason (PLAN §6.5). The load generator takes no load, core, swap or pressure
    // reading, so those rows say they were not read in this sample; the thermal sensor is no row.
    page.wait(
        "the newest host sample's readings",
        &format!("{} === 10", count_of(&q("host-reading"))),
    )
    .await;
    page.wait(
        "every host reading shows a value or a reason",
        &format!(
            "[...document.querySelectorAll('{}')].every(r => r.querySelector(\"td[data-label='Value']\").textContent.trim() !== '')",
            q("host-reading")
        ),
    )
    .await;
    page.wait(
        "a host reading the sample does not take says why",
        &format!(
            "[...document.querySelectorAll('{}')].some(r => r.textContent.includes('not read in this sample'))",
            q("host-reading")
        ),
    )
    .await;
    let rows = page.texts(&q("host-reading")).await.concat();
    assert!(
        rows.contains("25%"),
        "the newest CPU reading is shown: {rows}"
    );
    assert!(
        !rows.contains("Host temperature"),
        "a sensor the agent never reports is no row"
    );

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
    // The newest sample: all nine readings, each with its value or its reason and the Missing mark
    // beside every reason (PLAN §6.5). The load generator takes no probe, so its readings say so.
    page.wait(
        "the newest sample's readings",
        &format!("{} === 9", count_of(&q("device-reading"))),
    )
    .await;
    page.wait(
        "every reading shows a value or a reason",
        &format!(
            "[...document.querySelectorAll('{}')].every(r => r.querySelector(\"td[data-label='Value']\").textContent.trim() !== '')",
            q("device-reading")
        ),
    )
    .await;
    page.wait(
        "a reading the sample does not take says why",
        &format!(
            "[...document.querySelectorAll('{}')].some(r => r.textContent.includes('not read in this sample'))",
            q("device-reading")
        ),
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
    // Settled rows carry no mark of their own: the section says verified once (issue #149). The
    // failed step is in the tree, settled from the file, and its row is unmarked.
    assert_eq!(
        page.count("[role=treeitem] [role=img]").await,
        0,
        "no settled row carries a mark"
    );
    let failed = page
        .eval(
            "return Array.from(document.querySelectorAll('[role=treeitem]')).filter(e => e.getAttribute('data-status') === 'Failed').length;",
            vec![],
        )
        .await;
    assert_eq!(
        failed,
        serde_json::json!(1),
        "the failed step is in the tree"
    );
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// A flow whose final `commands.json` lines up with none of its steps (issue #148): its steps stay
/// provisional. The section says so once, and every row carries its own provisional mark, the
/// failed step included (issue #149).
#[tokio::test]
async fn steps_the_file_does_not_settle_stay_provisional_and_each_row_says_so() {
    let stack = stack(|_| {}).await;
    let opts = Options {
        pace: std::time::Duration::ZERO,
        unaligned: true,
        ..run_options(&stack, "steps-unaligned")
    };
    fleet_loadgen::run(opts).await.unwrap();
    let attempt = first_attempt(&stack).await;
    let snapshot = stack
        .get(&format!("/api/v1/projects/app/attempts/{attempt}"))
        .await;
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
    page.wait(
        "the steps section is provisional",
        "document.querySelector('.flow-steps .card-h [role=img]')?.getAttribute('aria-label') === 'Provenance: provisional'",
    )
    .await;
    let rows = page
        .eval(
            "return Array.from(document.querySelectorAll('[role=treeitem]')).map(e => [e.getAttribute('data-status'), e.querySelector('[role=img]')?.getAttribute('aria-label') ?? null]);",
            vec![],
        )
        .await;
    let rows = rows.as_array().unwrap();
    assert!(rows.len() >= 2, "the flow's steps are listed");
    assert!(
        rows.iter()
            .all(|r| r[1] == serde_json::json!("Provenance: provisional")),
        "every unsettled row carries its own provisional mark, got {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r[0] == serde_json::json!("Failed")),
        "the failed step is among them, got {rows:?}"
    );
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// A copy of the captured run `name` in a new directory, every file of it.
fn copy_of_capture(name: &str) -> std::path::PathBuf {
    fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let dst = std::env::temp_dir().join(format!("fleet-e2e-capture-{}", uuid::Uuid::now_v7()));
    copy_tree(&fixture(name), &dst);
    dst
}

/// A copy of the captured `paths-none` run whose command names a directory of flows, not its one
/// flow file: the same log and files, with only the command's last word changed.
fn unnamed_copy_of_paths_none() -> std::path::PathBuf {
    let dst = copy_of_capture("paths-none");
    let meta_path = dst.join("meta.json");
    let mut meta: Value = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
    let command = meta["command"].as_array_mut().unwrap();
    *command.last_mut().unwrap() = Value::String("flows/".into());
    std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
    dst
}

/// A default-format Maestro output directory is replayed by `fleet-agent publish` (issue #148). Its
/// log names no flow, so the flow is the one its command names. `paths-none` names its one flow
/// file, so its steps settle. The same capture under a command that names a directory names no
/// flow: its `commands.json` is warned on its run, and the file creates no flow. The job's page says
/// so, and a run whose file finds its flow says nothing of the kind.
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "three published runs, their attempt and the job page in one scenario"
)]
async fn a_commands_file_no_flow_names_is_said_on_its_job_and_nowhere_else() {
    let stack = stack(|_| {}).await;
    let unnamed = unnamed_copy_of_paths_none();
    for (dir, job) in [
        (unnamed.clone(), "job-unnamed"),
        (fixture("paths-none"), "job-single"),
        (fixture("pass"), "job-named"),
    ] {
        let (ws, key) = (stack.ws_url(), stack.key.clone());
        let job_key = job.to_owned();
        let shown = dir.display().to_string();
        let out = tokio::task::spawn_blocking(move || {
            publish(
                &dir,
                &[
                    ("FLEET_SERVER", &ws),
                    ("FLEET_TOKEN", &key),
                    ("FLEET_RUN_ID", "unnamed"),
                    ("FLEET_JOB_KEY", &job_key),
                ],
            )
        })
        .await
        .unwrap();
        assert_eq!(out.status.code(), Some(0), "publishing {shown}");
    }
    let snap = stack.await_run_ended("job-unnamed").await;
    stack.await_run_ended("job-single").await;
    stack.await_run_ended("job-named").await;
    let _ = std::fs::remove_dir_all(&unnamed);
    let snap = stack
        .get(&format!(
            "/api/v1/projects/app/attempts/{}",
            snap["attempt"]["id"].as_str().unwrap()
        ))
        .await;
    let job_id = |key: &str| {
        snap["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["job_key"] == key)
            .unwrap_or_else(|| panic!("job {key}"))["id"]
            .clone()
    };
    let run_of = |key: &str| -> Value {
        let id = job_id(key);
        snap["agent_runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["job_id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(
        run_of("job-unnamed")["file_warnings"],
        serde_json::json!(["commands_flow_unnamed"]),
        "the run says its commands file names no flow the log started"
    );
    let unnamed_job = job_id("job-unnamed");
    assert!(
        snap["flows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["job_id"] != unnamed_job),
        "the file creates no flow (I3)"
    );
    assert!(
        run_of("job-named")["file_warnings"].is_null(),
        "a run whose files all find their flow keeps no file warning"
    );
    let single = job_id("job-single");
    assert!(
        run_of("job-single")["file_warnings"].is_null(),
        "the run names its one flow, so its file finds it"
    );
    assert!(
        snap["flows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["job_id"] == single && f["state"] == "passed"),
        "the command's one flow passes from its settled final file"
    );

    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    let attempt = snap["attempt"]["id"].as_str().unwrap();
    browser
        .client
        .goto(&format!(
            "http://{}/p/app/pipelines/{attempt}",
            stack.addr()
        ))
        .await
        .unwrap();
    page.mark_loaded().await;
    let said = "Array.from(document.querySelectorAll('[role=status]')).filter(e => e.textContent.includes('names a flow the log never started')).length";
    page.wait(
        "the job says its file names no flow",
        &format!("{said} === 1"),
    )
    .await;
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// Issue #148 follow-up: a run whose flow started and then the run ended without any verdict for it
/// (the captured `single-flow-no-format` run with its flow folder removed: its log has the flow's
/// steps, and nothing else). The flow is not shown as running: its state reads Unknown and its
/// provenance reads Missing, on its page, and the attempt's pipeline says the same.
#[tokio::test(flavor = "multi_thread")]
async fn a_flow_open_when_its_run_ended_shows_missing_not_running() {
    let stack = stack(|_| {}).await;
    let dir = copy_of_capture("single-flow-no-format");
    std::fs::remove_dir_all(dir.join("state/maestro/tests/2026-11-04_022836/pass")).unwrap();
    let shown = dir.display().to_string();
    let (ws, key) = (stack.ws_url(), stack.key.clone());
    let out = tokio::task::spawn_blocking(move || {
        publish(
            &dir,
            &[
                ("FLEET_SERVER", &ws),
                ("FLEET_TOKEN", &key),
                ("FLEET_RUN_ID", "open"),
                ("FLEET_JOB_KEY", "job-open"),
            ],
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(0), "publishing {shown}");
    let snap = stack.await_run_ended("job-open").await;
    let snap = stack
        .get(&format!(
            "/api/v1/projects/app/attempts/{}",
            snap["attempt"]["id"].as_str().unwrap()
        ))
        .await;
    let flow = snap["flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "pass")
        .expect("the run's flow is listed");
    assert_eq!(
        flow["state"], "unknown",
        "no verdict, and not running: {flow}"
    );
    assert_eq!(flow["finality"], "inferred", "{flow}");
    let id = flow["id"].as_str().unwrap().to_owned();

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
    page.wait("the flow's name", &has_text(&q("flow-title"), "pass"))
        .await;
    let header = q("flow-header");
    page.wait(
        "the flow's provenance is Missing",
        &format!(
            "{} === 1",
            count_of("[data-testid=\"flow-header\"] .pv[aria-label=\"Provenance: missing\"]")
        ),
    )
    .await;
    let running = format!(
        "Array.from(document.querySelectorAll('{header} .st')).filter(e => e.textContent.includes('Running')).length"
    );
    let unknown = format!(
        "Array.from(document.querySelectorAll('{header} .st')).filter(e => e.textContent.includes('Unknown')).length"
    );
    assert_eq!(
        page.eval(&format!("return {running};"), vec![]).await,
        serde_json::json!(0),
        "the header never says running"
    );
    assert_eq!(
        page.eval(&format!("return {unknown};"), vec![]).await,
        serde_json::json!(1),
        "the state reads Unknown"
    );
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}
