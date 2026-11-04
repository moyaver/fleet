// SPDX-License-Identifier: AGPL-3.0-only
//! Screen F's REST read: `GET /api/v1/projects/{project}/compare?a=&b=` (seam S12). Attempts are
//! produced through the real ingest path. Every explanation of a difference must come from the
//! two sides' evidence; where there is none, there is no explanation.

use serde_json::{Value, json};

use super::{
    history::{DEATH_A, DEATH_B, Run, attempt_of},
    rig::{Rig, rig},
};

/// The compare view of two CI runs' attempts.
async fn compare(rig: &Rig, a: &str, b: &str) -> Value {
    let (a, b) = (attempt_of(rig, a).await, attempt_of(rig, b).await);
    rig.json(&format!("/api/v1/projects/app/compare?a={a}&b={b}"))
        .await
}

fn row<'a>(view: &'a Value, flow: &str) -> &'a Value {
    view["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["flow"] == flow)
        .unwrap_or_else(|| panic!("no row for {flow}: {view}"))
}

#[tokio::test]
async fn a_flow_that_fails_on_another_architecture_is_explained_by_the_changed_condition() {
    let rig = rig().await;
    // The same commit, tested on an x86_64 runner (A) and an aarch64 runner (B).
    let a = Run::passed("A1", "abc", "checkout")
        .and_passed("login")
        .and_passed("legacy")
        .publish_all(&rig)
        .await;
    let b = Run::failed("B1", "abc", "checkout", "Element not found: Done")
        .and_passed("login")
        .and_passed("new")
        .arch("aarch64")
        .publish_all(&rig)
        .await;

    let view = compare(&rig, "A1", "B1").await;

    // Both sides are summarised: which attempt, and how its flows ended.
    assert_eq!(view["a"]["attempt"]["run_key"], "o/r:A1");
    assert_eq!(view["b"]["attempt"]["run_key"], "o/r:B1");
    assert_eq!(view["a"]["flows"]["passed"], 3);
    assert_eq!(view["b"]["flows"]["passed"], 2);
    assert_eq!(view["b"]["flows"]["failed"], 1);
    assert_eq!(view["truncated"], false);

    let checkout = row(&view, "flows/checkout.yaml");
    assert_eq!(checkout["change"], "newly_failing");
    assert_eq!(checkout["a"]["flow_id"], a[0]);
    assert_eq!(checkout["b"]["flow_id"], b[0]);
    assert_eq!(checkout["b"]["state"], "failed");
    // The only explanation is what the two conditions say: the architecture differs.
    let explanations = checkout["explanations"].as_array().unwrap();
    assert_eq!(explanations.len(), 1, "{explanations:?}");
    let e = &explanations[0];
    assert_eq!(e["kind"], "condition_changed");
    assert_eq!(
        e["changes"],
        json!([{"dim": "arch", "a": "x86_64", "b": "aarch64"}])
    );
    assert_ne!(e["a"], e["b"]);
    assert_eq!(e["a"], checkout["a"]["condition_fingerprint"]);
    assert_eq!(e["b"], checkout["b"]["condition_fingerprint"]);

    // A flow that passed on both is unchanged, but its conditions differ too, and it says so.
    let login = row(&view, "flows/login.yaml");
    assert_eq!(login["change"], "unchanged");
    assert_eq!(login["explanations"][0]["kind"], "condition_changed");

    // Flows on one side only are listed as such, with the fact that explains it.
    let legacy = row(&view, "flows/legacy.yaml");
    assert_eq!(legacy["change"], "only_in_a");
    assert_eq!(legacy["b"], Value::Null);
    assert_eq!(
        legacy["explanations"],
        json!([{"kind": "flow_missing", "side": "b"}])
    );
    let new = row(&view, "flows/new.yaml");
    assert_eq!(new["change"], "only_in_b");
    assert_eq!(
        new["explanations"],
        json!([{"kind": "flow_missing", "side": "a"}])
    );
}

#[tokio::test]
async fn a_provisional_side_is_explained_by_its_provenance_and_nothing_else_is_invented() {
    let rig = rig().await;
    Run::passed("A2", "abc", "login")
        .and_passed("search")
        .publish(&rig)
        .await;
    // Same commit, same runner: B is still running, so its outcomes come from the live console.
    Run::passed("B2", "abc", "login")
        .and_passed("search")
        .live()
        .publish(&rig)
        .await;

    let view = compare(&rig, "A2", "B2").await;

    let login = row(&view, "flows/login.yaml");
    assert_eq!(login["change"], "unchanged", "both passed");
    assert_eq!(
        login["explanations"],
        json!([{
            "kind": "provenance_differs",
            "a": {"outcome_source": "junit_final", "finality": "verified"},
            "b": {"outcome_source": "live_console", "finality": "provisional"},
        }]),
        "B's pass is not settled, which is the only thing the evidence says differs: B has no \
         condition fingerprint yet, so no condition is claimed either way"
    );
    // Comparing an attempt with itself finds nothing to explain.
    let a = attempt_of(&rig, "A2").await;
    let same = rig
        .json(&format!("/api/v1/projects/app/compare?a={a}&b={a}"))
        .await;
    for r in same["rows"].as_array().unwrap() {
        assert_eq!(r["change"], "unchanged");
        assert_eq!(r["explanations"], json!([]), "{r}");
    }
}

#[tokio::test]
async fn two_failures_are_one_failure_unless_their_fingerprints_differ() {
    let rig = rig().await;
    Run::failed("A3", "abc", "pay", DEATH_A)
        .and_failed("cart", "Element not found: Cart")
        .and_failed("home", "Element not found: Home")
        .publish(&rig)
        .await;
    Run::failed("B3", "abc", "pay", DEATH_B)
        .and_failed("cart", "Assertion is false: cart is empty")
        .and_passed("home")
        .publish(&rig)
        .await;

    let view = compare(&rig, "A3", "B3").await;

    // The device died on another emulator at other times: the same failure, so nothing to explain.
    let pay = row(&view, "flows/pay.yaml");
    assert_eq!(pay["change"], "unchanged");
    assert_eq!(pay["explanations"], json!([]));
    assert_eq!(
        pay["a"]["failure_fingerprint"],
        pay["b"]["failure_fingerprint"]
    );
    // A different message is a different failure.
    let cart = row(&view, "flows/cart.yaml");
    assert_eq!(cart["change"], "unchanged", "failed on both sides");
    assert_eq!(cart["explanations"].as_array().unwrap().len(), 1);
    assert_eq!(cart["explanations"][0]["kind"], "failure_changed");
    assert_eq!(
        cart["explanations"][0]["a"],
        cart["a"]["failure_fingerprint"]
    );
    assert_eq!(
        cart["explanations"][0]["b"],
        cart["b"]["failure_fingerprint"]
    );
    // Recovered, with nothing the evidence says differs.
    let home = row(&view, "flows/home.yaml");
    assert_eq!(home["change"], "recovered");
    assert_eq!(home["explanations"], json!([]));
}

#[tokio::test]
async fn attempts_of_another_project_or_that_do_not_exist_are_missing() {
    let rig = rig().await;
    Run::passed("A4", "abc", "login").publish(&rig).await;
    let a = attempt_of(&rig, "A4").await;
    let nowhere = "00000000-0000-7000-8000-000000000000";

    let unknown = rig
        .get(&format!("/api/v1/projects/app/compare?a={a}&b={nowhere}"))
        .await;
    assert_eq!(unknown.status_code(), 404);
    // The attempt exists, but not in project `other`.
    let foreign = rig
        .get(&format!("/api/v1/projects/other/compare?a={a}&b={a}"))
        .await;
    assert_eq!(foreign.status_code(), 404);
    let malformed = rig.get("/api/v1/projects/app/compare?a=1&b=2").await;
    assert_eq!(malformed.status_code(), 400);
}
