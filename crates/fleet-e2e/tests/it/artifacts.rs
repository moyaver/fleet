// SPDX-License-Identifier: AGPL-3.0-only
//! Artifacts end to end: the real agent uploads what a failed Maestro run left behind to the
//! real server, and a person opens it through a signed link on the usercontent origin.

use std::{net::SocketAddr, path::PathBuf};

use fleet_server::config::Secret;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::support::Stack;

const APP: &str = "fleet.example.com";
const USERCONTENT: &str = "fleet-usercontent.example.net";
/// Appears in the fixture's log, `commands.json` and JUnit report: the test plants it as a secret.
const PLANTED: &str = "Fleet never shows this";

pub(crate) fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fleet-e2e-{name}-{}", uuid::Uuid::now_v7()))
}

/// GET with a chosen `Host`: the status, the lowercased head and the body.
pub(crate) async fn get_as(addr: SocketAddr, host: &str, path: &str) -> (u16, String, Vec<u8>) {
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    sock.read_to_end(&mut out).await.unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&out[..split]).to_ascii_lowercase();
    let status = head[9..12].parse().unwrap();
    (status, head, out[split + 4..].to_vec())
}

/// The device log around the failed step (issue #75): the excerpt is within its cap, reaches the
/// failing step's end, holds no planted value, and opens like any other evidence.
async fn assert_device_logcat_excerpt(stack: &Stack, excerpt: &Value) {
    assert_eq!(excerpt["name"], "device-logcat.txt");
    assert!(
        excerpt["size_bytes"].as_u64().unwrap() <= 512 * 1024,
        "{excerpt}"
    );
    let link = excerpt["link"]["url"].as_str().unwrap();
    let path = url::Url::parse(link).unwrap().path().to_owned();
    let (status, head, body) = get_as(stack.addr(), USERCONTENT, &path).await;
    assert_eq!(status, 200, "{head}");
    assert!(head.contains("content-disposition: attachment"), "{head}");
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.contains("11-04 02:25:28."),
        "the failing step's end is in the excerpt"
    );
    assert!(
        !text.contains(PLANTED),
        "device_logcat leaked the planted secret"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_runs_evidence_is_uploaded_redacted_and_opens_only_on_the_usercontent_origin() {
    let stack = Stack::start(|c| {
        c.api.public_url = Some(format!("https://{APP}"));
        c.usercontent.url = Some(format!("https://{USERCONTENT}"));
        c.usercontent.signing_key = Some(Secret::from("0123456789abcdef0123456789abcdef-e2e"));
        c.storage.path = temp("blobs");
        c.artifacts.spool_dir = temp("spool");
    })
    .await;
    let ws = stack.ws_url();
    let key = stack.key.clone();
    let out = tokio::task::spawn_blocking(move || {
        crate::support::run_agent(
            "fail",
            &[
                ("FLEET_SERVER", &ws),
                ("FLEET_TOKEN", &key),
                ("MAESTRO_PLANTED", PLANTED),
                ("FLEET_UPLOAD_TIMEOUT", "20"),
            ],
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(1), "Maestro's own exit status (I1)");

    let snap = stack.await_run_ended("job").await;
    let flow = snap["flows"][0]["id"].as_str().unwrap().to_owned();
    let listed = stack
        .get(&format!("/api/v1/projects/app/flows/{flow}/artifacts"))
        .await;
    let artifacts = listed["artifacts"].as_array().unwrap();
    let find = |class: &str| -> &Value {
        artifacts
            .iter()
            .find(|a| a["class"] == class)
            .unwrap_or_else(|| panic!("no {class} artifact in {listed}"))
    };
    assert_eq!(find("failure_screenshot")["content_type"], "image/png");
    assert_eq!(find("commands")["name"], "commands.json");
    assert!(
        artifacts.iter().all(|a| a["state"] == "available"),
        "{listed}"
    );

    // The log and the commands were redacted on the runner: the planted value is not in what
    // the server stored, and the screenshot is the exact bytes of the file Maestro wrote.
    for class in ["commands", "maestro_log"] {
        let link = find(class)["link"]["url"].as_str().unwrap();
        let path = url::Url::parse(link).unwrap().path().to_owned();
        let (status, head, body) = get_as(stack.addr(), USERCONTENT, &path).await;
        assert_eq!(status, 200, "{class}: {head}");
        assert!(head.contains("content-security-policy: sandbox;"), "{head}");
        assert!(head.contains("content-disposition: attachment"), "{head}");
        let text = String::from_utf8(body).unwrap();
        assert!(!text.contains(PLANTED), "{class} leaked the planted secret");
        assert!(
            text.contains("[REDACTED]"),
            "{class} should show where it was redacted"
        );
    }
    assert_device_logcat_excerpt(&stack, find("device_logcat")).await;

    let link = find("failure_screenshot")["link"]["url"].as_str().unwrap();
    let path = url::Url::parse(link).unwrap().path().to_owned();
    let (status, head, body) = get_as(stack.addr(), USERCONTENT, &path).await;
    assert_eq!(status, 200);
    assert!(head.contains("content-disposition: inline"), "{head}");
    let fixture = crate::support::fixture("fail")
        .join("run/debug/fail/screenshots/step-005-assertCondition-Fleet_never_shows_this.png");
    assert_eq!(body, std::fs::read(fixture).unwrap());
    // The same link on the dashboard's own host serves nothing.
    let (status, _, body) = get_as(stack.addr(), APP, &path).await;
    assert_eq!(status, 404);
    assert!(body.len() < 1000, "not the screenshot");

    // The failing step's UI hierarchy is listed on that step and downloads as the same tree with
    // its on-screen text hidden (issue #74): the settings screen's labels are gone, the structure
    // is kept.
    let hierarchy = find("failure_hierarchy");
    assert_eq!(hierarchy["step"], 4, "{hierarchy}");
    assert_eq!(
        hierarchy["content_type"], "text/plain",
        "served as the text it is"
    );
    let link = hierarchy["link"]["url"].as_str().unwrap();
    let path = url::Url::parse(link).unwrap().path().to_owned();
    let (status, head, body) = get_as(stack.addr(), USERCONTENT, &path).await;
    assert_eq!(status, 200, "{head}");
    assert!(head.contains("content-security-policy: sandbox;"), "{head}");
    let text = String::from_utf8(body).unwrap();
    for shown in ["Wallpaper & style", "Network & internet", "2:25"] {
        assert!(!text.contains(shown), "on-screen text {shown:?} was served");
    }
    assert!(
        text.contains("[REDACTED]"),
        "the hidden text should show where it was"
    );
    assert!(
        text.contains("com.android.systemui:id/status_bar_container"),
        "the structure is kept"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_refuses_uploads_never_changes_the_run() {
    // A storage directory that cannot be created: every upload fails server-side (I2).
    let stack = Stack::start(|c| {
        c.storage.path = PathBuf::from("/dev/null/blobs");
        c.artifacts.spool_dir = temp("spool");
    })
    .await;
    let ws = stack.ws_url();
    let key = stack.key.clone();
    let out = tokio::task::spawn_blocking(move || {
        crate::support::run_agent(
            "fail",
            &[
                ("FLEET_SERVER", &ws),
                ("FLEET_TOKEN", &key),
                ("FLEET_UPLOAD_TIMEOUT", "5"),
            ],
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(1), "unchanged exit status (I1, I2)");
    let snap = stack.await_run_ended("job").await;
    assert_eq!(
        snap["agent_runs"][0]["state"], "ended",
        "the journal still arrived"
    );
}

/// The app crashed during a failing flow (issue #152): the crash is shown on the flow, and the
/// flow's verdict and failure message stay Maestro's assertion, not the crash.
#[tokio::test(flavor = "multi_thread")]
async fn an_app_crash_is_shown_on_its_flow_and_the_verdict_stays_maestros() {
    let stack = Stack::start(|_| {}).await;
    let ws = stack.ws_url();
    let key = stack.key.clone();
    let out = tokio::task::spawn_blocking(move || {
        crate::support::run_agent(
            "crash",
            &[
                ("FLEET_SERVER", &ws),
                ("FLEET_TOKEN", &key),
                ("FLEET_UPLOAD_TIMEOUT", "20"),
            ],
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(1), "Maestro's own exit status (I1)");

    let snap = stack.await_run_ended("job").await;
    let flow = snap["flows"][0]["id"].as_str().unwrap().to_owned();
    let detail = stack
        .get(&format!("/api/v1/projects/app/flows/{flow}"))
        .await;
    assert_eq!(
        detail["crash"]["package"], "com.android.settings",
        "{detail}"
    );
    assert!(
        detail["crash"]["exception"].as_str().unwrap().starts_with(
            "android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash"
        ),
        "{detail}"
    );
    assert_eq!(detail["flow"]["state"], "failed", "{detail}");
    assert_eq!(
        detail["failure"]["message"], "Assertion is false: \"Fleet never shows this\" is visible",
        "{detail}"
    );
    assert_eq!(
        detail["infrastructure"],
        Value::Null,
        "an assertion is the app's, not the device's: {detail}"
    );

    let listed = stack
        .get(&format!("/api/v1/projects/app/flows/{flow}/artifacts"))
        .await;
    assert!(
        listed["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["class"] == "device_logcat"),
        "{listed}"
    );
}

/// The device was lost during a failing flow (issue #145): the flow is labelled as the device's,
/// with the exception that named it, and its status stays failed.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_device_is_labelled_as_infrastructure_and_the_status_stays_failed() {
    let stack = Stack::start(|_| {}).await;
    let ws = stack.ws_url();
    let key = stack.key.clone();
    let out = tokio::task::spawn_blocking(move || {
        crate::support::run_agent(
            "device-killed",
            &[
                ("FLEET_SERVER", &ws),
                ("FLEET_TOKEN", &key),
                ("FLEET_UPLOAD_TIMEOUT", "20"),
            ],
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(1), "Maestro's own exit status (I1)");

    let snap = stack.await_run_ended("job").await;
    let flow = snap["flows"][0]["id"].as_str().unwrap().to_owned();
    let detail = stack
        .get(&format!("/api/v1/projects/app/flows/{flow}"))
        .await;
    assert_eq!(detail["flow"]["state"], "failed", "{detail}");
    assert_eq!(
        detail["infrastructure"]["evidence"], "maestro.android.DeviceServerDiedException",
        "{detail}"
    );
    // The label carries the recorded warning that the flow was cut short, and the failure's
    // provenance (issue #145).
    assert!(
        detail["infrastructure"]["warning"]
            .as_str()
            .unwrap()
            .ends_with("is incomplete: the flow was cut short"),
        "{detail}"
    );
    assert_eq!(
        detail["infrastructure"]["source"], "junit_final",
        "{detail}"
    );
    assert_eq!(detail["infrastructure"]["finality"], "verified", "{detail}");
    assert!(
        detail["failure"]["message"]
            .as_str()
            .unwrap()
            .starts_with("maestro.android.DeviceServerDiedException: Device server died"),
        "the failure is Maestro's: {detail}"
    );

    // Screen E carries the same label on the row.
    let history = stack.get("/api/v1/projects/app/history").await;
    let row = history["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["flow_id"] == flow.as_str())
        .unwrap_or_else(|| panic!("no history row for the flow: {history}"));
    assert_eq!(row["state"], "failed", "{row}");
    assert_eq!(
        row["infrastructure"]["evidence"], "maestro.android.DeviceServerDiedException",
        "{row}"
    );
    assert_eq!(row["infrastructure"]["finality"], "verified", "{row}");
    assert!(
        row["infrastructure"]["warning"]
            .as_str()
            .unwrap()
            .ends_with("is incomplete: the flow was cut short"),
        "{row}"
    );
}

/// Issue #145: an assertion failure is the app's, so its flow carries no infrastructure label.
#[tokio::test(flavor = "multi_thread")]
async fn an_assertion_failure_carries_no_infrastructure_label() {
    let stack = Stack::start(|_| {}).await;
    let ws = stack.ws_url();
    let key = stack.key.clone();
    let out = tokio::task::spawn_blocking(move || {
        crate::support::run_agent(
            "fail",
            &[
                ("FLEET_SERVER", &ws),
                ("FLEET_TOKEN", &key),
                ("FLEET_UPLOAD_TIMEOUT", "20"),
            ],
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(1), "Maestro's own exit status (I1)");

    let snap = stack.await_run_ended("job").await;
    let flow = snap["flows"][0]["id"].as_str().unwrap().to_owned();
    let detail = stack
        .get(&format!("/api/v1/projects/app/flows/{flow}"))
        .await;
    assert_eq!(detail["flow"]["state"], "failed", "{detail}");
    assert_eq!(detail["infrastructure"], Value::Null, "{detail}");
}
