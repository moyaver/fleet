// SPDX-License-Identifier: AGPL-3.0-only
//! The usercontent origin over HTTP (seam S9): signed short-lived links, sandbox headers, no
//! cookies, and the app origin never serving artifact bytes (invariant I7). The real router on a
//! fresh database; requests pick the origin with the `Host` header, as a proxy would route them.
//! Links are forged with the documented format (`links.rs`: HMAC-SHA256 over a context line, the
//! project, the artifact and the expiry) to test what the server itself would never mint.

use axum_test::TestResponse;
use base64::Engine as _;
use fleet_core::policy::{Grant, Role};
use fleet_protocol::artifact::UploadResult;
use ring::hmac;
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::{
    artifacts::{Run, artifact_rig, request, run_with_failed_flow},
    rig::Rig,
};
use crate::{config::Secret, session};

const APP_HOST: &str = "fleet.example.com";
const UC_HOST: &str = "fleet-usercontent.example.net";
const KEY: &str = "0123456789abcdef0123456789abcdef-test-key";

/// A 1×1 PNG, as in the upload tests.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

/// class, path, declared type, bytes, expected disposition, expected served type.
type Case<'a> = (&'a str, &'a str, &'a str, &'a [u8], &'a str, &'a str);

const HTML: &[u8] =
    b"<!doctype html><html><body><script>fetch('/api/v1/projects')</script></body></html>";

async fn rig_with_usercontent() -> (Rig, Run) {
    let (rig, _dirs) = artifact_rig(|c| {
        c.api.public_url = Some(format!("https://{APP_HOST}"));
        c.usercontent.url = Some(format!("https://{UC_HOST}"));
        c.usercontent.signing_key = Some(Secret::from(KEY));
    })
    .await;
    let run = run_with_failed_flow(&rig, "1000").await;
    (rig, run)
}

async fn store(rig: &Rig, run: &Run, class: &str, path: &str, ctype: &str, bytes: &[u8]) -> Uuid {
    let req = request(run, class, path, ctype, bytes);
    match rig.upload(&req, bytes).await {
        UploadResult::Available { artifact_id } => artifact_id,
        rejected @ UploadResult::Rejected { .. } => panic!("{path} should be stored: {rejected:?}"),
    }
}

/// The link the listing hands out for artifact `id`.
async fn link_of(rig: &Rig, run: &Run, id: Uuid) -> String {
    let listed = rig.artifacts_of(run.flow).await;
    listed["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id.to_string())
        .unwrap()["link"]["url"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// GET a link's path on `host`, the way a proxy routes it.
async fn open(rig: &Rig, host: &str, link: &str) -> TestResponse {
    let url = url::Url::parse(link).unwrap();
    rig.app.get(url.path()).add_header("host", host).await
}

/// A link signed as `links.rs` documents, for any project, artifact and expiry.
fn forge(project: Uuid, artifact: Uuid, expires: i64, name: &str) -> String {
    let message = format!(
        "fleet-usercontent-v1\n{}\n{}\n{expires}",
        project.hyphenated(),
        artifact.hyphenated()
    );
    let key = hmac::Key::new(hmac::HMAC_SHA256, KEY.as_bytes());
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(hmac::sign(&key, message.as_bytes()).as_ref());
    format!(
        "https://{UC_HOST}/u/{}/{}/{expires}/{sig}/{name}",
        project.hyphenated(),
        artifact.hyphenated()
    )
}

fn header<'a>(res: &'a TestResponse, name: &str) -> &'a str {
    res.headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing header {name}"))
        .to_str()
        .unwrap()
}

#[tokio::test]
async fn a_screenshot_opens_inline_with_the_sandbox_headers_and_no_cookies() {
    let (rig, run) = rig_with_usercontent().await;
    let id = store(
        &rig,
        &run,
        "failure_screenshot",
        "checkout/shot.png",
        "image/png",
        PNG,
    )
    .await;
    let link = link_of(&rig, &run, id).await;
    assert!(link.starts_with(&format!("https://{UC_HOST}/u/")), "{link}");
    assert!(link.ends_with("/shot.png"), "{link}");

    let res = open(&rig, UC_HOST, &link).await;

    assert_eq!(res.status_code(), 200);
    assert_eq!(res.as_bytes().as_ref(), PNG);
    assert_eq!(header(&res, "content-type"), "image/png");
    assert_eq!(header(&res, "content-length"), "67");
    assert!(header(&res, "content-disposition").starts_with("inline; filename=\"shot.png\""));
    let csp = header(&res, "content-security-policy");
    assert!(csp.starts_with("sandbox;"), "{csp}");
    assert!(csp.contains("default-src 'none'"), "{csp}");
    assert!(
        csp.contains(&format!("frame-ancestors https://{APP_HOST}")),
        "{csp}"
    );
    assert!(!csp.contains("allow-"), "no sandbox exceptions: {csp}");
    assert_eq!(header(&res, "x-content-type-options"), "nosniff");
    assert_eq!(header(&res, "cross-origin-resource-policy"), "cross-origin");
    assert_eq!(header(&res, "referrer-policy"), "no-referrer");
    assert_eq!(
        header(&res, "access-control-allow-origin"),
        format!("https://{APP_HOST}"),
        "only the dashboard may fetch artifacts from scripts"
    );
    assert!(
        res.headers().get("set-cookie").is_none(),
        "no cookies are ever set"
    );
}

#[tokio::test]
async fn html_is_shown_sandboxed_and_everything_else_is_a_download() {
    let (rig, run) = rig_with_usercontent().await;
    let cases: [Case<'_>; 4] = [
        (
            "html_report",
            "index.html",
            "text/html",
            HTML,
            "inline",
            "text/html; charset=utf-8",
        ),
        (
            "junit",
            "report.xml",
            "application/xml",
            b"<?xml version=\"1.0\"?><testsuites/>",
            "attachment",
            "application/xml; charset=utf-8",
        ),
        (
            "maestro_log",
            "maestro.log",
            "text/plain",
            b"MFLT1|1|t|INFO|x|hello\n",
            "attachment",
            "text/plain; charset=utf-8",
        ),
        (
            "commands",
            "login/commands.json",
            "application/json",
            b"[{\"a\":1}]",
            "attachment",
            "text/plain; charset=utf-8",
        ),
    ];
    for (class, path, declared, bytes, disposition, served) in cases {
        let id = store(&rig, &run, class, path, declared, bytes).await;
        let res = open(&rig, UC_HOST, &link_of(&rig, &run, id).await).await;
        assert_eq!(res.status_code(), 200, "{path}");
        assert_eq!(res.as_bytes().as_ref(), bytes, "{path}");
        assert_eq!(
            header(&res, "content-type"),
            served,
            "{path}: what the bytes are"
        );
        let name = path.rsplit('/').next().unwrap();
        assert_eq!(
            header(&res, "content-disposition").split(';').next(),
            Some(disposition),
            "{path}"
        );
        assert!(header(&res, "content-disposition").contains(&format!("filename=\"{name}\"")));
        // The same hardening on every response, whatever it is.
        assert!(
            header(&res, "content-security-policy").starts_with("sandbox;"),
            "{path}"
        );
        assert_eq!(header(&res, "x-content-type-options"), "nosniff", "{path}");
    }
}

#[tokio::test]
async fn a_link_that_is_expired_tampered_or_for_another_project_opens_nothing() {
    let (rig, run) = rig_with_usercontent().await;
    let id = store(&rig, &run, "failure_screenshot", "s.png", "image/png", PNG).await;
    let project = rig.store.project_id("app").await.unwrap().unwrap();
    let other = rig.store.project_id("other").await.unwrap().unwrap();
    let now = OffsetDateTime::now_utc().unix_timestamp();

    // The server's own link works, and so does a forged one with the same format: the test's
    // forger is faithful.
    let real = link_of(&rig, &run, id).await;
    assert_eq!(open(&rig, UC_HOST, &real).await.status_code(), 200);
    let faithful = forge(project, id, now + 60, "s.png");
    assert_eq!(open(&rig, UC_HOST, &faithful).await.status_code(), 200);

    // Expired: genuinely signed, past its time.
    let expired = forge(project, id, now - 1, "s.png");
    assert_eq!(open(&rig, UC_HOST, &expired).await.status_code(), 410);

    // Tampered: any part changed after signing.
    let parts: Vec<&str> = faithful.split('/').collect();
    // [https:, "", host, u, project, artifact, expires, signature, name]
    let rebuilt = |at: usize, with: &str| {
        let mut p = parts.clone();
        p[at] = with;
        p.join("/")
    };
    let flipped_sig = {
        let mut s = parts[7].to_owned();
        let last = s.pop().unwrap();
        s.push(if last == 'A' { 'B' } else { 'A' });
        s
    };
    let later = (now + 7200).to_string();
    let other_artifact = Uuid::now_v7().to_string();
    for (what, link) in [
        ("signature", rebuilt(7, &flipped_sig)),
        ("expiry extended", rebuilt(6, &later)),
        ("another artifact", rebuilt(5, &other_artifact)),
        ("another project", rebuilt(4, &other.to_string())),
        ("truncated signature", rebuilt(7, "AAAA")),
        ("not a signature", rebuilt(7, "!!!")),
    ] {
        assert_eq!(
            open(&rig, UC_HOST, &link).await.status_code(),
            403,
            "{what}"
        );
    }

    // Wrong project: signed correctly for project `other`, naming this project's artifact.
    // The signature holds, and the artifact still isn't there.
    let wrong = forge(other, id, now + 60, "s.png");
    assert_eq!(open(&rig, UC_HOST, &wrong).await.status_code(), 404);

    // Signed by a different key (another installation's, or a leaked old one).
    let message = format!("fleet-usercontent-v1\n{project}\n{id}\n{}", now + 60);
    let foreign_key = hmac::Key::new(hmac::HMAC_SHA256, b"another key entirely, long enough!!");
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(hmac::sign(&foreign_key, message.as_bytes()).as_ref());
    let foreign = format!(
        "https://{UC_HOST}/u/{project}/{id}/{}/{sig}/s.png",
        now + 60
    );
    assert_eq!(open(&rig, UC_HOST, &foreign).await.status_code(), 403);

    // The refusals are sandboxed too.
    let res = open(&rig, UC_HOST, &expired).await;
    assert!(header(&res, "content-security-policy").starts_with("sandbox;"));
}

#[tokio::test]
async fn cookies_are_neither_needed_nor_honoured_on_the_usercontent_origin() {
    let (rig, run) = rig_with_usercontent().await;
    let id = store(&rig, &run, "failure_screenshot", "s.png", "image/png", PNG).await;
    let project = rig.store.project_id("app").await.unwrap().unwrap();
    // A fully signed-in admin session.
    let user = rig
        .store
        .upsert_user("https://idp.test", "u", None, None)
        .await
        .unwrap();
    let data = json!({
        "grants": [Grant { project: None, role: Role::Admin }], "csrf": "c", "email": null, "name": null,
    });
    let cookie = rig
        .store
        .create_session(
            Some(user),
            &data,
            OffsetDateTime::now_utc() + Duration::hours(1),
        )
        .await
        .unwrap();
    let cookie = format!("{}={}", session::COOKIE, cookie.expose());
    let url = |link: &str| url::Url::parse(link).unwrap().path().to_owned();

    // A session is no substitute for a signature …
    let unsigned = forge(project, id, 0, "s.png").replace("/0/", "/9999999999/");
    let res = rig
        .app
        .get(&url(&unsigned))
        .add_header("host", UC_HOST)
        .add_header("cookie", &cookie)
        .await;
    assert_eq!(res.status_code(), 403);
    // … and a signature needs no session; sending one changes nothing and sets nothing.
    let link = link_of(&rig, &run, id).await;
    let res = rig
        .app
        .get(&url(&link))
        .add_header("host", UC_HOST)
        .add_header("cookie", &cookie)
        .await;
    assert_eq!(res.status_code(), 200);
    assert!(res.headers().get("set-cookie").is_none());
}

#[tokio::test]
async fn the_app_origin_never_serves_artifact_bytes_and_the_usercontent_origin_serves_nothing_else()
{
    let (rig, run) = rig_with_usercontent().await;
    let id = store(&rig, &run, "failure_screenshot", "s.png", "image/png", PNG).await;
    let link = link_of(&rig, &run, id).await;
    let path = url::Url::parse(&link).unwrap().path().to_owned();

    // The very same path, on the dashboard's host (or any other host): not the artifact.
    for host in [APP_HOST, "evil.example.org", "localhost"] {
        let res = rig.app.get(&path).add_header("host", host).await;
        assert_eq!(res.status_code(), 404, "{host}");
        assert_ne!(res.as_bytes().as_ref(), PNG, "{host}");
    }
    // The upload endpoints take bytes in and never give them out.
    let put = format!("/api/v1/agent/artifacts/{id}/content");
    let res = rig
        .app
        .get(&put)
        .authorization_bearer(&rig.key)
        .add_header("host", APP_HOST)
        .await;
    assert_eq!(res.status_code(), 405);

    // On the usercontent host only signed links exist: no API, no dashboard, no agent routes.
    for (method, route) in [
        ("GET", "/".to_owned()),
        ("GET", "/api/v1/projects".to_owned()),
        ("GET", "/api/v1/openapi.json".to_owned()),
        ("GET", "/readyz".to_owned()),
        ("POST", "/api/v1/agent/artifacts".to_owned()),
        ("PUT", put),
    ] {
        let req = match method {
            "GET" => rig.app.get(&route),
            "POST" => rig.app.post(&route),
            _ => rig.app.put(&route),
        };
        let res = req
            .authorization_bearer(super::rig::VIEWER)
            .add_header("host", UC_HOST)
            .await;
        assert_eq!(
            res.status_code(),
            404,
            "{method} {route} on the usercontent host"
        );
        assert!(header(&res, "content-security-policy").starts_with("sandbox;"));
        assert_eq!(header(&res, "x-content-type-options"), "nosniff");
    }
    // The Host header may carry the default port.
    let res = open(&rig, &format!("{UC_HOST}:443"), &link).await;
    assert_eq!(res.status_code(), 200);
}

#[tokio::test]
async fn without_a_usercontent_origin_artifacts_are_stored_but_cannot_be_opened() {
    let (rig, _dirs) = artifact_rig(|_| {}).await;
    let run = run_with_failed_flow(&rig, "1010").await;
    let id = store(&rig, &run, "failure_screenshot", "s.png", "image/png", PNG).await;

    let listed = rig.artifacts_of(run.flow).await;
    assert_eq!(listed["artifacts"][0]["id"], id.to_string());
    assert_eq!(listed["artifacts"][0]["link"], Value::Null);
    assert_eq!(listed["usercontent_url"], Value::Null);
    let project = rig.store.project_id("app").await.unwrap().unwrap();
    let guess = forge(
        project,
        id,
        OffsetDateTime::now_utc().unix_timestamp() + 60,
        "s.png",
    );
    let res = rig
        .app
        .get(url::Url::parse(&guess).unwrap().path())
        .add_header("host", UC_HOST)
        .await;
    assert_eq!(res.status_code(), 404);
}

#[tokio::test]
async fn only_a_role_that_reads_artifacts_gets_links() {
    let (rig, run) = rig_with_usercontent().await;
    let id = store(&rig, &run, "failure_screenshot", "s.png", "image/png", PNG).await;
    let path = format!("/api/v1/projects/app/flows/{}/artifacts", run.flow);

    let viewer = rig.get_as("app", Role::Viewer, &path).await;
    assert_eq!(viewer.status_code(), 200);
    assert_eq!(viewer.json::<Value>()["artifacts"][0]["id"], id.to_string());
    assert_eq!(
        viewer.json::<Value>()["usercontent_url"],
        format!("https://{UC_HOST}")
    );
    // Someone with a role only on another project learns nothing about this one.
    let foreign = rig.get_as("other", Role::Admin, &path).await;
    assert_eq!(foreign.status_code(), 404);
    assert_eq!(foreign.json::<Value>(), json!({"error": "no such project"}));
    // The publisher that uploaded it cannot read it back (write-only credentials).
    let publisher = rig.app.get(&path).authorization_bearer(&rig.key).await;
    assert_eq!(publisher.status_code(), 401);
}
