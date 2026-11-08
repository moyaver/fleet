// SPDX-License-Identifier: AGPL-3.0-only
//! The whole agent against a mock server (M2 exit: "a real run streams to the mock server without
//! changing the result"; invariants I1, I2, I5, I6). A fake `maestro` replays a captured fixture.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use tokio_tungstenite::tungstenite::{
    Message,
    handshake::server::{Request, Response},
    http::HeaderValue,
};

/// How the mock server behaves.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Acks every batch.
    Normal,
    /// Closes the first connection after it receives this many events, without acking them.
    DropAfter(usize),
    /// Refuses every run (fatal).
    Reject,
    /// Accepts TCP, never answers.
    Blackhole,
    /// Upgrades at once, but takes this long to answer `hello` (a server busy under load).
    SlowWelcome(Duration),
    /// Refuses the first `times` upgrades with `429` and `Retry-After: retry_after` seconds.
    RateLimited { times: u32, retry_after: u32 },
}

#[derive(Default)]
pub(super) struct State {
    /// When each event type first arrived.
    first_seen: BTreeMap<String, Instant>,
    /// Committed events by seq (dedup: a resent seq replaces nothing).
    pub(super) committed: BTreeMap<u64, Value>,
    pub(super) connections: u32,
    /// When each connection was accepted, and each refusal sent.
    accepted: Vec<Instant>,
    refusals: Vec<Instant>,
    pub(super) hellos: Vec<Value>,
    /// Created once the agent has been sent its welcome, to release the fake Maestro's exit.
    gate: Option<PathBuf>,
    duplicates: u32,
    telemetry: u32,
    raw: String,
}

pub(super) struct Server {
    pub(super) addr: SocketAddr,
    pub(super) state: Arc<Mutex<State>>,
}

pub(super) fn server(mode: Mode) -> Server {
    let state = Arc::new(Mutex::new(State::default()));
    let (tx, rx) = std::sync::mpsc::channel();
    let st = Arc::clone(&state);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let st = Arc::clone(&st);
                tokio::spawn(async move {
                    if mode == Mode::Blackhole {
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                        drop(sock);
                        return;
                    }
                    let st_cb = Arc::clone(&st);
                    #[expect(clippy::result_large_err, reason = "tungstenite's callback signature")]
                    let cb = move |req: &Request, mut resp: Response| {
                        if let Mode::RateLimited { times, retry_after } = mode {
                            let mut s = st_cb.lock().unwrap();
                            if s.refusals.len() < times as usize {
                                s.refusals.push(Instant::now());
                                return Err(
                                    tokio_tungstenite::tungstenite::http::Response::builder()
                                        .status(429)
                                        .header("Retry-After", retry_after.to_string())
                                        .body(None)
                                        .unwrap(),
                                );
                            }
                        }
                        assert_eq!(
                            req.headers().get("Sec-WebSocket-Protocol").unwrap(),
                            "fleet.v1+json"
                        );
                        resp.headers_mut().insert(
                            "Sec-WebSocket-Protocol",
                            HeaderValue::from_static("fleet.v1+json"),
                        );
                        Ok(resp)
                    };
                    let Ok(ws) = tokio_tungstenite::accept_hdr_async(sock, cb).await else {
                        return;
                    };
                    connection(ws, mode, &st).await;
                });
            }
        });
    });
    Server {
        addr: rx.recv().unwrap(),
        state,
    }
}

async fn connection(
    mut ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    mode: Mode,
    st: &Mutex<State>,
) {
    let conn_no = {
        let mut s = st.lock().unwrap();
        s.connections += 1;
        s.accepted.push(Instant::now());
        s.connections
    };
    let mut received_here = 0usize;
    while let Some(Ok(message)) = ws.next().await {
        let Message::Text(text) = message else {
            if ws.flush().await.is_err() {
                return;
            }
            continue;
        };
        let frame: Value = serde_json::from_str(text.as_str()).unwrap();
        st.lock().unwrap().raw.push_str(text.as_str());
        let hello = frame["op"].as_str() == Some("hello");
        let reply = match frame["op"].as_str().unwrap() {
            "hello" => {
                if let Mode::SlowWelcome(wait) = mode {
                    tokio::time::sleep(wait).await;
                }
                if mode == Mode::Reject {
                    Some(
                        json!({ "op": "reject", "code": "unauthorized", "message": "no", "retry_after_ms": null, "fatal": true }),
                    )
                } else {
                    let mut s = st.lock().unwrap();
                    s.hellos.push(frame.clone());
                    let committed = s.committed.keys().next_back().copied().unwrap_or(0);
                    Some(
                        json!({ "op": "welcome", "protocol": 1, "connection_id": "00000000-0000-0000-0000-000000000001",
                        "epoch": conn_no, "committed_through": committed, "resume": if committed == 0 { "new" } else { "resumed" },
                        "limits": { "max_inflight_events": 512, "max_frame_bytes": 1_048_576, "heartbeat_interval_ms": 10000 } }),
                    )
                }
            }
            "events" => {
                let events = frame["events"].as_array().unwrap();
                received_here += events.len();
                if let Mode::DropAfter(n) = mode
                    && conn_no == 1
                    && received_here >= n
                {
                    return; // close without acking
                }
                let mut s = st.lock().unwrap();
                for e in events {
                    let kind = e["type"].as_str().unwrap().to_owned();
                    s.first_seen.entry(kind).or_insert_with(Instant::now);
                    let seq = e["seq"].as_u64().unwrap();
                    let expected = s.committed.keys().next_back().copied().unwrap_or(0) + 1;
                    match s.committed.entry(seq) {
                        std::collections::btree_map::Entry::Occupied(_) => s.duplicates += 1,
                        std::collections::btree_map::Entry::Vacant(slot) => {
                            assert_eq!(seq, expected, "events arrive contiguous");
                            slot.insert(e.clone());
                        }
                    }
                }
                let through = s.committed.keys().next_back().copied().unwrap_or(0);
                Some(json!({ "op": "ack", "through": through, "epoch": conn_no }))
            }
            "telemetry" => {
                st.lock().unwrap().telemetry += 1;
                Some(json!({ "op": "telemetry_ack", "through": frame["tseq_to"] }))
            }
            "bye" => return,
            _ => None,
        };
        if let Some(r) = reply
            && ws.send(Message::text(r.to_string())).await.is_err()
        {
            return;
        }
        if hello {
            let gate = st.lock().unwrap().gate.clone();
            if let Some(gate) = gate {
                let _ = std::fs::write(gate, b"");
            }
        }
    }
}

pub(super) fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/maestro/2.12.1")
        .join(name)
}

/// A directory with `maestro` → the fake script.
fn fake_maestro_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fleet-fake-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let link = dir.join("maestro");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/it/fake-maestro.sh"),
        &link,
    )
    .unwrap();
    dir
}

pub(super) fn run_agent(
    fixture_name: &str,
    server: Option<&str>,
    extra_args: &[&str],
    env: &[(&str, &str)],
) -> (Output, Duration, PathBuf) {
    let bin_dir = fake_maestro_dir();
    // A directory of flows: a suite, which is where the agent may ask for a JUnit report.
    let flows = bin_dir.join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    run_agent_on(
        fixture_name,
        server,
        extra_args,
        &[flows.as_os_str()],
        env,
        None,
    )
}

/// [`run_agent`] in `cwd`. Maestro writes a report without `--output` into its working directory,
/// and so does the agent's child, so a test that depends on that path gives it an empty one.
pub(super) fn run_agent_in(
    cwd: &Path,
    fixture_name: &str,
    server: Option<&str>,
    extra_args: &[&str],
    env: &[(&str, &str)],
) -> (Output, Duration, PathBuf) {
    let bin_dir = fake_maestro_dir();
    let flows = bin_dir.join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    run_agent_on(
        fixture_name,
        server,
        extra_args,
        &[flows.as_os_str()],
        env,
        Some(cwd),
    )
}

fn run_agent_on(
    fixture_name: &str,
    server: Option<&str>,
    extra_args: &[&str],
    flow_args: &[&std::ffi::OsStr],
    env: &[(&str, &str)],
    cwd: Option<&Path>,
) -> (Output, Duration, PathBuf) {
    let bin_dir = fake_maestro_dir();
    let run_dir = bin_dir.join("run");
    let _ = std::fs::remove_dir_all(&run_dir);
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("fleet-agent"));
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    // Hermetic: CI runners set GITHUB_* and the like, which the agent would (rightly) read.
    cmd.env_clear();
    for keep in ["PATH", "HOME", "TMPDIR"] {
        if let Some(v) = std::env::var_os(keep) {
            cmd.env(keep, v);
        }
    }
    cmd.args(["run", "--finalize-timeout", "30", "--run-dir"])
        .arg(&run_dir)
        .arg("--")
        .arg(bin_dir.join("maestro"))
        .args(["test"])
        .args(extra_args)
        .args(flow_args)
        .env("FLEET_FAKE_FIXTURE", fixture(fixture_name))
        .env("FLEET_FLUSH_TIMEOUT", "8")
        .stdin(Stdio::null());
    if let Some(s) = server {
        cmd.env("FLEET_SERVER", s);
        // A server run is authenticated unless a test says otherwise (`env` comes after this).
        cmd.env("FLEET_TOKEN", "fleet-test-token");
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let t = Instant::now();
    let out = cmd.output().unwrap();
    (out, t.elapsed(), run_dir)
}

pub(super) fn types(state: &State) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for e in state.committed.values() {
        *m.entry(e["type"].as_str().unwrap().to_owned()).or_default() += 1;
    }
    m
}

#[test]
fn a_run_streams_to_the_server_without_changing_the_result() {
    let srv = server(Mode::Normal);
    let (out, _, run_dir) = run_agent("suite", Some(&format!("ws://{}", srv.addr)), &[], &[]);
    // I1: same exit status and byte-identical stdout.
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stdout,
        std::fs::read(fixture("suite/stdout.bin")).unwrap()
    );
    let s = srv.state.lock().unwrap();
    let t = types(&s);
    assert_eq!(t.get("flow.started"), Some(&4), "{t:?}");
    assert_eq!(t.get("flow.reported"), Some(&4), "{t:?}");
    assert_eq!(t.get("run.ended"), Some(&1), "{t:?}");
    let last = *s.committed.keys().next_back().unwrap();
    assert_eq!(s.committed.len() as u64, last, "I5: gap-free from 1");
    assert_eq!(s.hellos[0]["ci"]["job_key"], "job");
    let failed: Vec<_> = s
        .committed
        .values()
        .filter(|e| e["type"] == "flow.reported" && e["data"]["outcome"] == "failed")
        .collect();
    assert_eq!(failed.len(), 1);
    let exited = s
        .committed
        .values()
        .find(|e| e["type"] == "process.exited")
        .unwrap();
    assert_eq!(
        exited["data"]["class"], "test_failures",
        "exit 1 with a report"
    );
    // The local journal has the same events.
    let journal = std::fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    assert_eq!(journal.lines().count() as u64, last);
    // The run-start event says which evidence the command will have (#31).
    let started = s
        .committed
        .values()
        .find(|e| e["type"] == "run.started")
        .unwrap();
    let ev = &started["data"]["evidence"];
    assert_eq!(ev["report"]["reason"], "injected", "{ev}");
    assert_eq!(
        ev["report"]["path"].as_str().map(Path::new),
        Some(run_dir.join("report.xml").as_path())
    );
    assert_eq!(
        ev["debug_output"].as_str().map(Path::new),
        Some(run_dir.join("artifacts").as_path())
    );
    assert_eq!(ev["maestro_log"], true);
}

/// The `maestro.detected` payloads the server committed.
fn maestro_detected(state: &State) -> Vec<Value> {
    state
        .committed
        .values()
        .filter(|e| e["type"] == "maestro.detected")
        .map(|e| e["data"].clone())
        .collect()
}

#[test]
fn the_maestro_version_is_journaled_once_per_run() {
    let srv = server(Mode::Normal);
    let (out, _, _) = run_agent("suite", Some(&format!("ws://{}", srv.addr)), &[], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stdout,
        std::fs::read(fixture("suite/stdout.bin")).unwrap()
    );
    assert_eq!(
        maestro_detected(&srv.state.lock().unwrap()),
        [json!({ "version": "2.12.1", "profile_supported": true })]
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("version profile"),
        "a supported version is not flagged"
    );
}

#[test]
fn an_unsupported_maestro_version_is_flagged_and_the_run_is_unchanged() {
    let srv = server(Mode::Normal);
    let (out, _, _) = run_agent(
        "suite",
        Some(&format!("ws://{}", srv.addr)),
        &[],
        &[("FLEET_FAKE_VERSION", "2.13.0")],
    );
    // I1: Maestro's exit status and output are untouched by the warning.
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stdout,
        std::fs::read(fixture("suite/stdout.bin")).unwrap()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "[fleet-agent] Maestro 2.13.0 is not in the version profile Fleet's parsers target (2.12): results may degrade"
        ),
        "{stderr}"
    );
    assert_eq!(
        maestro_detected(&srv.state.lock().unwrap()),
        [json!({ "version": "2.13.0", "profile_supported": false })]
    );
}

#[test]
fn a_log_without_a_version_line_reports_none() {
    let srv = server(Mode::Normal);
    let (out, _, _) = run_agent(
        "suite",
        Some(&format!("ws://{}", srv.addr)),
        &[],
        &[("FLEET_FAKE_VERSION", "")],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stdout,
        std::fs::read(fixture("suite/stdout.bin")).unwrap()
    );
    let detected: Vec<Value> = maestro_detected(&srv.state.lock().unwrap());
    assert!(detected.is_empty(), "{detected:?}");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("version profile"));
}

#[test]
fn a_dropped_connection_resumes_without_loss_or_duplicates() {
    let srv = server(Mode::DropAfter(10));
    let (out, _, _) = run_agent(
        "shard-split-2",
        Some(&format!("ws://{}", srv.addr)),
        &[],
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    let s = srv.state.lock().unwrap();
    assert!(s.connections >= 2, "reconnected");
    let last = *s.committed.keys().next_back().unwrap();
    assert_eq!(
        s.committed.len() as u64,
        last,
        "every seq from 1 committed once"
    );
    assert_eq!(types(&s).get("run.ended"), Some(&1));
    assert!(
        s.hellos.len() >= 2 && s.hellos[1]["resume"]["next_seq"].as_u64().unwrap() > 1,
        "the second hello resumes"
    );
}

#[test]
fn planted_secrets_never_reach_the_server() {
    let srv = server(Mode::Normal);
    let (out, _, _) = run_agent(
        "features",
        Some(&format!("ws://{}", srv.addr)),
        &["-e", "PASSWORD=fleet-planted-e-secret-7f3a"],
        &[("MAESTRO_FLEET_TOKEN", "fleet-planted-maestro-secret-91c2")],
    );
    assert_eq!(out.status.code(), Some(0));
    let s = srv.state.lock().unwrap();
    assert!(!s.committed.is_empty());
    for secret in [
        "fleet-planted-e-secret-7f3a",
        "fleet-planted-maestro-secret-91c2",
    ] {
        assert!(!s.raw.contains(secret), "{secret} reached the server");
    }
}

#[test]
fn no_server_black_hole_or_refusal_never_changes_the_result() {
    // A port nothing listens on.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let hole = server(Mode::Blackhole);
    let refusal = server(Mode::Reject);
    for (name, url) in [
        ("offline", None),
        ("closed port", Some(format!("ws://{closed}"))),
        ("black hole", Some(format!("ws://{}", hole.addr))),
        ("refused", Some(format!("ws://{}", refusal.addr))),
    ] {
        let (out, took, run_dir) = run_agent("fail", url.as_deref(), &[], &[]);
        assert_eq!(out.status.code(), Some(1), "{name}");
        assert_eq!(
            out.stdout,
            std::fs::read(fixture("fail/stdout.bin")).unwrap(),
            "{name}"
        );
        assert!(
            took < Duration::from_secs(25),
            "{name}: bounded by the flush timeout, took {took:?}"
        );
        assert!(
            run_dir.join("spool.bin").exists(),
            "{name}: the spool keeps the events"
        );
    }
}

#[test]
fn a_run_with_no_credential_is_offline_and_sends_nothing() {
    // The fork-PR case: the workflow names a server, but the job has no credential. A fork PR's
    // secret arrives as an empty FLEET_TOKEN, and there is no OIDC request (no `id-token`).
    let srv = server(Mode::Normal);
    let (out, _, run_dir) = run_agent(
        "fail",
        Some(&format!("ws://{}", srv.addr)),
        &[],
        &[("FLEET_TOKEN", "")],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stdout,
        std::fs::read(fixture("fail/stdout.bin")).unwrap(),
        "Maestro's output is unchanged"
    );
    // The observer's log is in the run directory.
    let said = std::fs::read_to_string(run_dir.join("observer.log")).unwrap();
    assert!(said.contains("running offline"), "{said}");
    let s = srv.state.lock().unwrap();
    assert_eq!(s.connections, 0, "no connection was attempted");
    assert!(s.hellos.is_empty() && s.committed.is_empty());
    assert!(
        run_dir.join("spool.bin").exists(),
        "the spool keeps the events"
    );
}

#[test]
fn a_server_slow_to_welcome_still_gets_the_whole_journal() {
    // The run is over before the server answers `hello` (a busy server takes seconds). The
    // finalize must keep waiting for that answer within its budget, not give up at once.
    let srv = server(Mode::SlowWelcome(Duration::from_secs(7)));
    let (out, took, run_dir) = run_agent(
        "pass",
        Some(&format!("ws://{}", srv.addr)),
        &[],
        &[("FLEET_FLUSH_TIMEOUT", "15")],
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(took < Duration::from_secs(15), "bounded: took {took:?}");
    let s = srv.state.lock().unwrap();
    assert_eq!(
        types(&s).get("run.ended"),
        Some(&1),
        "the run end reached the server"
    );
    let last = *s.committed.keys().next_back().unwrap();
    assert_eq!(s.committed.len() as u64, last, "gap-free (I5)");
    assert!(run_dir.join("spool.bin").exists());
}

#[test]
fn the_journal_is_sent_while_uploads_are_still_running() {
    // Uploads can't finish here (the fake server does not speak HTTP), so they hold the finalize
    // for their whole share of the budget. The journal's tail must not wait behind them: the
    // flows' results reach the server during that wait, and only the run end follows it.
    //
    // Maestro holds its exit until the server has sent the agent its welcome (FLEET_FAKE_GATE), so
    // the finalize starts on a connected transport. Otherwise the upload share is half of the
    // 3 s budget for an unanswered server, and a loaded runner measured 1.5 s here.
    //
    // The share is pinned at 10 s with FLEET_UPLOAD_TIMEOUT, so it does not depend on the
    // connection state at all. The flush budget is 20 s so the tail keeps its own time after the
    // share, and the whole finalize stays inside it. The flow results must reach the server at
    // least half the share before the run end: 5 s of slack for a slow runner.
    let srv = server(Mode::Normal);
    let gate = std::env::temp_dir().join(format!("fleet-gate-{}", std::process::id()));
    let _ = std::fs::remove_file(&gate);
    srv.state.lock().unwrap().gate = Some(gate.clone());
    let (out, took, _) = run_agent(
        "suite",
        Some(&format!("ws://{}", srv.addr)),
        &[],
        &[
            ("FLEET_FLUSH_TIMEOUT", "20"),
            ("FLEET_UPLOAD_TIMEOUT", "10"),
            ("FLEET_FAKE_GATE", gate.to_str().unwrap()),
            ("FLEET_FAKE_GATE_TIMEOUT", "30"),
        ],
    );
    let _ = std::fs::remove_file(&gate);
    assert_eq!(out.status.code(), Some(1));
    assert!(took < Duration::from_secs(20), "bounded: took {took:?}");
    let s = srv.state.lock().unwrap();
    assert!(
        !s.hellos.is_empty(),
        "the agent never connected before Maestro exited"
    );
    let reported = s.first_seen["flow.reported"];
    let ended = s.first_seen["run.ended"];
    assert!(
        ended.duration_since(reported) >= Duration::from_secs(5),
        "the run end waited for the uploads, the flow results did not: {:?} apart",
        ended.duration_since(reported)
    );
}

/// Reads the argv the fake Maestro recorded.
fn received_argv(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_single_flow_or_continuous_run_reaches_maestro_without_a_format() {
    // Issue #28: `--format` switches Maestro to its multi-flow path, so for one flow (or `-c`)
    // the argv Maestro receives must not carry it, and the run is otherwise unchanged.
    let bin_dir = fake_maestro_dir();
    let flow = bin_dir.join("login.yaml");
    std::fs::write(&flow, "appId: x\n---\n- launchApp\n").unwrap();
    for (name, extra) in [("single", &[][..]), ("continuous", &["-c"][..])] {
        let log = bin_dir.join(format!("argv-{name}.txt"));
        let (out, _, _) = run_agent_on(
            "single-flow-no-format",
            None,
            extra,
            &[flow.as_os_str()],
            &[("FLEET_FAKE_ARGV_LOG", log.to_str().unwrap())],
            None,
        );
        let argv = received_argv(&log);
        assert!(
            !argv.iter().any(|a| a == "--format" || a == "--output"),
            "{name}: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a == "--flatten-debug-output"),
            "{name}: {argv:?}"
        );
        assert_eq!(argv.last().map(String::as_str), flow.to_str(), "{name}");
        assert_eq!(
            out.stdout,
            std::fs::read(fixture("single-flow-no-format/stdout.bin")).unwrap(),
            "{name}: I1"
        );
    }
}

#[test]
fn a_suite_run_reaches_maestro_with_a_junit_report() {
    let bin_dir = fake_maestro_dir();
    let log = bin_dir.join("argv-suite.txt");
    let (_, _, run_dir) = run_agent(
        "suite",
        None,
        &[],
        &[("FLEET_FAKE_ARGV_LOG", log.to_str().unwrap())],
    );
    let argv = received_argv(&log);
    let pos = argv.iter().position(|a| a == "--format").unwrap();
    assert_eq!(argv.get(pos + 1).map(String::as_str), Some("junit"));
    assert_eq!(
        argv.get(pos + 3).map(Path::new),
        Some(run_dir.join("report.xml").as_path())
    );
}

#[test]
fn a_failing_single_flow_is_test_failures_without_a_report() {
    // No JUnit is asked for on a single flow, so exit 1 must still read as failed tests, from the
    // failure seen on stdout and in the bundle, not as an infrastructure error.
    let bin_dir = fake_maestro_dir();
    let flow = bin_dir.join("fail.yaml");
    std::fs::write(&flow, "appId: x\n---\n- launchApp\n").unwrap();
    let srv = server(Mode::Normal);
    let (out, _, _) = run_agent_on(
        "fail",
        Some(&format!("ws://{}", srv.addr)),
        &[],
        &[flow.as_os_str()],
        &[],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    let s = srv.state.lock().unwrap();
    let exited = s
        .committed
        .values()
        .find(|e| e["type"] == "process.exited")
        .expect("process.exited");
    assert_eq!(exited["data"]["class"], "test_failures", "{exited}");
    assert!(
        !s.committed
            .values()
            .any(|e| e["type"] == "report.parsed" && e["data"]["status"] != "absent"),
        "no report was read"
    );
}

#[test]
fn refused_upgrades_fall_back_to_batch_and_resume() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    let state = Arc::new(Mutex::new((0_u64, Vec::<u64>::new(), 0_u32)));
    let server_state = Arc::clone(&state);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let server = MockServer::start().await;
            Mock::given(method("GET")).respond_with(ResponseTemplate::new(426)).mount(&server).await;
            Mock::given(method("POST")).and(path("/api/v1/agent/batch"))
                .respond_with(move |request: &wiremock::Request| {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    let mut state = server_state.lock().unwrap();
                    state.2 += 1;
                    // Pretend an earlier connection committed the first two events.
                    if state.2 == 1 { state.0 = 2; }
                    let before = state.0;
                    for event in body["events"].as_array().unwrap() {
                        let seq = event["seq"].as_u64().unwrap();
                        if seq > state.0 {
                            assert_eq!(seq, state.0 + 1);
                            state.0 = seq;
                            state.1.push(seq);
                        }
                    }
                    ResponseTemplate::new(200).set_body_json(json!({"frames": [
                        {"op":"welcome", "protocol":1, "connection_id":"00000000-0000-0000-0000-000000000001",
                         "epoch":state.2, "committed_through":before, "resume":"resumed",
                         "limits":{"max_inflight_events":512,"max_frame_bytes":1_048_576,"heartbeat_interval_ms":10_000}},
                        {"op":"ack", "through":state.0,"epoch":state.2,"duplicates":0}
                    ]}))
                }).mount(&server).await;
            tx.send(*server.address()).unwrap();
            std::future::pending::<()>().await;
        });
    });
    let addr = rx.recv().unwrap();
    let (out, _, _) = run_agent(
        "suite",
        Some(&format!("ws://{addr}/api/v1/agent/ws")),
        &[],
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stdout,
        std::fs::read(fixture("suite/stdout.bin")).unwrap()
    );
    let state = state.lock().unwrap();
    assert!(state.2 >= 2, "expected batch requests");
    assert_eq!(
        state.1.first(),
        Some(&3),
        "server resume point is authoritative"
    );
    assert!(state.0 > 3, "journal was acknowledged");
}

#[test]
fn a_rate_limited_batch_waits_for_retry_after_and_the_run_keeps_maestros_status() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    // Requests seen, and when the first (refused) one and the first accepted one arrived.
    let state = Arc::new(Mutex::new((0_u32, None::<Instant>, None::<Instant>)));
    let server_state = Arc::clone(&state);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let server = MockServer::start().await;
            Mock::given(method("GET")).respond_with(ResponseTemplate::new(426)).mount(&server).await;
            Mock::given(method("POST")).and(path("/api/v1/agent/batch"))
                .respond_with(move |request: &wiremock::Request| {
                    let mut state = server_state.lock().unwrap();
                    state.0 += 1;
                    if state.0 == 1 {
                        state.1 = Some(Instant::now());
                        return ResponseTemplate::new(429).insert_header("Retry-After", "1");
                    }
                    state.2.get_or_insert_with(Instant::now);
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    let through = body["events"].as_array().unwrap().iter()
                        .map(|e| e["seq"].as_u64().unwrap())
                        .max()
                        .unwrap_or(0);
                    ResponseTemplate::new(200).set_body_json(json!({"frames": [
                        {"op":"welcome", "protocol":1, "connection_id":"00000000-0000-0000-0000-000000000001",
                         "epoch":1, "committed_through":0, "resume":"new",
                         "limits":{"max_inflight_events":512,"max_frame_bytes":1_048_576,"heartbeat_interval_ms":10_000}},
                        {"op":"ack", "through":through, "epoch":1, "duplicates":0}
                    ]}))
                }).mount(&server).await;
            tx.send(*server.address()).unwrap();
            std::future::pending::<()>().await;
        });
    });
    let addr = rx.recv().unwrap();
    let (out, _, _) = run_agent(
        "suite",
        Some(&format!("ws://{addr}/api/v1/agent/ws")),
        &[],
        &[],
    );
    // I1 and I2: the refusal never reaches Maestro's output or status.
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stdout,
        std::fs::read(fixture("suite/stdout.bin")).unwrap()
    );
    let state = state.lock().unwrap();
    let (refused, accepted) = (state.1.unwrap(), state.2.expect("a batch was accepted"));
    assert!(
        accepted.duration_since(refused) >= Duration::from_secs(1),
        "the batch waited for Retry-After"
    );
}

#[test]
fn unauthorized_upgrade_does_not_trigger_batch() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    let posts = Arc::new(Mutex::new(0_u32));
    let observed = Arc::clone(&posts);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(401))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v1/agent/batch"))
                .respond_with(move |_: &wiremock::Request| {
                    *observed.lock().unwrap() += 1;
                    ResponseTemplate::new(200)
                })
                .mount(&server)
                .await;
            tx.send(*server.address()).unwrap();
            std::future::pending::<()>().await;
        });
    });
    let addr = rx.recv().unwrap();
    let (out, _, _) = run_agent(
        "fail",
        Some(&format!("ws://{addr}/api/v1/agent/ws")),
        &[],
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(*posts.lock().unwrap(), 0);
}

#[test]
fn absolute_junit_path_emits_repository_relative_identity() {
    let (out, _, run_dir) = run_agent(
        "absolute-path",
        None,
        &[],
        &[("GITHUB_WORKSPACE", "/fixture/checkout")],
    );
    assert_eq!(out.status.code(), Some(0));
    let journal = std::fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    let event: serde_json::Value = journal
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|event| event["type"] == "flow.reported")
        .unwrap();
    assert_eq!(
        event["data"]["repo_path"],
        "examples/flows/phase0/pass.yaml"
    );
    assert_eq!(
        event["data"]["junit"]["file"],
        "/fixture/checkout/examples/flows/phase0/pass.yaml"
    );
}

#[test]
fn relative_parent_path_from_subdirectory_uses_git_root() {
    let bin_dir = fake_maestro_dir();
    let checkout = bin_dir.join("checkout");
    let subdir = checkout.join("subdir");
    std::fs::create_dir_all(checkout.join(".git")).unwrap();
    std::fs::create_dir_all(&subdir).unwrap();
    let flow = checkout.join("pass.yaml");
    std::fs::write(&flow, "captured input").unwrap();
    let run_dir = bin_dir.join("relative-run");
    let report = bin_dir.join("relative-report.xml");
    let custom_fixture = bin_dir.join("relative-fixture");
    std::fs::create_dir_all(custom_fixture.join("run/debug")).unwrap();
    for name in ["exit.json", "stdout.bin"] {
        std::fs::copy(
            fixture(&format!("absolute-path/{name}")),
            custom_fixture.join(name),
        )
        .unwrap();
    }
    std::fs::copy(
        fixture("absolute-path/run/debug/maestro.log"),
        custom_fixture.join("run/debug/maestro.log"),
    )
    .unwrap();
    let captured = std::fs::read_to_string(fixture("absolute-path/run/report.xml")).unwrap();
    std::fs::write(
        custom_fixture.join("run/report.xml"),
        captured.replace(
            "/fixture/checkout/examples/flows/phase0/pass.yaml",
            "../pass.yaml",
        ),
    )
    .unwrap();
    let out = Command::new(assert_cmd::cargo::cargo_bin!("fleet-agent"))
        .current_dir(&subdir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("FLEET_FAKE_FIXTURE", custom_fixture)
        .args(["run", "--finalize-timeout", "30", "--run-dir"])
        .arg(&run_dir)
        .arg("--")
        .arg(bin_dir.join("maestro"))
        .args(["test", "--format", "junit", "--output"])
        .arg(&report)
        .arg("../pass.yaml")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let journal = std::fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    let event: serde_json::Value = journal
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|event| event["type"] == "flow.reported")
        .unwrap();
    assert_eq!(event["data"]["repo_path"], "pass.yaml");
}

#[test]
fn unreachable_server_gets_a_short_finalization_budget() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let (out, elapsed, run_dir) = run_agent(
        "fail",
        Some(&format!("ws://{closed}")),
        &[],
        &[
            ("FLEET_FLUSH_TIMEOUT", "15"),
            ("FLEET_UPLOAD_ARTIFACTS", "off"),
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stdout,
        std::fs::read(fixture("fail/stdout.bin")).unwrap()
    );
    assert!(
        elapsed < Duration::from_secs(9),
        "unreachable server took {elapsed:?}"
    );
    assert!(run_dir.join("spool.bin").exists());
}

#[test]
fn batch_with_invalid_welcome_stops_without_sending_events() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = Arc::clone(&requests);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let server = MockServer::start().await;
            Mock::given(method("GET")).respond_with(ResponseTemplate::new(426)).mount(&server).await;
            Mock::given(method("POST")).and(path("/api/v1/agent/batch")).respond_with(move |request: &wiremock::Request| {
                seen.lock().unwrap().push(serde_json::from_slice(&request.body).unwrap());
                ResponseTemplate::new(200).set_body_json(json!({"frames":[{"op":"welcome", "protocol":99,
                    "connection_id":"00000000-0000-0000-0000-000000000001", "epoch":1, "committed_through":0,
                    "resume":"new", "limits":{"max_inflight_events":512,"max_frame_bytes":1_048_576,"heartbeat_interval_ms":10_000}}]}))
            }).mount(&server).await;
            tx.send(*server.address()).unwrap();
            std::future::pending::<()>().await;
        });
    });
    let addr = rx.recv().unwrap();
    let (out, _, _) = run_agent(
        "fail",
        Some(&format!("ws://{addr}/api/v1/agent/ws")),
        &[],
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "invalid welcome must stop batch transport"
    );
    assert_eq!(requests[0]["events"], serde_json::json!([]));
}

#[test]
fn planned_ordered_shard_forwards_its_workspace_config() {
    let bin_dir = fake_maestro_dir();
    let config = bin_dir.join("config.yaml");
    std::fs::write(&config, "flows: ['*.yaml']\nexecutionOrder:\n  continueOnFailure: false\n  flowsOrder: [first, second]\n").unwrap();
    let mut digest = String::new();
    for byte in Sha256::digest(std::fs::read(&config).unwrap()) {
        write!(digest, "{byte:02x}").unwrap();
    }
    let plan = bin_dir.join("plan.json");
    std::fs::write(&plan, serde_json::json!({"planId":"ordered", "configPath":"config.yaml", "configSha256":digest, "shards":[{"id":0,"flows":["first.yaml","second.yaml"]}]}).to_string()).unwrap();
    let log = bin_dir.join("argv.log");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("fleet-agent"))
        .current_dir(&bin_dir)
        .args(["run", "--finalize-timeout", "30", "--plan"])
        .arg(&plan)
        .args(["--shard", "0", "--"])
        .arg(bin_dir.join("maestro"))
        .arg("test")
        .env("FLEET_FAKE_FIXTURE", fixture("execution-order"))
        .env("FLEET_FAKE_ARGV_LOG", &log)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let args = std::fs::read_to_string(log).unwrap();
    assert!(args.contains("--config\nconfig.yaml\n"), "{args}");
}

#[test]
fn planned_config_digest_mismatch_refuses_to_launch_maestro() {
    let dir = fake_maestro_dir();
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    let plan = dir.join("digest-plan.json");
    std::fs::write(&plan, r#"{"planId":"ordered","configPath":"config.yaml","configSha256":"wrong","shards":[{"id":0,"flows":["first.yaml"]}]}"#).unwrap();
    let log = dir.join("never-launched");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("fleet-agent"))
        .current_dir(&dir)
        .args(["run", "--plan"])
        .arg(&plan)
        .args(["--shard", "0", "--"])
        .arg(dir.join("maestro"))
        .arg("test")
        .env("FLEET_FAKE_FIXTURE", fixture("pass"))
        .env("FLEET_FAKE_ARGV_LOG", &log)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!log.exists());
}

#[test]
fn planned_empty_shard_runs_nothing_and_exits_2() {
    let dir = fake_maestro_dir();
    let plan = dir.join("empty-shard-plan.json");
    std::fs::write(
        &plan,
        r#"{"planId":"empty","shards":[{"id":0,"flows":[]},{"id":1,"flows":["a.yaml"]}]}"#,
    )
    .unwrap();
    let log = dir.join("never-launched");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("fleet-agent"))
        .current_dir(&dir)
        .args(["run", "--plan"])
        .arg(&plan)
        .args(["--shard", "0", "--"])
        .arg(dir.join("maestro"))
        .arg("test")
        .env("FLEET_FAKE_FIXTURE", fixture("pass"))
        .env("FLEET_FAKE_ARGV_LOG", &log)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!log.exists(), "maestro must not start for an empty shard");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("nothing to run"), "{stderr}");
}

/// A journal at its cap stops with one `journal.truncated` line that says where it stopped. The
/// server still gets every event, numbered as before, and Maestro's output and status are the same
/// (I1, I5). Issue #37.
#[test]
fn a_journal_at_its_cap_stops_with_one_marker_and_the_run_is_unchanged() {
    const CAP: u64 = 2048;
    let srv = server(Mode::Normal);
    let (out, _, run_dir) = run_agent(
        "suite",
        Some(&format!("ws://{}", srv.addr)),
        &[],
        &[("FLEET_JOURNAL_MAX_BYTES", &CAP.to_string())],
    );
    // I1: Maestro's exit status and output are untouched.
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stdout,
        std::fs::read(fixture("suite/stdout.bin")).unwrap()
    );

    let text = std::fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let marker: Value = serde_json::from_str(lines.last().unwrap()).unwrap();
    assert_eq!(marker["type"], "journal.truncated", "{text}");
    assert_eq!(marker["reason"], "budget");
    let first_missing = marker["first_missing_seq"].as_u64().unwrap();
    let held = &lines[..lines.len() - 1];
    let held_bytes: u64 = held.iter().map(|l| l.len() as u64 + 1).sum();
    assert_eq!(marker["bytes_written"].as_u64(), Some(held_bytes));
    assert_eq!(
        held.len() as u64,
        first_missing - 1,
        "seq 1 up to the marker"
    );
    assert!(
        text.len() as u64 <= CAP + lines.last().unwrap().len() as u64 + 1,
        "at most the cap plus one marker line"
    );

    // The server got everything, gap-free (I5), and the journal's lines are the same events under
    // the same numbers.
    let s = srv.state.lock().unwrap();
    let last = *s.committed.keys().next_back().unwrap();
    assert_eq!(s.committed.len() as u64, last, "I5: gap-free from 1");
    assert!(last > first_missing, "events past the cap still streamed");
    for (i, line) in held.iter().enumerate() {
        let seq = i as u64 + 1;
        let journaled: Value = serde_json::from_str(line).unwrap();
        assert_eq!(journaled, s.committed[&seq], "seq {seq}");
    }
    drop(s);

    // The one end-of-run warning, with the agent's prefix, after Maestro has finished.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let warnings: Vec<&str> = stderr.lines().filter(|l| l.contains("size cap")).collect();
    assert_eq!(warnings.len(), 1, "{stderr}");
    assert_eq!(
        warnings[0],
        format!(
            "[fleet-agent] journal.jsonl reached its size cap before seq {first_missing}, so events from seq {first_missing} on are not in it (raise FLEET_JOURNAL_MAX_BYTES to keep them); the run is unaffected"
        )
    );

    // inspect reads the journal: everything up to the marker, and the first missing seq.
    let (result, events) = fleet_agent::inspect::inspect(&run_dir).unwrap();
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(
        json["journal"]["truncated"]["first_missing_seq"],
        first_missing
    );
    assert_eq!(json["journal"]["events"], held.len());
    assert_eq!(events.len(), held.len());
    assert!(
        fleet_agent::inspect::table(&result).contains(&format!(
            "Events from seq {first_missing} on are missing from it."
        )),
        "{}",
        fleet_agent::inspect::table(&result)
    );
}

/// A journal under its cap is the whole run: no marker, no warning, and the same events the server
/// got (issue #37).
#[test]
fn a_journal_under_the_cap_is_the_whole_run_with_no_marker() {
    let srv = server(Mode::Normal);
    let (out, _, run_dir) = run_agent("suite", Some(&format!("ws://{}", srv.addr)), &[], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stdout,
        std::fs::read(fixture("suite/stdout.bin")).unwrap()
    );
    assert!(!String::from_utf8_lossy(&out.stderr).contains("size cap"));

    let text = std::fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    assert!(!text.contains("journal.truncated"), "{text}");
    let s = srv.state.lock().unwrap();
    let last = *s.committed.keys().next_back().unwrap();
    assert_eq!(text.lines().count() as u64, last);
    for (i, line) in text.lines().enumerate() {
        let seq = i as u64 + 1;
        assert_eq!(
            serde_json::from_str::<Value>(line).unwrap(),
            s.committed[&seq]
        );
    }
    drop(s);
    let (result, _) = fleet_agent::inspect::inspect(&run_dir).unwrap();
    let json = serde_json::to_value(&result).unwrap();
    assert!(json["journal"]["truncated"].is_null(), "{json}");
}

#[test]
fn a_rate_limited_agent_waits_for_retry_after_and_the_run_keeps_maestros_status() {
    // Two upgrades are refused with `Retry-After: 1`; the third is accepted.
    let srv = server(Mode::RateLimited {
        times: 2,
        retry_after: 1,
    });
    let (out, _, _) = run_agent("suite", Some(&format!("ws://{}", srv.addr)), &[], &[]);
    // I1 and I2: the refusals never reach Maestro, and the run is what Maestro made it.
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stdout,
        std::fs::read(fixture("suite/stdout.bin")).unwrap()
    );
    let s = srv.state.lock().unwrap();
    assert_eq!(s.refusals.len(), 2, "both refusals were sent");
    // Each retry came no sooner than the server's Retry-After (the jittered backoff alone is
    // under a second at these attempt numbers).
    let [first, second] = s.refusals[..] else {
        panic!("two refusals");
    };
    assert!(second.duration_since(first) >= Duration::from_secs(1));
    let last_refusal = s.refusals[1];
    let accepted = s
        .accepted
        .first()
        .copied()
        .expect("a connection was accepted");
    assert!(accepted.duration_since(last_refusal) >= Duration::from_secs(1));
    assert!(
        s.committed.values().any(|e| e["type"] == "run.ended"),
        "the run's events arrived once the server let the agent in"
    );
}
