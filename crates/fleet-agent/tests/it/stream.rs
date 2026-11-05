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
use tokio_tungstenite::tungstenite::{
    Message,
    handshake::server::{Request, Response},
    http::HeaderValue,
};

/// How the mock server behaves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Acks every batch.
    Normal,
    /// Closes the first connection after it receives this many events, without acking them.
    DropAfter(usize),
    /// Refuses every run (fatal).
    Reject,
    /// Accepts TCP, never answers.
    Blackhole,
}

#[derive(Default)]
struct State {
    /// Committed events by seq (dedup: a resent seq replaces nothing).
    committed: BTreeMap<u64, Value>,
    connections: u32,
    hellos: Vec<Value>,
    duplicates: u32,
    telemetry: u32,
    raw: String,
}

struct Server {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
}

fn server(mode: Mode) -> Server {
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
                    #[expect(clippy::result_large_err, reason = "tungstenite's callback signature")]
                    let cb = |req: &Request, mut resp: Response| {
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
        let reply = match frame["op"].as_str().unwrap() {
            "hello" => {
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
    }
}

fn fixture(name: &str) -> PathBuf {
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

fn run_agent(
    fixture_name: &str,
    server: Option<&str>,
    extra_args: &[&str],
    env: &[(&str, &str)],
) -> (Output, Duration, PathBuf) {
    let bin_dir = fake_maestro_dir();
    // A directory of flows: a suite, which is where the agent may ask for a JUnit report.
    let flows = bin_dir.join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    run_agent_on(fixture_name, server, extra_args, &[flows.as_os_str()], env)
}

fn run_agent_on(
    fixture_name: &str,
    server: Option<&str>,
    extra_args: &[&str],
    flow_args: &[&std::ffi::OsStr],
    env: &[(&str, &str)],
) -> (Output, Duration, PathBuf) {
    let bin_dir = fake_maestro_dir();
    let run_dir = bin_dir.join("run");
    let _ = std::fs::remove_dir_all(&run_dir);
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("fleet-agent"));
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
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let t = Instant::now();
    let out = cmd.output().unwrap();
    (out, t.elapsed(), run_dir)
}

fn types(state: &State) -> BTreeMap<String, usize> {
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
fn unauthorized_upgrade_does_not_trigger_batch() {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
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
