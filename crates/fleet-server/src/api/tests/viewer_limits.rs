// SPDX-License-Identifier: AGPL-3.0-only
//! Viewer reads are limited per credential (issue #35): a browser session has a bucket of its own,
//! and a credential that was not accepted never gets one.

use fleet_core::policy::{Grant, Role};
use serde_json::json;
use time::{Duration, OffsetDateTime};

use super::rig::{Rig, rig_with};
use crate::{config::Config, session};

/// Two reads, then the third is refused.
fn tight(c: &mut Config) {
    c.rate_limits.viewer_burst = 2;
    c.rate_limits.viewer_requests_per_minute = 1;
}

/// A session that may read project `app`, as the sign-in route would create it.
async fn session_cookie(rig: &Rig) -> String {
    let project = rig.store.project_id("app").await.unwrap().unwrap();
    let user = rig
        .store
        .upsert_user("https://idp.test", "viewer", None, None)
        .await
        .unwrap();
    let data = json!({
        "grants": [Grant { project: Some(project), role: Role::Viewer }],
        "csrf": "csrf", "email": null, "name": null,
    });
    rig.store
        .create_session(
            Some(user),
            &data,
            OffsetDateTime::now_utc() + Duration::hours(1),
        )
        .await
        .unwrap()
        .expose()
        .to_owned()
}

async fn read_with_cookie(rig: &Rig, cookie: &str) -> axum_test::TestResponse {
    rig.app
        .get("/api/v1/projects")
        .add_header("cookie", format!("{}={cookie}", session::COOKIE))
        .await
}

#[tokio::test]
async fn a_browser_session_is_limited_on_its_own_bucket() {
    let rig = rig_with(tight).await;
    let cookie = session_cookie(&rig).await;
    assert_eq!(read_with_cookie(&rig, &cookie).await.status_code(), 200);
    assert_eq!(read_with_cookie(&rig, &cookie).await.status_code(), 200);
    let refused = read_with_cookie(&rig, &cookie).await;
    assert_eq!(refused.status_code(), 429);
    assert!(refused.headers().contains_key("retry-after"));

    assert_eq!(
        rig.get("/api/v1/projects").await.status_code(),
        200,
        "the viewer token is another credential with its own bucket"
    );
    let other = session_cookie(&rig).await;
    assert_eq!(
        read_with_cookie(&rig, &other).await.status_code(),
        200,
        "another session is another bucket"
    );
}

#[tokio::test]
async fn credentials_that_are_not_accepted_never_get_a_bucket() {
    let rig = rig_with(tight).await;
    for n in 0..50 {
        let res = rig
            .app
            .get("/api/v1/projects")
            .authorization_bearer(format!("made-up-{n}"))
            .await;
        assert_eq!(
            res.status_code(),
            401,
            "a made-up token is unauthorized, not limited"
        );
        let res = read_with_cookie(&rig, &format!("made-up-cookie-{n}")).await;
        assert_eq!(
            res.status_code(),
            401,
            "a made-up session is unauthorized, not limited"
        );
    }
    assert_eq!(
        rig.state.limits.viewer.tracked(),
        0,
        "no bucket for any of them"
    );

    assert_eq!(rig.get("/api/v1/projects").await.status_code(), 200);
    assert_eq!(
        rig.state.limits.viewer.tracked(),
        1,
        "the accepted viewer has one"
    );
}
