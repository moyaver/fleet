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

    let snap = stack.latest_snapshot().await;
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
    let snap = stack.latest_snapshot().await;
    assert_eq!(
        snap["agent_runs"][0]["state"], "ended",
        "the journal still arrived"
    );
}
