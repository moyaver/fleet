// SPDX-License-Identifier: AGPL-3.0-only
//! M5 exit (PLAN §12): "a failed run can be diagnosed after the runner is gone", proven in a real
//! browser with the real `fleet-agent` wrapping a fake Maestro that replays a captured fixture
//! (`fixtures/maestro/2.12.1/suite`: four flows, one failing, with its JUnit report, failure
//! screenshot, `commands.json` and session log), the real server on Postgres 18, and a usercontent
//! origin on its own host (`localhost`, while the dashboard is `127.0.0.1`: two origins, one
//! listener, routed by `Host` as a reverse proxy would).
//!
//! | Spec §16 criterion | Test |
//! |---|---|
//! | 7 Final results reconcile with Maestro's JUnit output | [`ac07_final_outcomes_equal_the_junit_report`] |
//! | 8 Failures link to actionable debugging evidence | [`ac08_a_failed_flow_links_its_screenshot_and_log_lines`] |
//! | 9 Test history remains accessible after the runner disappears | [`ac09_history_and_evidence_outlive_the_runner_and_a_server_restart`] |
//! | invariant I7 (uploaded HTML never runs on the app origin) | [`i7_an_uploaded_html_report_never_runs_on_the_app_origin`] |

use std::net::TcpListener;

use fleet_server::config::Secret;
use serde_json::{Value, json};

use crate::{
    artifacts::{get_as, temp},
    browser::Browser,
    dashboard_ab::{Page, count_of, first_attempt, has_text, q, stack},
    support::{self, Stack},
};

/// The fixture every test replays: three flows pass, `delta-fails` fails.
const FIXTURE: &str = "suite";
/// The flow that fails in [`FIXTURE`].
const FAILING: &str = "delta-fails";
/// The failing flow's assertion, as Maestro logs it.
const FAILING_STEP: &str = "Assert that \"Fleet never shows this\" is visible";

/// A stack whose dashboard is `http://127.0.0.1:<port>` and whose usercontent origin is
/// `http://localhost:<port>`; returns the port.
async fn evidence_stack() -> (Stack, u16) {
    evidence_stack_with_link_ttl(300).await
}

/// [`evidence_stack`] whose signed links last `link_ttl_seconds`.
async fn evidence_stack_with_link_ttl(link_ttl_seconds: u64) -> (Stack, u16) {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let stack = stack(|c| {
        c.listen = ([127, 0, 0, 1], port).into();
        c.api.public_url = Some(format!("http://127.0.0.1:{port}"));
        c.usercontent.url = Some(format!("http://localhost:{port}"));
        c.usercontent.signing_key = Some(Secret::from("0123456789abcdef0123456789abcdef-m5-exit"));
        c.usercontent.link_ttl_seconds = link_ttl_seconds;
        c.storage.path = temp("m5-blobs");
        c.artifacts.spool_dir = temp("m5-spool");
    })
    .await;
    (stack, port)
}

/// Runs the real agent to the end (Maestro exits 1: one flow fails); when this returns the agent
/// process is gone, its uploads are done, and the server has stored the run's end.
async fn run_agent_to_the_end(stack: &Stack) {
    let (ws, key) = (stack.ws_url(), stack.key.clone());
    let out = tokio::task::spawn_blocking(move || {
        support::run_agent(
            FIXTURE,
            &[
                ("FLEET_SERVER", &ws),
                ("FLEET_TOKEN", &key),
                ("FLEET_UPLOAD_TIMEOUT", "30"),
            ],
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(1), "Maestro's own exit status (I1)");
    // The default job key of a run without CI variables.
    stack.await_run_ended("job").await;
}

/// The `(name, outcome)` of each testcase of the fixture's JUnit report, sorted. Read from the
/// file itself, not through Fleet's parser: Maestro writes `SUCCESS` for a pass and `ERROR` for a
/// failure.
fn junit_outcomes() -> Vec<(String, String)> {
    let xml = std::fs::read_to_string(support::fixture(FIXTURE).join("run/report.xml")).unwrap();
    let attr = |tag: &str, name: &str| -> String {
        let key = format!(" {name}=\"");
        let from = tag.find(&key).unwrap() + key.len();
        tag[from..].split('"').next().unwrap().to_owned()
    };
    let mut cases: Vec<_> = xml
        .split("<testcase ")
        .skip(1)
        .map(|rest| {
            let tag = rest.split('>').next().unwrap();
            let outcome = match attr(tag, "status").as_str() {
                "SUCCESS" => "passed",
                "ERROR" | "FAILURE" => "failed",
                other => panic!("a JUnit status this test does not know: {other}"),
            };
            (attr(tag, "name"), outcome.to_owned())
        })
        .collect();
    cases.sort();
    cases
}

/// The id of the failing flow on the server (REST, not the browser).
async fn failing_flow(stack: &Stack) -> String {
    let snapshot = stack.latest_snapshot().await;
    snapshot["flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == FAILING)
        .unwrap_or_else(|| panic!("no flow {FAILING} in {snapshot}"))["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// `path` of an absolute URL, with its query.
fn path_of(url: &str) -> String {
    let url = url::Url::parse(url).unwrap();
    url.path().to_owned()
}

/// The flow rows of a pipeline page: `(name, state, provenance label)`, sorted by name.
async fn flow_rows(page: &Page<'_>) -> Vec<(String, String, String)> {
    let rows = page
        .eval(
            &format!(
                "return Array.from(document.querySelectorAll('{}')).map(r => [\
                 r.querySelector('a').textContent.trim(), r.dataset.state, \
                 (r.querySelector('{} [role=img]') || document.body).getAttribute('aria-label')]);",
                q("flow-row"),
                q("flow-provenance")
            ),
            vec![],
        )
        .await;
    let mut rows: Vec<(String, String, String)> = serde_json::from_value(rows).unwrap();
    rows.sort();
    rows
}

/// Spec §16 criterion 7: "Final results reconcile with Maestro's JUnit output."
///
/// The pipeline page of a real agent's run ends with exactly the testcases of Maestro's JUnit
/// report, with the same outcomes, each marked as verified: the final report decided them.
#[tokio::test]
async fn ac07_final_outcomes_equal_the_junit_report() {
    let (stack, port) = evidence_stack().await;
    run_agent_to_the_end(&stack).await;
    let attempt = first_attempt(&stack).await;

    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!(
            "http://127.0.0.1:{port}/p/app/pipelines/{attempt}"
        ))
        .await
        .unwrap();
    page.mark_loaded().await;

    let expected = junit_outcomes();
    assert_eq!(expected.len(), 4, "the fixture's report has four testcases");
    page.wait(
        "every testcase of the report as a verified flow",
        &format!(
            "{} === {} && Array.from(document.querySelectorAll('{} {} [role=img]')).every(\
             e => e.getAttribute('aria-label') === 'Provenance: verified')",
            count_of(&q("flow-row")),
            expected.len(),
            q("flow-row"),
            q("flow-provenance"),
        ),
    )
    .await;
    let shown: Vec<(String, String)> = flow_rows(&page)
        .await
        .into_iter()
        .map(|(name, state, provenance)| {
            assert_eq!(provenance, "Provenance: verified", "{name}");
            (name, state)
        })
        .collect();
    assert_eq!(
        shown, expected,
        "the dashboard's flows are the report's testcases, outcome for outcome"
    );

    assert!(page.never_reloaded().await);
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// The lines of the log viewer that carry `aria-current` (the highlighted ones).
async fn highlighted_log_lines(page: &Page<'_>) -> Vec<String> {
    page.texts(&format!("{} [aria-current=true]", q("log-viewer")))
        .await
}

/// The failing step's UI hierarchy is listed under its step, and its link names the step's file
/// (issue #74).
async fn check_hierarchy(page: &Page<'_>) {
    page.wait(
        "the failing step's UI hierarchy",
        &format!(
            "Array.from(document.querySelectorAll('{}')).some(e => e.textContent.includes('Failing step'))",
            q("evidence-hierarchy")
        ),
    )
    .await;
    let name = page
        .eval(
            &format!(
                "return document.querySelector('{} a').textContent;",
                q("evidence-hierarchy")
            ),
            vec![],
        )
        .await;
    assert_eq!(
        name.as_str(),
        Some("step-004-assertCondition-Fleet_never_shows_this.json"),
        "{name}"
    );
}

/// The failure screenshot is an image the browser really loaded, from the usercontent origin,
/// served sandboxed; the dashboard's own origin serves nothing of the kind.
async fn check_screenshot(page: &Page<'_>, stack: &Stack, port: u16) {
    let (app, usercontent) = (
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    );
    page.wait(
        "the failure screenshot to load",
        &format!(
            "Array.from(document.querySelectorAll('{} img')).some(i => i.complete && i.naturalWidth > 0)",
            q("screenshots")
        ),
    )
    .await;
    let src = page
        .eval(
            &format!(
                "return document.querySelector('{} img').src;",
                q("screenshots")
            ),
            vec![],
        )
        .await;
    let src = src.as_str().unwrap().to_owned();
    assert!(
        src.starts_with(&format!("{usercontent}/u/")),
        "the screenshot comes from the usercontent origin, not {app}: {src}"
    );
    let (status, head, body) =
        get_as(stack.addr(), &format!("localhost:{port}"), &path_of(&src)).await;
    assert_eq!(status, 200, "{head}");
    for header in [
        "content-type: image/png",
        "content-disposition: inline",
        "x-content-type-options: nosniff",
        "content-security-policy: sandbox;",
    ] {
        assert!(head.contains(header), "{header} in {head}");
    }
    assert!(
        !head.contains("set-cookie"),
        "no cookie from this origin: {head}"
    );
    let shot = support::fixture(FIXTURE).join(
        "run/debug/delta-fails/screenshots/step-004-assertCondition-Fleet_never_shows_this.png",
    );
    assert_eq!(body, std::fs::read(shot).unwrap(), "Maestro's own bytes");
    let (status, _, _) = get_as(stack.addr(), &format!("127.0.0.1:{port}"), &path_of(&src)).await;
    assert_eq!(
        status, 404,
        "the dashboard's origin does not serve artifacts"
    );
    // The dashboard's policy names that origin, and only in the two directives that need it.
    let (status, head, _) = get_as(stack.addr(), &format!("127.0.0.1:{port}"), "/").await;
    assert_eq!(status, 200);
    assert!(
        head.contains(&format!(
            "img-src 'self' blob: {usercontent}; connect-src 'self' {usercontent}; frame-ancestors 'none'"
        )),
        "{head}"
    );
    assert_eq!(head.matches(&usercontent).count(), 2, "{head}");
}

/// The log opens on the failing step of this flow, follows the step tree, and jumps back.
async fn check_log_jumps(page: &Page<'_>) {
    // The log opens on the failing step of *this* flow: alpha ran "Launch app" first too.
    page.wait(
        "the log scrolled to the failing step",
        &format!(
            "Array.from(document.querySelectorAll('{} [aria-current=true]')).some(\
             e => e.textContent.includes('{FAILING_STEP} RUNNING'))",
            q("log-viewer")
        ),
    )
    .await;
    let failing_line = highlighted_log_lines(page).await;
    assert_eq!(
        failing_line.len(),
        1,
        "one line is highlighted: {failing_line:?}"
    );
    assert!(
        failing_line[0].contains("CliConsoleListener"),
        "it is Maestro's step record: {failing_line:?}"
    );

    // Another step moves the log to that step's own lines, and "Jump to the failing step" returns.
    page.wait(
        "the steps",
        &format!("{} >= 4", count_of("[role=treeitem]")),
    )
    .await;
    // The step tree is a real ARIA tree: every item says its level, position and set size.
    for attribute in ["aria-level", "aria-posinset", "aria-setsize"] {
        assert_eq!(
            page.count(&format!("[role=treeitem][{attribute}]")).await,
            4,
            "four steps, each with {attribute}"
        );
    }
    page.eval(
        "Array.from(document.querySelectorAll('[role=treeitem]')).find(\
         e => e.textContent.includes('Launch app')).click();",
        vec![],
    )
    .await;
    page.wait(
        "the log at the launch step",
        &format!(
            "Array.from(document.querySelectorAll('{} [aria-current=true]')).some(\
             e => e.textContent.includes('Launch app') && e.textContent.includes('RUNNING'))",
            q("log-viewer")
        ),
    )
    .await;
    page.eval(
        &format!("document.querySelector('{}').click();", q("log-jump")),
        vec![],
    )
    .await;
    page.wait(
        "the log back at the failure",
        &format!(
            "Array.from(document.querySelectorAll('{} [aria-current=true]')).some(\
             e => e.textContent.includes('{FAILING_STEP} RUNNING'))",
            q("log-viewer")
        ),
    )
    .await;
}

/// One row per command of `commands.json`, the failed one marked.
async fn check_command_timings(page: &Page<'_>) {
    // Command timings: one row per command of commands.json, the failed one marked.
    let commands: Value = serde_json::from_str(
        &std::fs::read_to_string(
            support::fixture(FIXTURE).join("run/debug/delta-fails/commands.json"),
        )
        .unwrap(),
    )
    .unwrap();
    page.wait(
        "the command timings",
        &format!(
            "{} === {}",
            count_of(&q("command-row")),
            commands.as_array().unwrap().len()
        ),
    )
    .await;
    assert_eq!(
        page.count(&format!("{}[data-status=\"Failed\"]", q("command-row")))
            .await,
        1,
        "the failed command is marked"
    );
}

/// Spec §16 criterion 8: "Failures link to actionable debugging evidence."
///
/// The failed flow's page shows its failure screenshot (loaded from the usercontent origin, not
/// the dashboard's, under `CSP: sandbox`), the Maestro log scrolled to the failing step, and the
/// per-command timings; picking another step moves the log, and one click returns to the failure.
/// The browser reports no CSP violation: the dashboard's policy allows exactly that origin.
#[tokio::test]
async fn ac08_a_failed_flow_links_its_screenshot_and_log_lines() {
    let (stack, port) = evidence_stack().await;
    run_agent_to_the_end(&stack).await;
    let flow = failing_flow(&stack).await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://127.0.0.1:{port}/flows/{flow}?project=app"))
        .await
        .unwrap();
    page.mark_loaded().await;
    page.wait("the flow's name", &has_text(&q("flow-title"), FAILING))
        .await;

    check_screenshot(&page, &stack, port).await;
    check_log_jumps(&page).await;
    check_command_timings(&page).await;
    check_hierarchy(&page).await;

    assert!(page.never_reloaded().await);
    assert_eq!(
        browser.csp_violations().await,
        Vec::<String>::new(),
        "the image and the log were allowed by the CSP, not worked around"
    );
    browser.quit().await;
}

/// Presses the page's button with this label.
async fn click_button(page: &Page<'_>, label: &str) {
    page.eval(
        &format!(
            "Array.from(document.querySelectorAll('button')).find(b => b.textContent.trim() === '{label}').click();"
        ),
        vec![],
    )
    .await;
}

/// Issue #41: a flow page that stays open keeps its evidence viewers' state when the signed
/// links are renewed.
///
/// With links that last 61 seconds the page renews them about a second after each fetch, so
/// renewals keep coming while the test works. It zooms the screenshot to 200 %, types into the
/// log's search, then waits for the screenshot's link to change, which is a renewal that came
/// after that setup. The zoom, the search text and its count survive the renewal, and the page is
/// never reloaded.
#[tokio::test]
async fn evidence_viewers_keep_their_state_when_the_links_are_renewed() {
    let (stack, port) = evidence_stack_with_link_ttl(61).await;
    run_agent_to_the_end(&stack).await;
    let flow = failing_flow(&stack).await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://127.0.0.1:{port}/flows/{flow}?project=app"))
        .await
        .unwrap();
    page.mark_loaded().await;
    page.wait("the flow's name", &has_text(&q("flow-title"), FAILING))
        .await;
    let shot = format!("document.querySelector('{} img')", q("screenshots"));
    let search = "document.querySelector('input[aria-label=\"Search log\"]')";
    let status = "document.querySelector('section[aria-label=\"Log\"] span[aria-live=\"polite\"]')";
    page.wait(
        "the screenshot and the log search",
        &format!("Boolean({shot}) && Boolean({search})"),
    )
    .await;

    // Fit, 50, 100, 200 %.
    for _ in 0..3 {
        click_button(&page, "Zoom in").await;
    }
    page.wait(
        "the zoom at 200 %",
        &format!("{shot}.style.width === '200%'"),
    )
    .await;
    page.eval(
        &format!(
            "{search}.value = 'Assert'; {search}.dispatchEvent(new Event('input', {{ bubbles: true }}));"
        ),
        vec![],
    )
    .await;
    page.wait(
        "the search to count its matches",
        &format!("/^[1-9][0-9]* matches$/.test({status}.textContent.trim())"),
    )
    .await;
    let counted = page
        .eval(&format!("return {status}.textContent;"), vec![])
        .await;

    // The next renewal is after everything above, so it is the one that is checked.
    let link = page.eval(&format!("return {shot}.src;"), vec![]).await;
    page.wait(
        "a renewed screenshot link",
        &format!("{shot}.src !== {link}"),
    )
    .await;

    page.wait(
        "the zoom at 200 % after the renewal",
        &format!("{shot}.style.width === '200%'"),
    )
    .await;
    assert_eq!(
        page.eval(&format!("return {search}.value;"), vec![]).await,
        json!("Assert"),
        "the search text survives the renewal"
    );
    assert_eq!(
        page.eval(&format!("return {status}.textContent;"), vec![])
            .await,
        counted,
        "and so does its count"
    );
    assert!(page.never_reloaded().await);
    assert_eq!(
        browser.csp_violations().await,
        Vec::<String>::new(),
        "the renewed links are allowed by the CSP"
    );
    browser.quit().await;
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .fold(String::new(), |mut hex, b| {
            write!(hex, "{b:02x}").unwrap();
            hex
        })
}

/// Uploads `html` as the flow's Maestro HTML report over the artifact protocol, as an agent with
/// the project's key would (the real agent does not collect HTML reports yet).
async fn upload_html_report(stack: &Stack, flow: &str, html: &str) {
    let run = stack.latest_snapshot().await["agent_runs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let request = json!({
        "agent_run_id": run, "flow_execution_id": flow, "class": "html_report",
        "path": "report/index.html", "content_type": "text/html",
        "size": html.len(), "sha256": sha256_hex(html.as_bytes()),
    });
    let key = Some(stack.key.as_str());
    let (status, body) = support::http(
        stack.addr(),
        "POST",
        "/api/v1/agent/artifacts",
        key,
        &request.to_string(),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let grant: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(grant["disposition"], "upload", "{grant}");
    let (status, body) = support::http(
        stack.addr(),
        "PUT",
        grant["url"].as_str().unwrap(),
        key,
        html,
    )
    .await;
    assert_eq!(status, 204, "{body}");
    let (status, body) = support::http(
        stack.addr(),
        "POST",
        grant["complete_url"].as_str().unwrap(),
        key,
        "",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"available\""), "{body}");
}

/// A report that would take over the dashboard if it ran there.
const HOSTILE_REPORT: &str = "<!doctype html><html><head><title>report</title></head><body>\
    <script>document.title = 'pwned'; window.__ran = true; \
    fetch('/api/v1/projects').then(r => r.text()).then(t => { document.title = t; });</script>\
    <img src=x onerror=\"document.title = 'pwned'\"></body></html>";

/// Invariant I7: "The browser never talks to runners, and user-uploaded HTML never runs on the
/// app origin."
///
/// An uploaded HTML report is linked from the flow page, refused by the dashboard's own origin,
/// and served by the usercontent origin with `CSP: sandbox`; opened in the browser it gets an
/// opaque origin and none of its scripts run.
#[tokio::test]
async fn i7_an_uploaded_html_report_never_runs_on_the_app_origin() {
    let (stack, port) = evidence_stack().await;
    run_agent_to_the_end(&stack).await;
    let flow = failing_flow(&stack).await;
    upload_html_report(&stack, &flow, HOSTILE_REPORT).await;

    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://127.0.0.1:{port}/flows/{flow}?project=app"))
        .await
        .unwrap();
    page.wait("the report link", &count_of(&q("report-link")))
        .await;
    let link = page
        .eval(
            &format!(
                "return document.querySelector('{}').href;",
                q("report-link")
            ),
            vec![],
        )
        .await;
    let link = link.as_str().unwrap().to_owned();
    assert!(
        link.starts_with(&format!("http://localhost:{port}/u/")),
        "the report opens on the usercontent origin: {link}"
    );
    let rel = page
        .eval(
            &format!("return document.querySelector('{}').rel;", q("report-link")),
            vec![],
        )
        .await;
    assert!(
        rel.as_str().unwrap().contains("noopener"),
        "the report cannot reach the dashboard through window.opener: {rel}"
    );
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());

    // Through the dashboard's origin: refused, and not the report's bytes.
    let path = path_of(&link);
    let (status, _, body) = get_as(stack.addr(), &format!("127.0.0.1:{port}"), &path).await;
    assert_eq!(status, 404, "the app origin refuses artifact paths");
    assert!(
        !String::from_utf8_lossy(&body).contains("pwned"),
        "none of the report is served from the app origin"
    );
    // Through the usercontent origin: served, sandboxed, never sniffed.
    let (status, head, body) = get_as(stack.addr(), &format!("localhost:{port}"), &path).await;
    assert_eq!(status, 200, "{head}");
    assert!(head.contains("content-security-policy: sandbox;"), "{head}");
    assert!(head.contains("x-content-type-options: nosniff"), "{head}");
    assert!(head.contains("content-type: text/html"), "{head}");
    assert_eq!(body, HOSTILE_REPORT.as_bytes());
    // A link with one character of its signature changed opens nothing. The first character of
    // the signature (after the leading '/') carries six data bits; the last carries padding bits
    // too, so changing it can leave the decoded signature unchanged.
    let (head, last) = path.split_at(path.rfind('/').unwrap());
    let (head, signature) = head.split_at(head.rfind('/').unwrap());
    let first = if &signature[1..2] == "A" { "B" } else { "A" };
    let flipped = format!("{head}/{first}{}{last}", &signature[2..]);
    let (status, _, _) = get_as(stack.addr(), &format!("localhost:{port}"), &flipped).await;
    assert_eq!(status, 403, "a tampered link is refused");

    // In the browser the report is a document with an opaque origin and no scripts.
    browser.client.goto(&link).await.unwrap();
    let state = browser
        .client
        .execute(
            "return {origin: window.origin, title: document.title, ran: window.__ran === true};",
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        state,
        json!({"origin": "null", "title": "report", "ran": false}),
        "sandboxed: an opaque origin, and none of the report's scripts ran"
    );
    // And the same path on the app origin is just a 404 page: nothing runs there either.
    browser
        .client
        .goto(&format!("http://127.0.0.1:{port}{path}"))
        .await
        .unwrap();
    let state = browser
        .client
        .execute(
            "return {title: document.title, ran: window.__ran === true};",
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(state["ran"], false, "{state}");
    assert_ne!(state["title"], "pwned", "{state}");
    browser.quit().await;
}

/// Opens the history, finds the failed flow there and follows it to its page, then checks that
/// the page still shows the failure and its evidence: the screenshot loaded from the usercontent
/// origin and the log at the failing step.
async fn diagnose_from_history(port: u16, what: &str) {
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://127.0.0.1:{port}/history?project=app"))
        .await
        .unwrap();
    page.mark_loaded().await;

    // Screen E lists all four executions with the provenance of the final report.
    page.wait(
        &format!("{what}: the four executions in history"),
        &format!("{} === 4", count_of(&q("history-row"))),
    )
    .await;
    let rows = page
        .eval(
            &format!(
                "return Array.from(document.querySelectorAll('{}')).map(r => [\
                 r.querySelector('a').textContent.trim(), r.dataset.state, \
                 (r.querySelector('[role=img]') || document.body).getAttribute('aria-label')]);",
                q("history-row")
            ),
            vec![],
        )
        .await;
    let mut rows: Vec<(String, String, String)> = serde_json::from_value(rows).unwrap();
    rows.sort();
    let expected: Vec<(String, String, String)> = junit_outcomes()
        .into_iter()
        .map(|(name, outcome)| {
            let state = if outcome == "passed" {
                "Passed"
            } else {
                "Failed"
            };
            (name, state.to_owned(), "Provenance: verified".to_owned())
        })
        .collect();
    assert_eq!(
        rows, expected,
        "{what}: history is the report, outcome for outcome"
    );
    let fingerprints = page.texts(&q("failure-fingerprint")).await;
    assert_eq!(fingerprints.len(), 1, "{what}: only the failure has one");
    assert!(
        fingerprints[0].starts_with("v1:"),
        "{what}: {fingerprints:?}"
    );

    // The failed row opens the flow page, client-side.
    page.eval(
        &format!(
            "document.querySelector('{}[data-state=\"Failed\"] a').click();",
            q("history-row")
        ),
        vec![],
    )
    .await;
    page.wait(
        &format!("{what}: the failed flow's page"),
        &has_text(&q("flow-title"), FAILING),
    )
    .await;
    page.wait(
        &format!("{what}: the screenshot, loaded"),
        &format!(
            "Array.from(document.querySelectorAll('{} img')).some(\
             i => i.complete && i.naturalWidth > 0 && i.src.startsWith('http://localhost:{port}/u/'))",
            q("screenshots")
        ),
    )
    .await;
    page.wait(
        &format!("{what}: the log at the failing step"),
        &format!(
            "Array.from(document.querySelectorAll('{} [aria-current=true]')).some(\
             e => e.textContent.includes('{FAILING_STEP} RUNNING'))",
            q("log-viewer")
        ),
    )
    .await;
    assert!(page.never_reloaded().await, "{what}");
    assert_eq!(
        browser.csp_violations().await,
        Vec::<String>::new(),
        "{what}"
    );
    browser.quit().await;
}

/// Spec §16 criterion 9: "Test history remains accessible after the runner disappears."
///
/// The real agent runs to the end and exits: the runner is gone, nothing of it is left to ask.
/// History (screen E) and the failed flow's page still show the results and the evidence. Then the
/// server restarts too (everything it serves is in Postgres and the blob store) and the same
/// path through the dashboard works again.
#[tokio::test]
async fn ac09_history_and_evidence_outlive_the_runner_and_a_server_restart() {
    let (mut stack, port) = evidence_stack().await;
    run_agent_to_the_end(&stack).await;
    // The agent process has been waited for: no `fleet-agent` of this run is alive to answer.
    let stats = stack.latest_snapshot().await["agent_runs"][0]["state"].clone();
    assert_eq!(
        stats, "ended",
        "the run's agent said goodbye before it went"
    );

    diagnose_from_history(port, "after the runner is gone").await;

    stack.restart().await;
    diagnose_from_history(port, "after the server restarted").await;
}
