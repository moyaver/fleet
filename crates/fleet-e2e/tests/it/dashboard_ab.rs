// SPDX-License-Identifier: AGPL-3.0-only
//! Screens A (overview) and B (pipeline detail) in a real browser against the real server and
//! simulated agents (`fleet-loadgen`). Every wait is on a DOM condition with a timeout, never a
//! fixed sleep, and every scenario ends by asserting the page reported no CSP violation.
//!
//! The dashboard is `crates/fleet-web/dist` (`cargo xtask web`). The server allows anonymous
//! reads here: sign-in has its own tests.

use std::time::Duration;

use fantoccini::Client;
use fleet_loadgen::Options;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::{browser::Browser, support::Stack};

/// How long any one condition may take to become true.
const PATIENCE: Duration = Duration::from_secs(60);

/// A stack with anonymous reads, which is how a browser without a login reads in development.
async fn stack(tweak: impl FnOnce(&mut fleet_server::config::Config)) -> Stack {
    Stack::start(|c| {
        c.api.anonymous_read = true;
        tweak(c);
    })
    .await
}

/// One simulated run, stretched over time so the page can be watched while it changes.
fn run_options(stack: &Stack, run_id: &str) -> Options {
    Options {
        server: stack.ws_url(),
        token: stack.key.clone(),
        run_id: run_id.into(),
        agents: 1,
        devices: 2,
        flows: 3,
        job_keys: 1,
        batch: 4,
        pace: Duration::from_millis(350),
        deadline: Duration::from_secs(120),
        ..Options::default()
    }
}

fn start_run(opts: Options) -> JoinHandle<fleet_loadgen::Report> {
    tokio::spawn(async move { fleet_loadgen::run(opts).await.unwrap() })
}

/// The DOM, polled by script.
struct Page<'a> {
    client: &'a Client,
}

impl Page<'_> {
    async fn eval(&self, script: &str, args: Vec<Value>) -> Value {
        self.client.execute(script, args).await.unwrap()
    }

    /// Waits until the JS expression is truthy; on timeout fails with what the page said.
    async fn wait(&self, what: &str, expression: &str) {
        let script = format!("return Boolean({expression});");
        let outcome = tokio::time::timeout(PATIENCE, async {
            loop {
                if self.eval(&script, vec![]).await == json!(true) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        if outcome.is_err() {
            let body = self
                .eval("return document.body.innerText.slice(0, 3000);", vec![])
                .await;
            panic!("timed out waiting for {what} ({expression}); the page says:\n{body}");
        }
    }

    async fn texts(&self, css: &str) -> Vec<String> {
        let v = self
            .eval(
                "return Array.from(document.querySelectorAll(arguments[0])).map(e => e.textContent.trim());",
                vec![json!(css)],
            )
            .await;
        serde_json::from_value(v).unwrap()
    }

    async fn count(&self, css: &str) -> usize {
        self.eval(
            "return document.querySelectorAll(arguments[0]).length;",
            vec![json!(css)],
        )
        .await
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap()
    }

    /// Marks this document: if the page reloads, the mark is gone.
    async fn mark_loaded(&self) {
        self.eval("window.__noReload = true;", vec![]).await;
    }

    async fn never_reloaded(&self) -> bool {
        self.eval("return window.__noReload === true;", vec![])
            .await
            == json!(true)
    }
}

/// Waits for the project's first attempt on the server (REST, not the browser) and returns its id.
async fn first_attempt(stack: &Stack) -> String {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let attempts = stack.get("/api/v1/projects/app/attempts").await;
        if let Some(id) = attempts.get(0).and_then(|a| a["id"].as_str()) {
            return id.to_owned();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no attempt appeared on the server"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn q(testid: &str) -> String {
    format!("[data-testid=\"{testid}\"]")
}

/// JS: the number in the first element matching `selector`.
fn number_in(selector: &str) -> String {
    format!("Number((document.querySelector('{selector}') || {{}}).textContent)")
}

/// JS: how many elements match.
fn count_of(selector: &str) -> String {
    format!("document.querySelectorAll('{selector}').length")
}

/// JS: some element matching `selector` has exactly this text.
fn has_text(selector: &str, text: &str) -> String {
    format!(
        "Array.from(document.querySelectorAll('{selector}')).some(e => e.textContent.trim() === '{text}')"
    )
}

#[tokio::test]
async fn a_run_appears_on_the_overview_and_its_counts_move_without_a_reload() {
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

    let run = start_run(run_options(&stack, "overview-live"));

    page.wait("the run to appear", &count_of(&q("active-row")))
        .await;
    let active = q("active-row");
    page.wait(
        "a flow to pass while the run is still active",
        &format!("{} >= 1", number_in(&format!("{active} {}", q("passed")))),
    )
    .await;
    // No plan was given, so the page must say it cannot tell how far along the run is (AC6).
    let progress = page
        .texts(&format!("{active} {}", q("progress-text")))
        .await;
    assert!(
        progress.iter().all(|t| t.contains("progress uncertain")),
        "an unplanned run is marked uncertain, got {progress:?}"
    );
    page.wait(
        "the finished run to move to recently finished",
        &format!(
            "{} === 1 && {} === 0",
            count_of(&q("recent-row")),
            count_of(&q("active-row"))
        ),
    )
    .await;
    let recent = q("recent-row");
    // The finished job carries the provenance of its outcome (I4): the script's failing flow ends
    // with a non-zero exit, which the agent observed.
    page.wait(
        "a provenance on the finished job",
        &format!("document.querySelector('{recent} [role=img]').getAttribute('aria-label') === 'Provenance: observed'"),
    )
    .await;
    assert!(
        page.never_reloaded().await,
        "the page updated itself; it was never reloaded"
    );
    run.await.unwrap();
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

#[tokio::test]
async fn pipeline_detail_shows_flows_with_provenance_and_says_when_progress_is_uncertain() {
    let stack = stack(|_| {}).await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    let run = start_run(run_options(&stack, "pipeline-detail"));
    let attempt = first_attempt(&stack).await;
    browser
        .client
        .goto(&format!(
            "http://{}/p/app/pipelines/{attempt}",
            stack.addr()
        ))
        .await
        .unwrap();
    page.mark_loaded().await;

    page.wait("flows on the page", &count_of(&q("flow-row")))
        .await;
    page.wait("the live connection", &has_text(&q("connection"), "Live"))
        .await;
    let progress = q("progress-text");
    page.wait(
        "uncertain progress",
        &format!("document.querySelector('{progress}').dataset.progress === 'uncertain'"),
    )
    .await;
    let text = page.texts(&progress).await;
    assert!(
        text[0].contains("uncertain"),
        "no plan: the text says so, got {text:?}"
    );

    run.await.unwrap();
    page.wait(
        "every flow to be settled by the report",
        &format!(
            "{n} > 0 && Array.from(document.querySelectorAll('{flow} [role=img]')).every(e => e.getAttribute('aria-label') === 'Provenance: verified')",
            n = count_of(&q("flow-row")),
            flow = q("flow-row"),
        ),
    )
    .await;
    // Every flow row carries a badge (I4), and the job's result is no longer "Running".
    assert_eq!(
        page.count(&format!("{} {}", q("flow-row"), q("flow-provenance")))
            .await,
        page.count(&q("flow-row")).await
    );
    page.wait(
        "the result to settle",
        &format!(
            "document.querySelector('{}').textContent.trim() !== 'Running'",
            q("pipeline-result")
        ),
    )
    .await;
    // The scripted failure (agent 0, flow-0-0) is listed, with a link to its page.
    page.wait("the failure", &count_of(&q("failure"))).await;
    let link = page
        .eval(
            &format!(
                "return document.querySelector('{} a').getAttribute('href');",
                q("failure")
            ),
            vec![],
        )
        .await;
    assert!(
        link.as_str().unwrap().starts_with("/flows/")
            && link.as_str().unwrap().ends_with("?project=app"),
        "the failure links to its investigation page: {link}"
    );
    // The timeline is there, with its accessible table alternative.
    assert!(
        page.count("section[aria-label=\"Flow timeline\"] canvas")
            .await
            >= 1
    );
    assert!(page.never_reloaded().await);
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

#[tokio::test]
async fn after_a_server_restart_the_page_resubscribes_and_keeps_updating() {
    let mut stack = stack(|_| {}).await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    let mut opts = run_options(&stack, "restart");
    opts.flows = 5;
    opts.pace = Duration::from_millis(450);
    let run = start_run(opts);
    let attempt = first_attempt(&stack).await;
    browser
        .client
        .goto(&format!(
            "http://{}/p/app/pipelines/{attempt}",
            stack.addr()
        ))
        .await
        .unwrap();
    page.mark_loaded().await;
    page.wait("the live connection", &has_text(&q("connection"), "Live"))
        .await;
    page.wait(
        "a first flow to finish",
        &format!("{} >= 1", number_in(&q("count-passed"))),
    )
    .await;
    let before = page.texts(&q("count-passed")).await[0]
        .parse::<u64>()
        .unwrap();

    stack.restart().await;

    // The page reconnects by itself, takes a new snapshot and goes on: more flows finish than
    // had when the server went away, and the run ends with the report's verdict.
    page.wait(
        "more flows to finish after the restart",
        &format!("{} > {before}", number_in(&q("count-passed"))),
    )
    .await;
    page.wait(
        "the live connection again",
        &has_text(&q("connection"), "Live"),
    )
    .await;
    run.await.unwrap();
    page.wait(
        "the final verdict",
        &format!(
            "document.querySelector('{}').textContent.trim() !== 'Running'",
            q("pipeline-result")
        ),
    )
    .await;
    page.wait(
        "all ten flows settled",
        &format!(
            "{n} === 10 && Array.from(document.querySelectorAll('{flow} [role=img]')).every(e => e.getAttribute('aria-label') === 'Provenance: verified')",
            n = count_of(&q("flow-row")),
            flow = q("flow-row"),
        ),
    )
    .await;
    assert!(page.never_reloaded().await, "no manual refresh was needed");
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

#[tokio::test]
async fn an_agent_that_goes_quiet_turns_stale_and_then_disconnected() {
    let stack = stack(|c| {
        c.liveness.stale_after_ms = 4_000;
        c.liveness.sweep_interval_ms = 500;
    })
    .await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    // The first flow has started and then the agent says nothing more.
    let agent = fleet_loadgen::quiet(&run_options(&stack, "quiet"), 6)
        .await
        .unwrap();
    let attempt = first_attempt(&stack).await;
    browser
        .client
        .goto(&format!(
            "http://{}/p/app/pipelines/{attempt}",
            stack.addr()
        ))
        .await
        .unwrap();
    page.mark_loaded().await;

    let liveness = q("run-liveness");
    page.wait("a run, online", &has_text(&liveness, "Online"))
        .await;
    page.wait("the run to turn stale", &has_text(&liveness, "Stale"))
        .await;
    // What was running is no longer shown as live: its badge says stale too.
    page.wait(
        "the running flow's badge to go stale",
        &format!(
            "Array.from(document.querySelectorAll('{flow}[data-state=running] [role=img]')).some(e => e.getAttribute('aria-label') === 'Provenance: stale')",
            flow = q("flow-row"),
        ),
    )
    .await;

    agent.disconnect().await;
    page.wait(
        "the run to show disconnected",
        &has_text(&liveness, "Disconnected"),
    )
    .await;

    // The overview agrees about the runner.
    browser
        .client
        .goto(&format!("http://{}/", stack.addr()))
        .await
        .unwrap();
    page.wait(
        "the runner shown disconnected",
        &has_text(&q("runner-liveness"), "Disconnected"),
    )
    .await;
    page.wait(
        "the runner counts to agree",
        &has_text(&q("runner-counts"), "0 stale · 1 disconnected"),
    )
    .await;
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

#[tokio::test]
async fn two_concurrent_runs_both_update_on_the_overview() {
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

    let first = start_run(run_options(&stack, "concurrent-a"));
    let second = start_run(run_options(&stack, "concurrent-b"));

    page.wait(
        "both runs to be active",
        &format!("{} === 2", count_of(&q("active-row"))),
    )
    .await;
    page.wait(
        "the tile to count both",
        &has_text(&format!("{} p", q("tile-active")), "2"),
    )
    .await;
    // Both rows move: each has a passed flow while both are still active.
    page.wait(
        "both rows to show a passed flow",
        &format!(
            "{rows} === 2 && Array.from(document.querySelectorAll('{row} {passed}')).every(e => Number(e.textContent) >= 1)",
            rows = count_of(&q("active-row")),
            row = q("active-row"),
            passed = q("passed"),
        ),
    )
    .await;
    let attempts = page
        .eval(
            &format!(
                "return Array.from(document.querySelectorAll('{}')).map(e => e.dataset.attempt);",
                q("active-row")
            ),
            vec![],
        )
        .await;
    let attempts: Vec<String> = serde_json::from_value(attempts).unwrap();
    assert_ne!(attempts[0], attempts[1], "two different pipelines");

    first.await.unwrap();
    second.await.unwrap();
    page.wait(
        "both to finish",
        &format!(
            "{} === 2 && {} === 0",
            count_of(&q("recent-row")),
            count_of(&q("active-row"))
        ),
    )
    .await;
    assert!(page.never_reloaded().await);
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// A signed-in session straight in the database (the sign-in flow has its own tests in
/// `fleet-server`): returns the cookie value of a viewer on every project.
async fn sign_in_session(stack: &Stack) -> String {
    let url = stack.config.database.url.as_ref().unwrap().expose();
    let store = fleet_store::Store::connect_lazy(url, fleet_store::PoolOptions::default()).unwrap();
    let user = store
        .upsert_user(
            "https://idp.example",
            "ada",
            Some("ada@example.com"),
            Some("Ada"),
        )
        .await
        .unwrap();
    let data = json!({
        "grants": [{"project": null, "role": "viewer"}],
        "csrf": "e2e-csrf-token", "email": "ada@example.com", "name": "Ada"
    });
    let expires = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    store
        .create_session(Some(user), &data, expires)
        .await
        .unwrap()
        .expose()
        .to_owned()
}

#[tokio::test]
async fn without_a_login_the_dashboard_links_to_sign_in_and_with_one_it_signs_out_with_the_csrf_token()
 {
    // No anonymous reads and no viewer token in the browser: reading needs a login.
    let stack = Stack::start(|_| {}).await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    let origin = format!("http://{}", stack.addr());
    browser.client.goto(&format!("{origin}/")).await.unwrap();
    page.wait("the sign-in notice", &count_of(&q("sign-in-required")))
        .await;
    let link = page
        .eval(
            &format!(
                "return document.querySelector('{} a').getAttribute('href');",
                q("sign-in-required")
            ),
            vec![],
        )
        .await;
    assert_eq!(link, json!("/api/v1/auth/login?return_to=%2F"));
    // A deep link returns to itself: the path is escaped into `return_to`.
    let deep = "/p/app/pipelines/00000000-0000-7000-8000-000000000001";
    browser
        .client
        .goto(&format!("{origin}{deep}"))
        .await
        .unwrap();
    page.wait("the header's sign-in link", &count_of(&q("sign-in")))
        .await;
    let link = page
        .eval(
            &format!(
                "return document.querySelector('{}').getAttribute('href');",
                q("sign-in")
            ),
            vec![],
        )
        .await;
    assert_eq!(
        link,
        json!(
            "/api/v1/auth/login?return_to=%2Fp%2Fapp%2Fpipelines%2F00000000-0000-7000-8000-000000000001"
        )
    );

    // With a session cookie the same page reads, greets the person and offers "Sign out".
    let cookie = sign_in_session(&stack).await;
    browser
        .set_secure_cookie(&origin, "__Host-fleet_session", &cookie)
        .await;
    browser.client.goto(&format!("{origin}/")).await.unwrap();
    page.wait(
        "the signed-in overview",
        &has_text("h2", "No active pipelines"),
    )
    .await;
    page.wait("the person's name", &has_text(&q("whoami"), "Ada"))
        .await;
    assert_eq!(page.count(&q("sign-in-required")).await, 0);

    browser
        .client
        .find(fantoccini::Locator::XPath(
            "//button[normalize-space()='Sign out']",
        ))
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    // The server accepted the CSRF token: the session is gone and the page asks to sign in again.
    page.wait(
        "the sign-in notice after sign-out",
        &count_of(&q("sign-in-required")),
    )
    .await;
    let url = stack.config.database.url.as_ref().unwrap().expose();
    let store = fleet_store::Store::connect_lazy(url, fleet_store::PoolOptions::default()).unwrap();
    assert!(store.session(&cookie).await.unwrap().is_none());
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}
