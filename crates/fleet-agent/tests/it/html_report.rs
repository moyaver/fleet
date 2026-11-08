// SPDX-License-Identifier: AGPL-3.0-only
//! The Maestro HTML report (issue #43): found where `--output` says, uploaded as `html_report`,
//! and an upload that fails changes nothing about the run (I1, I2). The HTML bytes are the test's
//! own: no HTML report is captured in `fixtures/maestro` yet, and the file's name is the user's.

use std::path::PathBuf;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path, path_regex},
};

use super::stream::{fixture, run_agent};

const HTML: &[u8] = b"<!doctype html><html><body><h1>Maestro report</h1></body></html>";

/// The exit code and stdout the `suite` fixture replays: what Maestro itself produced.
fn maestro_result() -> (i32, Vec<u8>) {
    let exit: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture("suite/exit.json")).unwrap())
            .unwrap();
    let code = i32::try_from(exit["code"].as_i64().unwrap()).unwrap();
    (code, std::fs::read(fixture("suite/stdout.bin")).unwrap())
}

/// A temporary `report.html` for the fake Maestro to write to `--output`.
fn html_file(name: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("fleet-html-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("source.html");
    std::fs::write(&source, HTML).unwrap();
    (dir.join("maestro-report.html"), source)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn bodies(requests: &[Request], method_name: &str, suffix: &str) -> Vec<Vec<u8>> {
    requests
        .iter()
        .filter(|r| r.method.as_str() == method_name && r.url.path().ends_with(suffix))
        .map(|r| r.body.clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_html_report_is_found_and_uploaded_as_an_html_report() {
    let server = MockServer::start().await;
    // Every upload gets its own grant; the suite's other artifacts are uploaded too.
    Mock::given(method("POST"))
        .and(path("/api/v1/agent/artifacts"))
        .respond_with(|req: &Request| {
            let asked: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let artifact = uuid::Uuid::now_v7().to_string();
            ResponseTemplate::new(201).set_body_json(json!({
                "disposition": "upload",
                "artifact_id": artifact,
                "url": format!("/api/v1/agent/artifacts/{artifact}/content"),
                "complete_url": format!("/api/v1/agent/artifacts/{artifact}/complete"),
                "expires_at": "2030-01-01T00:00:00Z",
                "size": asked["size"],
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/api/v1/agent/artifacts/[^/]+/content$"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/v1/agent/artifacts/[^/]+/complete$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "available",
            "artifact_id": "0192f000-0000-7000-8000-000000000043",
        })))
        .mount(&server)
        .await;
    let (report, source) = html_file("found");
    let uri = server.uri();
    let report_arg = report.to_str().unwrap().to_owned();
    let source_env = source.to_str().unwrap().to_owned();
    let (out, _, _) = tokio::task::spawn_blocking(move || {
        run_agent(
            "suite",
            Some(&uri),
            &["--format", "html", "--output", report_arg.as_str()],
            &[
                ("FLEET_FAKE_HTML", source_env.as_str()),
                ("FLEET_TOKEN", "fleet-test-token"),
                ("FLEET_UPLOAD_TIMEOUT", "20"),
            ],
        )
    })
    .await
    .unwrap();

    let (code, stdout) = maestro_result();
    assert_eq!(out.status.code(), Some(code), "I1: Maestro's exit status");
    assert_eq!(out.stdout, stdout, "I1: Maestro's output, byte for byte");
    let requests = server.received_requests().await.unwrap();
    let asked: Vec<Value> = bodies(&requests, "POST", "/api/v1/agent/artifacts")
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    let html: Vec<&Value> = asked
        .iter()
        .filter(|r| r["class"] == "html_report")
        .collect();
    assert_eq!(
        html.len(),
        1,
        "the report is found and asked for once: {asked:?}"
    );
    assert_eq!(html[0]["content_type"], "text/html");
    assert_eq!(html[0]["path"], "maestro-report.html");
    assert_eq!(html[0]["size"], HTML.len());
    assert_eq!(
        html[0]["sha256"],
        hex(&Sha256::digest(HTML)),
        "the digest of the bytes sent"
    );
    // The bytes that went up are the report's, and it was completed.
    assert!(
        bodies(&requests, "PUT", "/content").contains(&HTML.to_vec()),
        "the report's bytes are PUT"
    );
    assert_eq!(bodies(&requests, "POST", "/complete").len(), asked.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_html_report_upload_that_fails_does_not_fail_the_run() {
    let server = MockServer::start().await;
    // Every upload request fails on the server side.
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/v1/agent/artifacts"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let (report, source) = html_file("fails");
    let uri = server.uri();
    let report_arg = report.to_str().unwrap().to_owned();
    let source_env = source.to_str().unwrap().to_owned();
    let (out, elapsed, _) = tokio::task::spawn_blocking(move || {
        run_agent(
            "suite",
            Some(&uri),
            &["--format", "html", "--output", report_arg.as_str()],
            &[
                ("FLEET_FAKE_HTML", source_env.as_str()),
                ("FLEET_TOKEN", "fleet-test-token"),
                ("FLEET_UPLOAD_TIMEOUT", "2"),
            ],
        )
    })
    .await
    .unwrap();

    let (code, stdout) = maestro_result();
    assert_eq!(
        out.status.code(),
        Some(code),
        "I2: the run keeps Maestro's status"
    );
    assert_eq!(out.stdout, stdout, "I1: Maestro's output, byte for byte");
    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "a failed upload is bounded by its budget: {elapsed:?}"
    );
}
