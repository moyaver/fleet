// SPDX-License-Identifier: AGPL-3.0-only
//! The M4 exit (PLAN §12): "a developer can follow a full run without CI logs (spec Phase 3
//! gate); acceptance criteria 2, 4, 5, 6 and 14 pass", proven in a real browser against the real
//! server, the real `fleet-agent` wrapping the fake Maestro, and `fleet-loadgen`.
//!
//! Where an earlier scenario already proves a criterion it is listed here rather than repeated;
//! this module adds only what was missing.
//!
//! | Spec §16 criterion | Proven by |
//! |---|---|
//! | 2. The browser updates during execution, without manual refresh. | [`ac02_a_real_agents_failing_run_is_followed_live_from_the_overview_to_the_failed_flow`] (real agent), `dashboard_ab::a_run_appears_on_the_overview_and_its_counts_move_without_a_reload`, `dashboard_ab::after_a_server_restart_the_page_resubscribes_and_keeps_updating`, `dashboard_cd::a_flow_page_follows_its_run_live_from_provisional_to_verified` |
//! | 4. Host and device metrics have accurate labels and units. | `dashboard_cd::the_worker_page_labels_metrics_with_units_and_shows_missing_thermal_as_unavailable` |
//! | 5. Missing thermal sensors show unavailable, never fake values. | the same worker-page test, and `dashboard_cd::a_device_page_lists_its_flows_with_links_and_shows_missing_sensors_as_unavailable` |
//! | 6. Flow progress is accurate or explicitly marked uncertain. | [`ac06_progress_counts_toward_the_planned_total_while_the_run_goes`] and [`ac06_a_real_agent_running_a_plan_shows_its_count_while_maestro_runs`] (known), the walkthrough and `dashboard_ab::pipeline_detail_shows_flows_with_provenance_and_says_when_progress_is_uncertain` (uncertain) |
//! | 14. The web experience remains usable with multiple simultaneous active runs. | [`ac14_ten_simultaneous_runs_stay_correct_and_the_page_stays_responsive`] (ten), `dashboard_ab::two_concurrent_runs_both_update_on_the_overview` |
//! | 12. Private test data is protected by company authentication. | [`ac12_signing_in_with_the_company_identity_provider_works_in_the_browser`] |
//! | Phase 3 gate: follow a full run without CI logs. | the same walkthrough |

use std::{
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    process::Output,
};

use fantoccini::Locator;
use fleet_core::policy::Role;
use fleet_loadgen::{Hold, HoldAt};
use fleet_server::config::{GroupRole, Login, Secret};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::{
    browser::Browser,
    dashboard_ab::{
        Page, before_report, count_of, first_attempt, has_text, held_run_options, number_in, q,
        stack, start_run,
    },
    idp::{CLIENT_ID, FakeIdp},
    support::{Stack, http, run_agent, run_agent_planned},
};

/// A stack that reads only with a company login: no anonymous reads, and the server's own address
/// is its public URL, which is where the identity provider sends the person back.
async fn stack_with_login(idp: &FakeIdp) -> Stack {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let issuer = idp.url();
    Stack::start(|c| {
        c.listen = SocketAddr::from(([127, 0, 0, 1], port));
        c.api.public_url = Some(format!("http://127.0.0.1:{port}"));
        c.login = Some(Login {
            issuer,
            client_id: CLIENT_ID.into(),
            client_secret: Some(Secret::from("shh")),
            scopes: vec!["profile".into(), "email".into()],
            group_claims: vec!["groups".into()],
            session_ttl_minutes: 60,
            group_roles: vec![GroupRole {
                group: "qa".into(),
                role: Role::Viewer,
                project: Some("app".into()),
            }],
        });
    })
    .await
}

/// Spec §16.12: "Private test data is protected by company authentication."
///
/// A person with no session is asked to sign in; the provider sends them back with a code
/// (PKCE, state, nonce), the dashboard shows them and their runs live, with a session cookie the
/// page's scripts cannot read; "Sign out" ends the session. A person whose groups grant nothing
/// is refused and sees nothing.
#[tokio::test]
async fn ac12_signing_in_with_the_company_identity_provider_works_in_the_browser() {
    let idp = FakeIdp::start(&["qa"]).await;
    let stack = stack_with_login(&idp).await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    let origin = format!("http://{}", stack.addr());

    // Unauthenticated: nothing is shown but the way in.
    browser.client.goto(&format!("{origin}/")).await.unwrap();
    page.wait("the sign-in notice", &count_of(&q("sign-in-required")))
        .await;
    assert_eq!(page.count(&q("active-row")).await, 0);

    // Whoever is in the wrong groups is refused by Fleet after the provider vouched for them.
    idp.set_groups(&["strangers"]);
    browser
        .client
        .find(Locator::Css(&format!("{} a", q("sign-in-required"))))
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    page.wait(
        "the refusal",
        "document.body.innerText.includes('your groups grant no access to Fleet')",
    )
    .await;
    // Whatever cookie is left over (the finished sign-in transaction) opens nothing.
    browser.client.goto(&format!("{origin}/")).await.unwrap();
    page.wait(
        "still the sign-in notice",
        &count_of(&q("sign-in-required")),
    )
    .await;
    assert_eq!(page.count(&q("whoami")).await, 0);

    // The right group: the same journey ends on the page they started from.
    idp.set_groups(&["qa"]);
    browser
        .client
        .find(Locator::Css(&format!("{} a", q("sign-in-required"))))
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    page.wait(
        "the signed-in overview",
        &has_text("h2", "No active pipelines"),
    )
    .await;
    page.wait("the person's name", &has_text(&q("whoami"), "Ada Lovelace"))
        .await;
    assert_eq!(browser.client.current_url().await.unwrap().path(), "/");

    // The session cookie is HttpOnly, Secure and SameSite=Lax, and scripts cannot see it.
    let cookies = browser.client.get_all_cookies().await.unwrap();
    let session = cookies
        .iter()
        .find(|c| c.name() == "__Host-fleet_session")
        .expect("a session cookie");
    assert_eq!(
        (
            session.http_only(),
            session.secure(),
            format!("{:?}", session.same_site())
        ),
        (Some(true), Some(true), "Some(Lax)".to_owned())
    );
    let visible = page.eval("return document.cookie;", vec![]).await;
    assert_eq!(visible, json!(""), "HttpOnly: no script can read it");
    page.mark_loaded().await;

    // Signed in, the live feed works with the cookie alone: a run appears without a reload.
    let hold = before_report();
    let run = start_run(held_run_options(&stack, "signed-in", &hold));
    page.wait("the run to appear live", &count_of(&q("active-row")))
        .await;
    hold.release();
    run.await.unwrap();
    assert!(page.never_reloaded().await);

    // Sign out ends the session on the server and on the page.
    browser
        .client
        .find(Locator::XPath("//button[normalize-space()='Sign out']"))
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    page.wait(
        "the sign-in notice after sign-out",
        &count_of(&q("sign-in-required")),
    )
    .await;
    assert_eq!(
        idp.token_requests().await,
        2,
        "one code redeemed per sign-in"
    );
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// The fake Maestro's gate (`FLEET_FAKE_GATE`): after replaying its log it waits for this file to
/// exist before it writes the report and exits, so the run stays open, with every outcome
/// provisional, exactly until the test calls [`Gate::open`].
#[derive(Debug)]
struct Gate(PathBuf);

impl Gate {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("fleet-e2e-gate-{}", uuid::Uuid::now_v7())))
    }

    fn open(&self) {
        std::fs::write(&self.0, "").unwrap();
    }
}

impl Drop for Gate {
    /// A test that fails while Maestro waits must not leave the agent waiting for the gate's
    /// timeout: opening it lets the run end and the test report its failure.
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "");
    }
}

/// Runs the real `fleet-agent` around the fake Maestro replaying `fixture`, against `stack`, on a
/// blocking thread. Maestro stops at `gate` between the live log and the final report.
fn start_agent(
    stack: &Stack,
    fixture: &'static str,
    run_id: &'static str,
    gate: &Gate,
    plan: Option<Value>,
) -> JoinHandle<Output> {
    let (ws, key) = (stack.ws_url(), stack.key.clone());
    let gate = gate.0.to_string_lossy().into_owned();
    tokio::task::spawn_blocking(move || {
        let env = [
            ("FLEET_SERVER", ws.as_str()),
            ("FLEET_TOKEN", key.as_str()),
            ("FLEET_RUN_ID", run_id),
            ("FLEET_REPOSITORY", "acme/app"),
            ("FLEET_JOB_KEY", "e2e"),
            ("FLEET_FAKE_GATE", gate.as_str()),
        ];
        match plan {
            Some(manifest) => run_agent_planned(fixture, &env, &manifest),
            None => run_agent(fixture, &env),
        }
    })
}

/// The overview shows the run by itself, without a plan the page says it cannot tell how far
/// along the run is, and the run's link opens its pipeline page.
async fn overview_shows_the_run_and_opens_it(page: &Page<'_>) {
    page.wait("the run on the overview", &count_of(&q("active-row")))
        .await;
    let progress = format!("{} {}", q("active-row"), q("progress-text"));
    page.wait(
        "uncertain progress on the overview",
        &format!("document.querySelector('{progress}').dataset.progress === 'uncertain'"),
    )
    .await;
    page.eval(
        &format!("document.querySelector('{} a').click();", q("active-row")),
        vec![],
    )
    .await;
}

/// Flows appear while the job is still running, from the live log, and say so.
async fn pipeline_page_shows_the_live_flows_before_the_report(page: &Page<'_>) {
    page.wait("the pipeline page", &count_of(&q("progress-text")))
        .await;
    page.wait("four flows", &format!("{} === 4", count_of(&q("flow-row"))))
        .await;
    page.wait(
        "the flows' outcomes from the live log",
        &format!(
            "{} === 3 && {} === 1",
            number_in(&q("count-passed")),
            number_in(&q("count-failed")),
        ),
    )
    .await;
    assert!(
        page.texts(&q("pipeline-result")).await[0].contains("Running"),
        "the run is still in progress while its flows already show"
    );
    page.wait(
        "live-log flows marked provisional",
        &format!(
            "Array.from(document.querySelectorAll('{flow} [role=img]')).every(e => e.getAttribute('aria-label') === 'Provenance: provisional')",
            flow = q("flow-row"),
        ),
    )
    .await;
    assert!(
        page.texts(&q("progress-text")).await[0].contains("progress uncertain"),
        "no plan: the page says progress is uncertain"
    );

    // Maestro told Fleet which four flows it runs, so the suite itself can count them.
    assert_eq!(
        page.texts(&q("suite-progress")).await,
        ["4 of 4 flows finished"]
    );
}

/// The report settles every flow; the failure's link leads to its page, which shows Maestro's
/// message and every claim with its provenance.
async fn pipeline_page_settles_and_leads_to_the_failure(page: &Page<'_>) {
    page.wait(
        "every flow verified by the report",
        &format!(
            "Array.from(document.querySelectorAll('{flow} [role=img]')).every(e => e.getAttribute('aria-label') === 'Provenance: verified')",
            flow = q("flow-row"),
        ),
    )
    .await;
    page.wait(
        "the final result",
        &format!(
            "document.querySelector('{}').textContent.trim() !== 'Running'",
            q("pipeline-result")
        ),
    )
    .await;

    // The failure leads to its page.
    page.wait("the failure listed", &count_of(&q("failure")))
        .await;
    page.eval(
        &format!("document.querySelector('{} a').click();", q("failure")),
        vec![],
    )
    .await;
    page.wait("the flow page", &has_text(&q("flow-title"), "delta-fails"))
        .await;
    page.wait("its failure message", &count_of(&q("failure-message")))
        .await;
    assert_eq!(
        page.texts(&q("failure-message")).await,
        ["Assertion is false: \"Fleet never shows this\" is visible"],
        "Maestro's own words, from the report"
    );
    // Three sources spoke: the live console, the flow's own artifacts, and the JUnit report.
    page.wait(
        "all three claims",
        &format!("{} === 3", count_of(&q("claim"))),
    )
    .await;
    let badges = page
        .eval(
            &format!(
                "return Array.from(document.querySelectorAll('{} {} [role=img]')).map(e => e.getAttribute('aria-label')).sort();",
                q("claim"),
                q("claim-provenance")
            ),
            vec![],
        )
        .await;
    assert_eq!(
        badges,
        json!([
            "Provenance: provisional",
            "Provenance: verified",
            "Provenance: verified"
        ]),
        "the live console's claim is provisional; the artifacts' and the report's are verified"
    );
    let winner = page.texts(&format!("{}[data-winning]", q("claim"))).await;
    assert_eq!(winner.len(), 1, "exactly one claim decided");
    assert!(
        winner[0].starts_with("Final JUnit report"),
        "the report decides, got {winner:?}"
    );
    page.wait(
        "the steps",
        &format!("{} >= 1", count_of("[role=treeitem]")),
    )
    .await;
}

/// Spec §16.2: "The browser updates during execution, without manual refresh."
/// And the Phase 3 gate: a developer can follow a full run without CI logs.
///
/// The real agent wraps the fake Maestro replaying the captured `suite` fixture (three flows
/// pass, `delta-fails` fails) and streams to the real server. In one browser session, never
/// reloaded: the overview shows the run appear, its link opens the pipeline page where the
/// flows appear while the run is still going (provisional, from the live log), and when Maestro
/// exits the report settles them as verified; the failure links to its flow page, which shows
/// Maestro's message and every claim with its provenance.
#[tokio::test(flavor = "multi_thread")]
async fn ac02_a_real_agents_failing_run_is_followed_live_from_the_overview_to_the_failed_flow() {
    let stack = stack(|_| {}).await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://{}/", stack.addr()))
        .await
        .unwrap();
    page.wait("the empty overview", &has_text("h2", "No active pipelines"))
        .await;
    page.mark_loaded().await;

    // Maestro stops after its log is complete and waits at the gate, so the live view can be
    // inspected for as long as it takes.
    let gate = Gate::new();
    let agent = start_agent(&stack, "suite", "walkthrough", &gate, None);

    overview_shows_the_run_and_opens_it(&page).await;

    pipeline_page_shows_the_live_flows_before_the_report(&page).await;
    // Maestro is let go: the JUnit report decides every flow, and the job's result is final.
    gate.open();
    let out = agent.await.unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "the agent keeps Maestro's exit code"
    );
    pipeline_page_settles_and_leads_to_the_failure(&page).await;
    assert!(
        page.never_reloaded().await,
        "from the overview to the failed flow without a manual refresh"
    );
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// Uploads a Fleet plan (ADR-0013) for the CI run `run_id` of `repository`.
async fn upload_plan(stack: &Stack, manifest: &Value, repository: &str, run_id: &str) {
    let (status, body) = http(
        stack.addr(),
        "POST",
        "/api/v1/plans",
        Some(&stack.key),
        &json!({"manifest": manifest,
                "pipeline": {"repository": repository, "run_id": run_id}})
        .to_string(),
    )
    .await;
    assert_eq!(status, 201, "{body}");
}

/// JS: the progress sentence, or "" while the card isn't on the page yet.
fn progress_sentence() -> String {
    format!(
        "((document.querySelector('{}') || {{}}).textContent || '')",
        q("progress-text")
    )
}

/// Spec §16.6: "Flow progress is accurate or explicitly marked uncertain."
///
/// The uncertain half (no plan) is proven by the walkthrough above and by
/// `dashboard_ab::pipeline_detail_shows_flows_with_provenance_and_says_when_progress_is_uncertain`.
/// This test proves the other half: when a Fleet plan names the six flows before the run starts,
/// the page reads "N of 6 flows finished" from the start, and (with the run held open after two
/// flows, after four and before the report) reads exactly 2, 4 and 6 of the same six, ending at
/// "6 of 6" when the report comes.
#[tokio::test(flavor = "multi_thread")]
async fn ac06_progress_counts_toward_the_planned_total_while_the_run_goes() {
    let stack = stack(|_| {}).await;
    let flows: Vec<String> = (0..2u32)
        .flat_map(|d| (0..3u32).map(move |f| format!("flows/flow-{d}-{f}.yaml")))
        .collect();
    let manifest = json!({"planId": "ac06", "shards": [{"id": 0, "flows": flows}]});
    upload_plan(&stack, &manifest, "loadgen/app", "planned").await;
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
    // Before any agent has connected, the plan already gives the total.
    page.wait(
        "the planned total",
        &format!("{}.includes('0 of 6 flows finished')", progress_sentence()),
    )
    .await;

    // Stops after two flows, after four, and before the report: at each the page must read that
    // many of the plan's six, and the total must not move.
    let hold = Hold::new([
        HoldAt::AfterFlows(2),
        HoldAt::AfterFlows(4),
        HoldAt::BeforeReport,
    ]);
    let run = start_run(held_run_options(&stack, "planned", &hold));
    for finished in [2, 4, 6] {
        page.wait(
            &format!("{finished} of 6 flows finished"),
            &format!(
                "{}.startsWith('{finished} of 6 flows finished')",
                progress_sentence()
            ),
        )
        .await;
        assert_eq!(
            page.eval(
                &format!(
                    "return document.querySelector('{}').dataset.progress;",
                    q("progress-text")
                ),
                vec![]
            )
            .await,
            json!("known")
        );
        hold.release();
    }
    run.await.unwrap();
    page.wait(
        "all six finished",
        &format!(
            "{}.startsWith('6 of 6 flows finished')",
            progress_sentence()
        ),
    )
    .await;
    assert_eq!(
        page.eval(
            &format!(
                "return document.querySelector('{}').dataset.progress;",
                q("progress-text")
            ),
            vec![]
        )
        .await,
        json!("known")
    );
    assert_eq!(
        page.eval(
            "return document.querySelector('progress[aria-label=\"Test progress\"]').value;",
            vec![]
        )
        .await,
        json!(100),
        "the bar is full"
    );
    assert!(page.never_reloaded().await);
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// Spec §16.6, with the real agent: `fleet-agent run --plan` reports the plan it runs, the
/// plan's four flows give the total before Maestro starts, and while Maestro is still running
/// the page counts finished flows out of that total.
#[tokio::test(flavor = "multi_thread")]
async fn ac06_a_real_agent_running_a_plan_shows_its_count_while_maestro_runs() {
    let stack = stack(|_| {}).await;
    let manifest = json!({"planId": "ac06-agent", "shards": [{"id": 0, "flows": [
        "flows/alpha.yaml", "flows/bravo.yaml", "flows/charlie.yaml", "flows/delta-fails.yaml"
    ]}]});
    upload_plan(&stack, &manifest, "acme/app", "planned-agent").await;
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
    page.wait(
        "the planned total",
        &format!("{}.includes('0 of 4 flows finished')", progress_sentence()),
    )
    .await;

    let gate = Gate::new();
    let agent = start_agent(&stack, "suite", "planned-agent", &gate, Some(manifest));
    page.wait(
        "the live log's four flows counted against the plan, with Maestro still running",
        &format!(
            "{}.startsWith('4 of 4 flows finished') \
             && (document.querySelector('{}') || {{}}).textContent.includes('Running')",
            progress_sentence(),
            q("pipeline-result"),
        ),
    )
    .await;
    gate.open();
    let out = agent.await.unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "the agent keeps Maestro's exit code"
    );
    page.wait(
        "the final counts",
        &format!(
            "{}.startsWith('4 of 4 flows finished') && {} === 3 && {} === 1",
            progress_sentence(),
            number_in(&q("count-passed")),
            number_in(&q("count-failed")),
        ),
    )
    .await;
    assert!(page.never_reloaded().await);
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// How many simultaneous runs AC14 uses.
const RUNS: usize = 10;

/// Longest an in-page script round trip may take while ten runs update the page.
const RESPONSIVE: std::time::Duration = std::time::Duration::from_secs(2);

/// Longest the page may block its main thread, by the browser's own long-task measure.
const LONG_TASK_MS: f64 = 1000.0;

/// How long the page has to catch up once the last run has finished.
const CONVERGE: std::time::Duration = std::time::Duration::from_secs(20);

/// Records every main-thread stall from now on in `window.__longest` (ms), after proving the
/// meter sees a deliberate 150 ms one.
async fn start_stall_meter(page: &Page<'_>) {
    page.eval(
        "window.__longest = 0; new PerformanceObserver(list => list.getEntries().forEach(e => \
         window.__longest = Math.max(window.__longest, e.duration))).observe({entryTypes: ['longtask']});",
        vec![],
    )
    .await;
    page.eval(
        "setTimeout(() => { const end = Date.now() + 150; while (Date.now() < end) {} }, 0);",
        vec![],
    )
    .await;
    page.wait("the stall to be measured", "window.__longest >= 100")
        .await;
    page.eval("window.__longest = 0;", vec![]).await;
}

/// With all runs active the page lists exactly them and answers scripts promptly; opening one
/// run is client-side, and so is coming back.
async fn ten_runs_are_listed_and_the_page_answers(browser: &Browser, page: &Page<'_>) {
    page.wait(
        "all ten runs active at once",
        &format!("{} === {RUNS}", count_of(&q("active-row"))),
    )
    .await;
    page.wait(
        "the tile to count all ten",
        &has_text(&format!("{} p", q("tile-active")), &RUNS.to_string()),
    )
    .await;
    // Every row holds its own run's live counts: eight flows, one failed, seven passed.
    page.wait(
        "every run's own live counts",
        &format!(
            "Array.from(document.querySelectorAll('{row}')).every(r => \
             Number(r.querySelector('{passed}').textContent) === 7 \
             && Number(r.querySelector('{failed}').textContent) === 1)",
            row = q("active-row"),
            passed = q("passed"),
            failed = q("failed"),
        ),
    )
    .await;
    let mut slowest = std::time::Duration::ZERO;
    for _ in 0..20 {
        let started = tokio::time::Instant::now();
        let rows = page.count(&q("active-row")).await;
        slowest = slowest.max(started.elapsed());
        assert!(rows <= RUNS, "never more rows than runs, got {rows}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(slowest < RESPONSIVE, "a script round trip took {slowest:?}");

    // Open one run while the others keep updating, then come back.
    page.eval(
        &format!("document.querySelector('{} a').click();", q("active-row")),
        vec![],
    )
    .await;
    page.wait("the run's own page", &count_of(&q("flow-row")))
        .await;
    let url = browser.client.current_url().await.unwrap();
    assert!(url.path().starts_with("/p/app/pipelines/"), "{url}");
    browser.client.back().await.unwrap();
    page.wait("the overview again", &count_of(&q("active-row")))
        .await;
    assert!(page.never_reloaded().await, "navigating is client-side");
}

/// Every run's own page holds that run's counts (eight flows: the loadgen's truth) and no
/// other's.
async fn each_runs_page_has_that_runs_truth(
    stack: &Stack,
    browser: &Browser,
    reports: &[fleet_loadgen::Report],
) {
    let page = Page {
        client: &browser.client,
    };
    let attempts = stack.get("/api/v1/projects/app/attempts?limit=50").await;
    for (i, report) in reports.iter().enumerate() {
        let failed = report.agents[0].failed_flows;
        assert_eq!(failed, 1, "the script fails exactly one flow per run");
        let key = format!("loadgen/app:ac14-{i}");
        let attempt = attempts
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["run_key"] == key.as_str())
            .unwrap_or_else(|| panic!("no attempt for {key}"))["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let url = format!("http://{}/p/app/pipelines/{attempt}", stack.addr());
        browser.client.goto(&url).await.unwrap();
        let passed = 8 - failed;
        page.wait(
            &format!("{key}: eight flows, {passed} passed and {failed} failed"),
            &format!(
                "{} === 8 && {} === {passed} && {} === {failed}",
                count_of(&q("flow-row")),
                number_in(&q("count-passed")),
                number_in(&q("count-failed")),
            ),
        )
        .await;
    }
}

/// Spec §16.14: "The web experience remains usable with multiple simultaneous active runs."
///
/// Ten loadgen runs (one agent, two devices, four flows each; the script fails exactly one flow
/// per run) are active at once while the overview is open and never reloaded. The page lists all
/// ten, answers scripts promptly and never blocks its main thread for long while they update,
/// navigates to one run and back, and converges: when the runs have finished, ten recent rows
/// and every run's own pipeline page reads the loadgen's truth (seven passed, one failed), none
/// mixed up with another's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac14_ten_simultaneous_runs_stay_correct_and_the_page_stays_responsive() {
    let stack = stack(|c| c.database.max_connections = 30).await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://{}/", stack.addr()))
        .await
        .unwrap();
    page.wait("the empty overview", &has_text("h2", "No active pipelines"))
        .await;
    page.mark_loaded().await;
    start_stall_meter(&page).await;

    // Every run stops with its flows ended live and the report not yet sent: all ten stay open
    // together for as long as the checks below take.
    let hold = before_report();
    let runs: Vec<_> = (0..RUNS)
        .map(|i| {
            let mut opts = held_run_options(&stack, &format!("ac14-{i}"), &hold);
            opts.flows = 4;
            start_run(opts)
        })
        .collect();
    ten_runs_are_listed_and_the_page_answers(&browser, &page).await;
    hold.release();
    let mut reports = Vec::new();
    for run in runs {
        reports.push(run.await.unwrap());
    }

    // The live page, never reloaded, catches up with every run finished.
    let converged = tokio::time::timeout(
        CONVERGE,
        page.wait(
            "ten finished runs and none active",
            &format!(
                "{} === {RUNS} && {} === 0",
                count_of(&q("recent-row")),
                count_of(&q("active-row"))
            ),
        ),
    )
    .await;
    assert!(
        converged.is_ok(),
        "the overview did not converge in {CONVERGE:?}"
    );
    let longest = page
        .eval("return window.__longest;", vec![])
        .await
        .as_f64()
        .unwrap();
    assert!(
        longest < LONG_TASK_MS,
        "the page blocked its main thread for {longest} ms"
    );
    assert!(page.never_reloaded().await);

    each_runs_page_has_that_runs_truth(&stack, &browser, &reports).await;
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}
