// SPDX-License-Identifier: AGPL-3.0-only
//! The live pages' contract (issue #149), in a real browser against the real server and simulated
//! agents: one polite status region per live page that batches run-level events, settled results
//! marked once per group and never per row, no bordered cards, and a phone header that is the
//! brand mark with every page one tap away. The emulator tile (issue #123) sits beside the runners.
//!
//! Every wait is on a DOM condition with a timeout, and every scenario ends with no CSP violation.

use std::time::{Duration, Instant};

use fleet_loadgen::Options;
use serde_json::{Value, json};

use crate::{
    browser::Browser,
    dashboard_ab::{
        PATIENCE, Page, before_report, count_of, has_text, held_run_options, q, run_options, stack,
        start_run,
    },
    dashboard_cd::worker_of,
    dashboard_ef::attempt_of,
    support::Stack,
};

/// A finished run on the project's one commit, with one failing flow on its first agent.
async fn finished_run(stack: &Stack, run_id: &str) {
    fleet_loadgen::run(Options {
        pace: Duration::ZERO,
        commit: Some("abc1234def5678".into()),
        git_ref: Some("refs/heads/feat/pay".into()),
        ..run_options(stack, run_id)
    })
    .await
    .unwrap();
}

/// The attempt of CI run `run_id`, once the agent has reported it (a run started in the background
/// registers its attempt a moment later).
async fn attempt_appears(stack: &Stack, run_id: &str) -> String {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let attempts = stack.get("/api/v1/projects/app/attempts").await;
        let found = attempts.as_array().and_then(|list| {
            list.iter()
                .find(|a| a["run_key"] == format!("loadgen/app:{run_id}"))
        });
        if let Some(a) = found {
            return a["id"].as_str().unwrap().to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "no attempt for {run_id}: {attempts}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The page's status region as the page's test id names it.
const REGION: &str = "[data-testid=announcements]";

#[tokio::test]
async fn the_active_emulators_tile_sits_beside_the_runners_and_counts_the_devices_seen() {
    // Issue #123: the tile belongs with the runners, not in the readout strip, and it counts the
    // emulators the run's two devices reported.
    let stack = stack(|_| {}).await;
    let hold = before_report();
    let run = start_run(held_run_options(&stack, "emulators", &hold));
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser
        .client
        .goto(&format!("http://{}/", stack.addr()))
        .await
        .unwrap();
    page.mark_loaded().await;
    page.wait(
        "the emulator count",
        &format!(
            "document.querySelector('{}') && document.querySelector('{} .tile-n')?.textContent.trim() === '2'",
            q("tile-emulators"),
            q("tile-emulators")
        ),
    )
    .await;
    assert_eq!(
        page.count(&format!(
            "aside[aria-label=\"Runners\"] {}",
            q("tile-emulators")
        ))
        .await,
        1,
        "beside the runners"
    );
    assert_eq!(
        page.count(&format!(".readouts {}", q("tile-emulators")))
            .await,
        0,
        "not in the readout strip any more"
    );
    hold.release();
    run.await.unwrap();
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

#[tokio::test]
async fn every_live_page_has_one_polite_status_region() {
    // Issue #149: Overview, Pipeline, Flow, Runner and Device are each one live region, polite.
    let stack = stack(|_| {}).await;
    let hold = before_report();
    let run = start_run(held_run_options(&stack, "regions", &hold));
    let attempt = attempt_appears(&stack, "regions").await;
    // Wait until the run has its flows and devices and runners on the server.
    let deadline = Instant::now() + PATIENCE;
    let snapshot = loop {
        let s: Value = stack
            .get(&format!("/api/v1/projects/app/attempts/{attempt}"))
            .await;
        if s["flows"].as_array().is_some_and(|f| !f.is_empty()) {
            break s;
        }
        assert!(Instant::now() < deadline, "no flows yet: {s}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let flow = snapshot["flows"][0]["id"].as_str().unwrap().to_owned();
    let device = snapshot["devices"][0]["id"].as_str().unwrap().to_owned();
    let worker = worker_of(&stack, &snapshot).await;

    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    let base = format!("http://{}", stack.addr());
    let screens = [
        ("overview", "/".to_owned(), "true".to_owned()),
        (
            "pipeline",
            format!("/p/app/pipelines/{attempt}"),
            format!("{} >= 1", count_of(&q("flow-row"))),
        ),
        (
            "flow",
            format!("/flows/{flow}?project=app"),
            format!("{} === 1", count_of(&q("flow-page"))),
        ),
        (
            "runner",
            format!("/workers/{worker}?project=app"),
            format!("{} === 1", count_of(&q("worker-page"))),
        ),
        (
            "device",
            format!("/devices/{device}?project=app"),
            format!("{} === 1", count_of(&q("device-page"))),
        ),
    ];
    for (screen, path, ready) in &screens {
        browser.client.goto(&format!("{base}{path}")).await.unwrap();
        page.wait(&format!("the {screen} page"), ready).await;
        page.wait(
            &format!("the {screen} status region"),
            &format!("{} === 1", count_of(REGION)),
        )
        .await;
        // One region for run events, polite; nothing on the page is assertive.
        assert_eq!(
            page.count(&format!("{REGION}[aria-live=polite][role=status]"))
                .await,
            1,
            "{screen}: one polite status region for run events"
        );
        assert_eq!(
            page.count("[aria-live=assertive]").await,
            0,
            "{screen}: nothing is assertive"
        );
    }
    hold.release();
    run.await.unwrap();
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

#[tokio::test]
async fn the_overview_reads_run_events_in_batches_and_never_per_update() {
    // Issue #149: a run that starts, a flow that fails and a run that ends are read out, and the
    // region changes at most once per five seconds however many frames arrive.
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
    page.wait("the empty overview", &has_text("h2", "No pipelines yet"))
        .await;
    page.mark_loaded().await;
    page.eval(
        &format!(
            "window.__changes = []; const el = document.querySelector('{REGION}'); window.__last = el.textContent;\
             new MutationObserver(() => {{ const t = el.textContent; if (t !== window.__last) {{ window.__last = t; window.__changes.push([Date.now(), t]); }} }})\
             .observe(el, {{ childList: true, subtree: true, characterData: true }});"
        ),
        vec![],
    )
    .await;
    let started = Instant::now();
    let hold = before_report();
    let run = start_run(held_run_options(&stack, "batched", &hold));
    page.wait(
        "the run started announcement",
        "window.__changes.some(c => c[1].includes(' started.'))",
    )
    .await;
    hold.release();
    run.await.unwrap();
    page.wait(
        "the run ended announcement",
        "window.__changes.some(c => c[1].includes(' ended: '))",
    )
    .await;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let changes = page.eval("return window.__changes;", vec![]).await;
    let changes = changes.as_array().unwrap();
    // At most one read-out per five seconds, plus the first.
    let allowed = elapsed_ms / 5_000 + 1;
    assert!(
        changes.len() as u64 <= allowed,
        "{} read-outs in {elapsed_ms} ms, at most {allowed} allowed: {changes:?}",
        changes.len()
    );
    let all: String = changes
        .iter()
        .filter_map(|c| c[1].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(all.contains("Run loadgen/app:batched started."), "{all}");
    assert!(
        all.contains("failed in"),
        "the failed flow is read out: {all}"
    );
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// The settled provenance marks of a page, checked against the contract: a row carries none, and
/// each section states Verified or Observed at most once. Every provenance label is one
/// `role=img` named "Provenance: …".
const AUDIT: &str = r"
  const settled = ['Provenance: verified', 'Provenance: observed'];
  const marks = Array.from(document.querySelectorAll('[aria-label^=\'Provenance: \']'));
  const problems = [];
  for (const m of marks) {
    if (m.getAttribute('role') !== 'img') problems.push('not role=img: ' + m.outerHTML.slice(0, 80));
  }
  for (const m of document.querySelectorAll('.pv, .pg')) {
    const label = m.closest('.pv') ? m.closest('.pv').getAttribute('aria-label') : '';
    if (!label || !label.startsWith('Provenance: ')) problems.push('unnamed provenance: ' + m.outerHTML.slice(0, 80));
  }
  const row = 'tbody tr, ul.list > li, [data-testid=flow-row], [data-testid=history-row], [data-testid=compare-row]';
  for (const r of document.querySelectorAll(row)) {
    for (const m of r.querySelectorAll('[aria-label^=\'Provenance: \']')) {
      if (settled.includes(m.getAttribute('aria-label'))) problems.push('a settled row is marked: ' + r.textContent.trim().slice(0, 60));
    }
  }
  for (const s of document.querySelectorAll('section')) {
    const inside = Array.from(s.querySelectorAll('[aria-label^=\'Provenance: \']')).filter(m => settled.includes(m.getAttribute('aria-label')) && !m.closest(row));
    if (inside.length > 1) problems.push('stated more than once in ' + (s.getAttribute('aria-labelledby') || s.getAttribute('aria-label') || s.className));
  }
  return problems;
";

/// The bordered boxes on a page: a container with a border on all four sides and a radius. Controls
/// (buttons, fields, menus, tables and their cells) are not cards.
const CARDS: &str = r"
  const controls = 'button, input, select, textarea, a, summary, label, table, th, td, svg, img, [popover], [role=menu], [role=menuitem], [role=region], [role=img], canvas';
  const found = [];
  for (const e of document.querySelectorAll('body *')) {
    if (e.matches(controls) || e.closest(controls) || e.closest('header.top')) continue;
    const s = getComputedStyle(e);
    const all = ['Top', 'Right', 'Bottom', 'Left'].every(side => parseFloat(s['border' + side + 'Width']) > 0);
    const radius = parseFloat(s.borderTopLeftRadius);
    const pill = radius >= Math.min(e.offsetWidth, e.offsetHeight) / 2;
    // A tag or a count chip is not a card: a card is a section, at least as tall as a line of text.
    const tall = e.offsetHeight >= 40;
    if (all && radius > 0 && !pill && tall) found.push(e.className || e.tagName);
  }
  return found;
";

#[tokio::test]
async fn settled_results_carry_no_mark_each_group_states_its_label_once_and_no_page_draws_a_card() {
    // Issue #149: the provenance and no-cards rules across every live page and the history.
    let stack = stack(|_| {}).await;
    finished_run(&stack, "marks").await;
    let attempt = attempt_of(&stack, "marks").await;
    let snapshot: Value = stack
        .get(&format!("/api/v1/projects/app/attempts/{attempt}"))
        .await;
    let failed = snapshot["flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["state"] == "failed")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let device = snapshot["devices"][0]["id"].as_str().unwrap().to_owned();
    let worker = worker_of(&stack, &snapshot).await;

    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    let base = format!("http://{}", stack.addr());
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
            format!("/p/app/pipelines/{attempt}"),
            format!("{} >= 1", count_of(&q("flow-row"))),
        ),
        (
            "flow",
            format!("/flows/{failed}?project=app"),
            format!("{} === 1", count_of(&q("flow-page"))),
        ),
        (
            "history",
            "/history?project=app".to_owned(),
            format!("{} >= 1", count_of(&q("history-row"))),
        ),
        (
            "runner",
            format!("/workers/{worker}?project=app"),
            format!("{} === 1", count_of(&q("worker-page"))),
        ),
        (
            "device",
            format!("/devices/{device}?project=app"),
            format!("{} === 1", count_of(&q("device-page"))),
        ),
    ];
    for (screen, path, ready) in &screens {
        browser.client.goto(&format!("{base}{path}")).await.unwrap();
        page.wait(&format!("the {screen} page"), ready).await;
        let problems = page.eval(AUDIT, vec![]).await;
        assert_eq!(problems, json!([]), "{screen}: provenance");
        let cards = page.eval(CARDS, vec![]).await;
        assert_eq!(cards, json!([]), "{screen}: bordered cards");
    }
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

#[tokio::test]
async fn the_phone_header_is_the_brand_mark_and_every_page_is_one_tap_away() {
    // Issue #149: under 720 px the bar shows the brand mark (and the live indicator and the ⋯
    // menu); the pages, the project and the account are in the menu.
    let stack = stack(|_| {}).await;
    finished_run(&stack, "phone").await;
    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    browser.set_viewport_width(390).await;
    browser
        .client
        .goto(&format!("http://{}/?project=app", stack.addr()))
        .await
        .unwrap();
    page.wait(
        "the overview",
        &format!("{} >= 1", count_of(&q("tile-emulators"))),
    )
    .await;
    let visible_links = page
        .eval(
            "return Array.from(document.querySelectorAll('.top-in a:not(.brand)')).filter(a => a.getBoundingClientRect().width > 1).length;",
            vec![],
        )
        .await;
    assert_eq!(
        visible_links,
        json!(0),
        "the bar shows no page links on a phone"
    );
    let menu = page
        .eval(
            "const m = document.getElementById('more-menu'); return { links: Array.from(m.querySelectorAll('nav a')).map(a => [a.textContent.trim(), a.getAttribute('href')]), project: m.textContent.includes('Project app') };",
            vec![],
        )
        .await;
    assert_eq!(
        menu["links"],
        json!([
            ["Overview", "/"],
            ["History", "/history"],
            ["Compare", "/compare"]
        ]),
        "every page is in the menu"
    );
    assert_eq!(menu["project"], json!(true), "the project is in the menu");
    assert_eq!(
        page.eval(
            "return getComputedStyle(document.querySelector('.only-phone')).display !== 'none';",
            vec![],
        )
        .await,
        json!(true),
        "the menu is the phone's"
    );
    browser.set_viewport_width(1440).await;
    browser.quit().await;
}
