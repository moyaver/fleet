// SPDX-License-Identifier: AGPL-3.0-only
//! Every screen at a desktop width and at a phone width, with realistic data: long flow names, a
//! long failure message and many steps per flow (`fleet_loadgen::Options::wide`). A page never
//! scrolls sideways. A table may scroll inside its own box; the page stays the width of the window.

use fleet_loadgen::Options;
use serde_json::Value;

use crate::{
    browser::Browser,
    dashboard_ab::{Page, count_of, q, run_options, stack},
    dashboard_cd::worker_of,
    dashboard_ef::attempt_of,
    support::Stack,
};

/// The widths the redesign is checked at: a desktop window and a phone.
const WIDTHS: [u32; 2] = [1440, 390];

/// Seeds one finished run of realistic shape on the project's one commit, so the history and the
/// comparison have two attempts to show.
async fn seed(stack: &Stack, run_id: &str, first_agent: usize) {
    fleet_loadgen::run(Options {
        pace: std::time::Duration::ZERO,
        commit: Some("abc1234def5678".into()),
        git_ref: Some("refs/heads/feat/pay".into()),
        first_agent,
        wide: true,
        ..run_options(stack, run_id)
    })
    .await
    .unwrap();
}

/// What is wrong with the page at `width` px, or `None` when nothing reaches past the edge. Names
/// the deepest elements that do, skipping those inside a scrolling box of their own.
async fn overflow(page: &Page<'_>, width: u32) -> Option<String> {
    let fit = page
        .eval(
            r"const inner = window.innerWidth;
               const past = e => e.getBoundingClientRect().right > inner + 0.5;
               // Clipped by a scrolling ancestor, unless absolutely positioned against an
               // ancestor above that scroller (then it is a page-wide overflow).
               const inOwnScroller = e => {
                   const own = getComputedStyle(e).position;
                   if (own === 'fixed') return false;
                   for (let p = e.parentElement; p; p = p.parentElement) {
                       const s = getComputedStyle(p);
                       if (own !== 'absolute') {
                           if (s.overflowX !== 'visible') return true;
                       } else if (s.position !== 'static') {
                           return s.overflowX !== 'visible';
                       }
                   }
                   return false;
               };
               const culprit = e => past(e) && !inOwnScroller(e);
               const offenders = Array.from(document.body.querySelectorAll('*'))
                   .filter(culprit)
                   .filter(e => !Array.from(e.querySelectorAll('*')).some(culprit))
                   .slice(0, 12)
                   .map(e => e.tagName.toLowerCase()
                       + (e.dataset.testid ? '[' + e.dataset.testid + ']' : '')
                       + ' ' + (e.textContent || '').trim().slice(0, 40));
               return { inner, scroll: document.documentElement.scrollWidth, offenders };",
            vec![],
        )
        .await;
    let inner = fit["inner"].as_u64().unwrap();
    assert_eq!(
        inner,
        u64::from(width),
        "the layout viewport is the width the test sets"
    );
    let scroll = fit["scroll"].as_u64().unwrap();
    (scroll > inner).then(|| {
        format!(
            "page is {scroll} px wide at {width} px; deepest offenders: {}",
            fit["offenders"]
        )
    })
}

#[tokio::test]
async fn every_screen_fits_a_desktop_and_a_phone_without_sideways_page_scroll() {
    let stack = stack(|_| {}).await;
    seed(&stack, "wide-a", 0).await;
    seed(&stack, "wide-b", 1).await;
    let a = attempt_of(&stack, "wide-a").await;
    let b = attempt_of(&stack, "wide-b").await;
    let snapshot: Value = stack
        .get(&format!("/api/v1/projects/app/attempts/{a}"))
        .await;

    // The data is the shape the test is about: a long name and a long message on a failed flow.
    let failed = snapshot["flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["state"] == "failed")
        .unwrap_or_else(|| panic!("no failed flow: {snapshot}"));
    assert!(failed["name"].as_str().unwrap().len() > 60);
    let flow = failed["id"].as_str().unwrap().to_owned();
    let detail = stack
        .get(&format!("/api/v1/projects/app/flows/{flow}"))
        .await;
    assert!(detail["failure"]["message"].as_str().unwrap().len() > 200);

    let device = snapshot["devices"][0]["id"].as_str().unwrap().to_owned();
    let worker = worker_of(&stack, &snapshot).await;
    // Each screen, its address, and the condition that says it has loaded.
    let screens = [
        (
            "overview",
            "/".to_owned(),
            format!(
                "{} >= 1",
                count_of(&format!("{}, {}", q("active-row"), q("recent-row")))
            ),
        ),
        (
            "pipeline",
            format!("/p/app/pipelines/{a}"),
            format!("{} >= 1", count_of(&q("flow-row"))),
        ),
        (
            "flow",
            format!("/flows/{flow}?project=app"),
            format!(
                "{} === 1 && {} >= 2",
                count_of(&q("flow-page")),
                count_of("[role=treeitem]")
            ),
        ),
        (
            "history",
            "/history?project=app".to_owned(),
            format!("{} >= 1", count_of(&q("history-row"))),
        ),
        (
            "compare",
            format!("/compare?project=app&a={a}&b={b}"),
            format!("{} === 1", count_of(&q("comparison"))),
        ),
        (
            "worker",
            format!("/workers/{worker}?project=app"),
            format!("{} === 1", count_of(&q("worker-page"))),
        ),
        (
            "device",
            format!("/devices/{device}?project=app"),
            format!(
                "{} === 1 && {} >= 1",
                count_of(&q("device-page")),
                count_of(&q("device-flow"))
            ),
        ),
    ];

    let base = format!("http://{}", stack.addr());
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    let mut problems = Vec::new();
    for (screen, path, ready) in &screens {
        for width in WIDTHS {
            browser.set_viewport_width(width).await;
            browser.client.goto(&format!("{base}{path}")).await.unwrap();
            page.wait(&format!("the {screen} screen at {width} px"), ready)
                .await;
            if let Some(problem) = overflow(&page, width).await {
                problems.push(format!("{screen}: {problem}"));
            }
        }
    }
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
    assert!(
        problems.is_empty(),
        "screens that scroll sideways:\n{}",
        problems.join("\n")
    );
}
