// SPDX-License-Identifier: AGPL-3.0-only
//! Failure triage over HTTP (issue #84): a maintainer marks a failure known or resolved, the state
//! shows on screens D and E, and a role below maintainer is refused. Failures are produced through
//! the real ingest path.

use axum_test::TestResponse;
use fleet_core::policy::{Grant, Role};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};

use super::{
    history::Run,
    rig::{Rig, rig},
};
use crate::session;

/// A signed-in session whose only grants are `grants`, and its cookie.
async fn cookie_for(rig: &Rig, email: &str, grants: Vec<Grant>) -> String {
    let user = rig
        .store
        .upsert_user("https://idp.test", email, None, None)
        .await
        .unwrap();
    let data = json!({ "grants": grants, "csrf": "csrf", "email": email, "name": null });
    let cookie = rig
        .store
        .create_session(
            Some(user),
            &data,
            OffsetDateTime::now_utc() + Duration::hours(1),
        )
        .await
        .unwrap();
    format!("{}={}", session::COOKIE, cookie.expose())
}

async fn project_grant(rig: &Rig, role: Role) -> Vec<Grant> {
    let project = rig.store.project_id("app").await.unwrap().unwrap();
    vec![Grant {
        project: Some(project),
        role,
    }]
}

/// A PUT (or DELETE, when `body` is `None`) as a signed-in person, with the CSRF token.
async fn triage(
    rig: &Rig,
    cookie: &str,
    method: &str,
    fp: &str,
    body: Option<Value>,
) -> TestResponse {
    let url = format!("/api/v1/projects/app/failures/{fp}/triage");
    let request = match method {
        "PUT" => rig.app.put(&url),
        _ => rig.app.delete(&url),
    };
    let request = request
        .add_header("cookie", cookie)
        .add_header("x-csrf-token", "csrf");
    match body {
        Some(body) => request.json(&body).await,
        None => request.await,
    }
}

/// One failure, published; returns its fingerprint from the history.
async fn one_failure(rig: &Rig) -> String {
    let flow = Run::failed("T1", "abc", "checkout", "Element not found: Done")
        .publish(rig)
        .await;
    let page = rig.json("/api/v1/projects/app/history").await;
    let item = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["flow_id"] == flow.as_str())
        .unwrap();
    item["failure_fingerprint"].as_str().unwrap().to_owned()
}

async fn history_state(rig: &Rig, fp: &str) -> Value {
    let page = rig.json("/api/v1/projects/app/history").await;
    let item = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["failure_fingerprint"] == fp)
        .unwrap();
    item["triage"].clone()
}

#[tokio::test]
async fn a_maintainer_marks_a_failure_known_and_every_screen_shows_it() {
    let rig = rig().await;
    let fp = one_failure(&rig).await;
    // A second failure of the same kind, in another run: the same fingerprint.
    Run::failed("T2", "def", "checkout", "Element not found: Done")
        .publish(&rig)
        .await;
    let maintainer = cookie_for(
        &rig,
        "m@example.com",
        project_grant(&rig, Role::Maintainer).await,
    )
    .await;

    let untriaged = history_state(&rig, &fp).await;
    assert_eq!(
        untriaged["state"],
        Value::Null,
        "a seen failure starts untriaged"
    );
    assert!(untriaged["first_seen_at"].is_string());

    let res = triage(
        &rig,
        &maintainer,
        "PUT",
        &fp,
        Some(json!({"state": "known", "issue_url": "https://github.com/o/r/issues/84"})),
    )
    .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["state"], "known");

    // Screen E: every matching failure in the history carries the state and the link.
    let known = history_state(&rig, &fp).await;
    assert_eq!(known["state"], "known");
    assert_eq!(known["issue_url"], "https://github.com/o/r/issues/84");
    let page = rig.json("/api/v1/projects/app/history").await;
    for item in page["items"].as_array().unwrap() {
        if item["failure_fingerprint"] == fp.as_str() {
            assert_eq!(item["triage"]["state"], "known");
        }
    }
    // Screen E's flow panel groups the failures with the same state.
    let stats = rig
        .json("/api/v1/projects/app/flows/flows%2Fcheckout.yaml/stats")
        .await;
    assert_eq!(stats["recent_failures"][0]["triage"]["state"], "known");

    // Screen D: the failed execution names its fingerprint and the state.
    let page = rig.json("/api/v1/projects/app/history").await;
    let flow = page["items"][0]["flow_id"].as_str().unwrap().to_owned();
    let detail = rig
        .json(&format!("/api/v1/projects/app/flows/{flow}"))
        .await;
    assert_eq!(detail["failure"]["fingerprint"], fp.as_str());
    assert_eq!(detail["failure"]["triage"]["state"], "known");

    // Clearing it returns the failure to untriaged.
    triage(&rig, &maintainer, "DELETE", &fp, None)
        .await
        .assert_status_ok();
    assert_eq!(history_state(&rig, &fp).await["state"], Value::Null);
}

#[tokio::test]
async fn a_viewer_is_refused_and_the_state_does_not_change() {
    let rig = rig().await;
    let fp = one_failure(&rig).await;
    let viewer = cookie_for(
        &rig,
        "v@example.com",
        project_grant(&rig, Role::Viewer).await,
    )
    .await;

    let body = json!({"state": "known", "issue_url": "https://github.com/o/r/issues/84"});
    triage(&rig, &viewer, "PUT", &fp, Some(body))
        .await
        .assert_status_forbidden();
    triage(&rig, &viewer, "DELETE", &fp, None)
        .await
        .assert_status_forbidden();

    assert_eq!(history_state(&rig, &fp).await["state"], Value::Null);
}

#[tokio::test]
async fn a_person_with_no_role_on_the_project_gets_not_found() {
    let rig = rig().await;
    let fp = one_failure(&rig).await;
    let other = rig.store.project_id("other").await.unwrap().unwrap();
    let outsider = cookie_for(
        &rig,
        "o@example.com",
        vec![Grant {
            project: Some(other),
            role: Role::Admin,
        }],
    )
    .await;
    triage(
        &rig,
        &outsider,
        "PUT",
        &fp,
        Some(json!({"state": "resolved"})),
    )
    .await
    .assert_status_not_found();
}

#[tokio::test]
async fn a_known_failure_needs_an_http_link_and_a_change_needs_the_csrf_token() {
    let rig = rig().await;
    let fp = one_failure(&rig).await;
    let maintainer = cookie_for(
        &rig,
        "m@example.com",
        project_grant(&rig, Role::Maintainer).await,
    )
    .await;

    triage(
        &rig,
        &maintainer,
        "PUT",
        &fp,
        Some(json!({"state": "known"})),
    )
    .await
    .assert_status_bad_request();
    triage(
        &rig,
        &maintainer,
        "PUT",
        &fp,
        Some(json!({"state": "known", "issue_url": "javascript:alert(1)"})),
    )
    .await
    .assert_status_bad_request();

    // Without the session's CSRF token the same request is refused, even from a maintainer.
    let url = format!("/api/v1/projects/app/failures/{fp}/triage");
    rig.app
        .put(&url)
        .add_header("cookie", &maintainer)
        .json(&json!({"state": "resolved"}))
        .await
        .assert_status_forbidden();
    assert_eq!(history_state(&rig, &fp).await["state"], Value::Null);
}
