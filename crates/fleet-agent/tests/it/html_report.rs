// SPDX-License-Identifier: AGPL-3.0-only
//! The Maestro HTML report (issue #43): found where the command puts it (`--output`, or Maestro's
//! default `report.html` in its working directory), uploaded as `html_report`, refused above 16 MiB
//! and ignored when stale, and an upload that fails changes nothing about the run (I1, I2, I3, I6).
//! The captured reports replay from `fixtures/maestro/2.12.1/html-*`; the HTML the tests write
//! themselves covers what no capture holds (a report over the size limit, a stale one).

use std::{
    fs,
    path::{Path, PathBuf},
    process::Output,
    time::{Duration, SystemTime},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path, path_regex},
};

use super::stream::{fixture, run_agent, run_agent_in};

const HTML: &[u8] = b"<!doctype html><html><body><h1>Maestro report</h1></body></html>";

/// The size above which a text artifact can't be redacted in memory (`upload::MAX_TEXT`).
const SIZE_LIMIT: usize = 16 * 1024 * 1024;

/// The exit code and stdout a fixture replays: what Maestro itself produced.
fn maestro_result(name: &str) -> (i32, Vec<u8>) {
    let exit: Value =
        serde_json::from_str(&fs::read_to_string(fixture(&format!("{name}/exit.json"))).unwrap())
            .unwrap();
    let code = i32::try_from(exit["code"].as_i64().unwrap()).unwrap();
    (
        code,
        fs::read(fixture(&format!("{name}/stdout.bin"))).unwrap(),
    )
}

/// The report a captured fixture holds, as Maestro wrote it.
fn captured_report(name: &str) -> Vec<u8> {
    fs::read(fixture(&format!("{name}/run/report.html"))).unwrap()
}

/// A temporary `report.html` for the fake Maestro to write to `--output`.
fn html_file(name: &str) -> (PathBuf, PathBuf) {
    let dir = scratch(name);
    let source = dir.join("source.html");
    fs::write(&source, HTML).unwrap();
    (dir.join("maestro-report.html"), source)
}

/// An empty directory for one test: the agent's working directory, where Maestro's default report
/// lands, and where the test's own files go.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fleet-html-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
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

/// Every upload the agent asked for, as the request bodies sent to the server.
fn asked(requests: &[Request]) -> Vec<Value> {
    bodies(requests, "POST", "/api/v1/agent/artifacts")
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect()
}

/// The journal's events of one type, from the run directory the agent wrote.
fn journal_events(run_dir: &Path, kind: &str) -> Vec<Value> {
    let text = fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    text.lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["type"] == kind)
        .collect()
}

/// A mock server that accepts every artifact: each upload gets its own grant and completes.
async fn accepting() -> MockServer {
    let server = MockServer::start().await;
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
    server
}

/// Runs the agent over `fixture_name` in `cwd` against `uri`, on a thread the mock server can use.
async fn agent_in(
    cwd: &Path,
    fixture_name: &'static str,
    uri: &str,
    args: &[&str],
    extra_env: &[(&'static str, String)],
) -> (Output, PathBuf) {
    let (cwd, uri) = (cwd.to_path_buf(), uri.to_owned());
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let mut env: Vec<(&'static str, String)> = vec![
        ("FLEET_TOKEN", "fleet-test-token".to_owned()),
        ("FLEET_UPLOAD_TIMEOUT", "20".to_owned()),
    ];
    env.extend(extra_env.iter().cloned());
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (out, _, run_dir) = run_agent_in(&cwd, fixture_name, Some(&uri), &args, &env);
        (out, run_dir)
    })
    .await
    .unwrap()
}

/// The one `html_report` upload: its name, type, size and digest, and the bytes that were PUT.
fn assert_html_uploaded(requests: &[Request], label: &str, bytes: &[u8]) {
    let asked = asked(requests);
    let html: Vec<&Value> = asked
        .iter()
        .filter(|r| r["class"] == "html_report")
        .collect();
    assert_eq!(html.len(), 1, "the report is asked for once: {asked:?}");
    assert_eq!(html[0]["path"], label);
    assert_eq!(html[0]["content_type"], "text/html");
    assert_eq!(html[0]["size"], bytes.len());
    assert_eq!(
        html[0]["sha256"],
        hex(&Sha256::digest(bytes)),
        "the digest of the bytes sent"
    );
    assert!(
        bodies(requests, "PUT", "/content").contains(&bytes.to_vec()),
        "the report's bytes are PUT"
    );
    assert_eq!(bodies(requests, "POST", "/complete").len(), asked.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_html_report_is_found_and_uploaded_as_an_html_report() {
    let server = accepting().await;
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

    let (code, stdout) = maestro_result("suite");
    assert_eq!(out.status.code(), Some(code), "I1: Maestro's exit status");
    assert_eq!(out.stdout, stdout, "I1: Maestro's output, byte for byte");
    let requests = server.received_requests().await.unwrap();
    assert_html_uploaded(&requests, "maestro-report.html", HTML);
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

    let (code, stdout) = maestro_result("suite");
    assert_eq!(
        out.status.code(),
        Some(code),
        "I2: the run keeps Maestro's status"
    );
    assert_eq!(out.stdout, stdout, "I1: Maestro's output, byte for byte");
    assert!(
        elapsed < Duration::from_secs(60),
        "a failed upload is bounded by its budget: {elapsed:?}"
    );
}

/// The captured `--format html --output` report (fixture `html-report`, a failing `pass.yaml` run
/// on the captured emulator) is found at its output and uploaded with Maestro's own bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_captured_html_report_is_uploaded_from_its_output() {
    let server = accepting().await;
    let cwd = scratch("captured-output");
    let output = cwd.join("report.html");
    let (out, _) = agent_in(
        &cwd,
        "html-report",
        &server.uri(),
        &["--format", "html", "--output", output.to_str().unwrap()],
        &[],
    )
    .await;

    let (code, stdout) = maestro_result("html-report");
    assert_eq!(out.status.code(), Some(code), "I1: Maestro's exit status");
    assert_eq!(out.stdout, stdout, "I1: Maestro's output, byte for byte");
    let report = captured_report("html-report");
    assert_html_uploaded(
        &server.received_requests().await.unwrap(),
        "report.html",
        &report,
    );
}

/// Maestro writes `report.html` into its working directory when `--output` is absent (fixture
/// `html-report-default-path`: the command ran with its working directory set to the run's
/// directory). The agent finds it there, and uploads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_captured_default_path_report_is_found_in_the_working_directory() {
    let server = accepting().await;
    let cwd = scratch("captured-default");
    let (out, _) = agent_in(
        &cwd,
        "html-report-default-path",
        &server.uri(),
        &["--format", "html"],
        &[],
    )
    .await;

    let (code, stdout) = maestro_result("html-report-default-path");
    assert_eq!(out.status.code(), Some(code), "I1: Maestro's exit status");
    assert_eq!(out.stdout, stdout, "I1: Maestro's output, byte for byte");
    assert!(cwd.join("report.html").is_file(), "Maestro's default path");
    let report = captured_report("html-report-default-path");
    assert_html_uploaded(
        &server.received_requests().await.unwrap(),
        "report.html",
        &report,
    );
}

/// `--format html-detailed` writes one file the same way: fixture `html-detailed`, asked for with
/// `--output`, is uploaded as the html report it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_captured_html_detailed_report_is_uploaded() {
    let server = accepting().await;
    let cwd = scratch("captured-detailed");
    let output = cwd.join("detailed.html");
    let (out, _) = agent_in(
        &cwd,
        "html-detailed",
        &server.uri(),
        &[
            "--format",
            "html-detailed",
            "--output",
            output.to_str().unwrap(),
        ],
        &[],
    )
    .await;

    let (code, stdout) = maestro_result("html-detailed");
    assert_eq!(out.status.code(), Some(code), "I1: Maestro's exit status");
    assert_eq!(out.stdout, stdout, "I1: Maestro's output, byte for byte");
    let report = captured_report("html-detailed");
    assert_html_uploaded(
        &server.received_requests().await.unwrap(),
        "detailed.html",
        &report,
    );
}

/// `html-detailed` without `--output` writes `report.html` in the working directory too (fixture
/// `html-detailed-default-path`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_captured_html_detailed_default_path_report_is_found() {
    let server = accepting().await;
    let cwd = scratch("captured-detailed-default");
    let (out, _) = agent_in(
        &cwd,
        "html-detailed-default-path",
        &server.uri(),
        &["--format", "html-detailed"],
        &[],
    )
    .await;

    let (code, stdout) = maestro_result("html-detailed-default-path");
    assert_eq!(out.status.code(), Some(code), "I1: Maestro's exit status");
    assert_eq!(out.stdout, stdout, "I1: Maestro's output, byte for byte");
    let report = captured_report("html-detailed-default-path");
    assert_html_uploaded(
        &server.received_requests().await.unwrap(),
        "report.html",
        &report,
    );
}

/// A report over the size limit can't be redacted in memory, so it is refused as
/// `not_redactable` (I6): nothing of it is sent, the journal and a warning say so, and the run
/// keeps Maestro's status and output (I2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_html_report_over_16_mib_is_refused_as_not_redactable_and_the_run_is_unchanged() {
    let server = accepting().await;
    let (report, source) = html_file("too-big");
    let mut big = HTML.to_vec();
    big.resize(SIZE_LIMIT + 1, b' ');
    fs::write(&source, &big).unwrap();
    let uri = server.uri();
    let report_arg = report.to_str().unwrap().to_owned();
    let source_env = source.to_str().unwrap().to_owned();
    let (out, _, run_dir) = tokio::task::spawn_blocking(move || {
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

    let (code, stdout) = maestro_result("suite");
    assert_eq!(
        out.status.code(),
        Some(code),
        "I2: the run keeps Maestro's status"
    );
    assert_eq!(out.stdout, stdout, "I1: Maestro's output, byte for byte");
    let requests = server.received_requests().await.unwrap();
    assert!(
        asked(&requests).iter().all(|r| r["class"] != "html_report"),
        "a report over the limit is never asked for"
    );
    assert!(
        !bodies(&requests, "PUT", "/content")
            .iter()
            .any(|b| b.len() == big.len()),
        "none of its bytes are sent"
    );
    let rejected = journal_events(&run_dir, "artifact.rejected");
    assert!(
        rejected.iter().any(|e| e["data"]["class"] == "html_report"
            && e["data"]["reason"] == "not_redactable"),
        "the refusal is journaled: {rejected:?}"
    );
    let warnings = journal_events(&run_dir, "agent.warning");
    assert!(
        warnings
            .iter()
            .any(|e| e["data"]["code"] == "artifact_not_uploaded"),
        "and warned about: {warnings:?}"
    );
}

/// A report last written before the run started is an earlier run's (I3): it is not uploaded,
/// and the journal says it was ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_html_report_is_ignored_with_a_report_stale_warning() {
    let server = accepting().await;
    let (report, source) = html_file("stale");
    fs::write(&source, HTML).unwrap();
    fs::File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    let uri = server.uri();
    let report_arg = report.to_str().unwrap().to_owned();
    let source_env = source.to_str().unwrap().to_owned();
    let (out, _, run_dir) = tokio::task::spawn_blocking(move || {
        run_agent(
            "suite",
            Some(&uri),
            &["--format", "html", "--output", report_arg.as_str()],
            &[
                ("FLEET_FAKE_HTML", source_env.as_str()),
                ("FLEET_FAKE_HTML_KEEP_MTIME", "1"),
                ("FLEET_TOKEN", "fleet-test-token"),
                ("FLEET_UPLOAD_TIMEOUT", "20"),
            ],
        )
    })
    .await
    .unwrap();

    let (code, stdout) = maestro_result("suite");
    assert_eq!(out.status.code(), Some(code), "I1: Maestro's exit status");
    assert_eq!(out.stdout, stdout, "I1: Maestro's output, byte for byte");
    let requests = server.received_requests().await.unwrap();
    assert!(
        asked(&requests).iter().all(|r| r["class"] != "html_report"),
        "an earlier run's report is not uploaded"
    );
    let warnings = journal_events(&run_dir, "agent.warning");
    assert!(
        warnings.iter().any(|e| e["data"]["code"] == "report_stale"
            && e["data"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("before this run started"))),
        "the stale report is named in a report_stale warning: {warnings:?}"
    );
}
