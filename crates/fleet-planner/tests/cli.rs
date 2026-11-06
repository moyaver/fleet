// SPDX-License-Identifier: AGPL-3.0-only
//! Public planner CLI tests.

#[test]
fn plans_the_captured_ordered_suite_as_one_unit() {
    let dir = std::env::temp_dir().join(format!("fleet-plan-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = dir.join("manifest.json");
    let matrix = dir.join("matrix.json");
    let config = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/flows/phase0/ordered/config.yaml");
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"))
        .args(["plan", "--config"])
        .arg(config)
        .args(["--shards", "1", "--manifest"])
        .arg(&manifest)
        .arg("--matrix")
        .arg(&matrix)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    let shards = plan["shards"].as_array().unwrap();
    assert!(shards.iter().any(|shard| shard["flows"]
        == serde_json::json!([
            "examples/flows/phase0/ordered/first-fails.yaml",
            "examples/flows/phase0/ordered/never-runs.yaml"
        ])));
    assert_eq!(
        plan["configPath"],
        "examples/flows/phase0/ordered/config.yaml"
    );
    assert_eq!(plan["configSha256"].as_str().map(str::len), Some(64));
    assert!(matrix.exists());
}

#[test]
fn discovers_unordered_flows_and_balances_supplied_estimates() {
    let dir = std::env::temp_dir().join(format!("fleet-plan-balance-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["a.yaml", "b.yaml", "c.yaml"] {
        std::fs::write(dir.join(name), "appId: example\n---\n- launchApp\n").unwrap();
    }
    std::fs::write(dir.join("config.yaml"), "flows:\n  - '*.yaml'\n").unwrap();
    std::fs::write(
        dir.join("estimates.json"),
        r#"{"a.yaml":10,"b.yaml":7,"c.yaml":3}"#,
    )
    .unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"))
        .args(["plan", "--config"])
        .arg(dir.join("config.yaml"))
        .args(["--shards", "2", "--estimates"])
        .arg(dir.join("estimates.json"))
        .arg("--manifest")
        .arg(dir.join("manifest.json"))
        .arg("--matrix")
        .arg(dir.join("matrix.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(plan["shards"][0]["flows"], serde_json::json!(["a.yaml"]));
    assert_eq!(
        plan["shards"][1]["flows"],
        serde_json::json!(["b.yaml", "c.yaml"])
    );
}

#[test]
fn includes_and_excludes_tagged_flows() {
    let dir = std::env::temp_dir().join(format!("fleet-plan-tags-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, tags) in [("a", "smoke"), ("b", "slow"), ("c", "smoke, slow")] {
        std::fs::write(
            dir.join(format!("{name}.yaml")),
            format!("appId: example\ntags: [{tags}]\n---\n- launchApp\n"),
        )
        .unwrap();
    }
    std::fs::write(
        dir.join("config.yaml"),
        "flows: ['*.yaml']\nincludeTags: [smoke]\nexcludeTags: [slow]\n",
    )
    .unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"))
        .args(["plan", "--config"])
        .arg(dir.join("config.yaml"))
        .args(["--shards", "1", "--manifest"])
        .arg(dir.join("manifest.json"))
        .arg("--matrix")
        .arg(dir.join("matrix.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(plan["shards"][0]["flows"], serde_json::json!(["a.yaml"]));
}

#[test]
fn constraints_keep_exclusive_flows_together_on_a_capable_shard() {
    let dir = std::env::temp_dir().join(format!("fleet-plan-constraints-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["a.yaml", "b.yaml", "c.yaml"] {
        std::fs::write(dir.join(name), "appId: example\n---\n- launchApp\n").unwrap();
    }
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    std::fs::write(dir.join("constraints.json"), r#"{"shards":[{"id":0,"capabilities":[]},{"id":1,"capabilities":["android"]}],"flows":{"a.yaml":{"requires":["android"],"exclusiveGroup":"account"},"b.yaml":{"exclusiveGroup":"account"}}}"#).unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"))
        .args(["plan", "--config"])
        .arg(dir.join("config.yaml"))
        .args(["--shards", "2", "--constraints"])
        .arg(dir.join("constraints.json"))
        .arg("--manifest")
        .arg(dir.join("manifest.json"))
        .arg("--matrix")
        .arg(dir.join("matrix.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        plan["shards"][1]["flows"],
        serde_json::json!(["a.yaml", "b.yaml"])
    );
    assert_eq!(plan["shards"][0]["flows"], serde_json::json!(["c.yaml"]));
}

#[test]
fn identical_inputs_produce_stable_plan_ids() {
    let dir = std::env::temp_dir().join(format!("fleet-plan-id-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    let run = |output: &str| {
        let status = std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"))
            .args(["plan", "--config"])
            .arg(dir.join("config.yaml"))
            .args(["--shards", "1", "--manifest"])
            .arg(dir.join(output))
            .arg("--matrix")
            .arg(dir.join(format!("{output}.matrix")))
            .status()
            .unwrap();
        assert!(status.success());
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(dir.join(output)).unwrap())
            .unwrap()
    };
    let first = run("one.json");
    let second = run("two.json");
    assert_eq!(first["planId"], second["planId"]);
    assert_ne!(first["planId"], "plan");
}

#[test]
fn publishes_manifest_with_publisher_credentials() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = vec![0; 8192];
        let n = socket.read(&mut request).unwrap();
        let text = String::from_utf8_lossy(&request[..n]);
        assert!(text.starts_with("POST /api/v1/plans "));
        assert!(
            text.to_ascii_lowercase()
                .contains("authorization: bearer test-token")
        );
        socket
            .write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .unwrap();
    });
    let dir = std::env::temp_dir().join(format!("fleet-plan-publish-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"))
        .args(["plan", "--config"])
        .arg(dir.join("config.yaml"))
        .args(["--shards", "1", "--manifest"])
        .arg(dir.join("manifest.json"))
        .arg("--matrix")
        .arg(dir.join("matrix.json"))
        .args([
            "--server",
            &format!("http://{address}"),
            "--token",
            "test-token",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
}

#[test]
fn history_p50_drives_allocation() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 8192];
            let n = socket.read(&mut request).unwrap();
            let text = String::from_utf8_lossy(&request[..n]);
            let p50 = if text.starts_with("GET /api/v1/projects/app/flows/a.yaml/stats") {
                10000
            } else {
                1000
            };
            let body = format!(r#"{{"duration":{{"p50_ms":{p50},"p90_ms":{p50}}}}}"#);
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    let dir = std::env::temp_dir().join(format!("fleet-plan-history-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["a.yaml", "b.yaml"] {
        std::fs::write(dir.join(name), "appId: example\n---\n- launchApp\n").unwrap();
    }
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"))
        .args(["plan", "--config"])
        .arg(dir.join("config.yaml"))
        .args(["--shards", "1", "--manifest"])
        .arg(dir.join("manifest.json"))
        .arg("--matrix")
        .arg(dir.join("matrix.json"))
        .args([
            "--history-server",
            &format!("http://{address}"),
            "--project",
            "app",
            "--token",
            "reader",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(plan["shards"][0]["estimatedSeconds"], 11);
    server.join().unwrap();
}

#[test]
fn refuses_to_replace_a_different_manifest() {
    let dir = std::env::temp_dir().join(format!("fleet-plan-immutable-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    std::fs::write(dir.join("manifest.json"), "existing").unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"))
        .args(["plan", "--config"])
        .arg(dir.join("config.yaml"))
        .args(["--shards", "1", "--manifest"])
        .arg(dir.join("manifest.json"))
        .arg("--matrix")
        .arg(dir.join("matrix.json"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read_to_string(dir.join("manifest.json")).unwrap(),
        "existing"
    );
}

#[test]
fn conflicting_matrix_does_not_create_manifest() {
    let dir = std::env::temp_dir().join(format!("fleet-plan-matrix-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    std::fs::write(dir.join("matrix.json"), "different").unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"))
        .args(["plan", "--config"])
        .arg(dir.join("config.yaml"))
        .args(["--shards", "1", "--manifest"])
        .arg(dir.join("manifest.json"))
        .arg("--matrix")
        .arg(dir.join("matrix.json"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!dir.join("manifest.json").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("matrix.json")).unwrap(),
        "different"
    );
}

#[test]
fn nested_config_emits_checkout_relative_flow_paths() {
    let dir = std::env::temp_dir().join(format!("fleet-plan-root-{}", std::process::id()));
    let nested = dir.join("suite");
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("a.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(nested.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"))
        .current_dir(&dir)
        .args([
            "plan",
            "--config",
            "suite/config.yaml",
            "--shards",
            "1",
            "--manifest",
            "manifest.json",
            "--matrix",
            "matrix.json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        plan["shards"][0]["flows"],
        serde_json::json!(["suite/a.yaml"])
    );
}

#[test]
fn default_config_discovers_top_level_flows_only() {
    let dir = std::env::temp_dir().join(format!("fleet-plan-default-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(dir.join("a.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(
        dir.join("nested/b.yaml"),
        "appId: example\n---\n- launchApp\n",
    )
    .unwrap();
    std::fs::write(dir.join("config.yaml"), "{}\n").unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"))
        .args(["plan", "--config"])
        .arg(dir.join("config.yaml"))
        .args(["--shards", "1", "--manifest"])
        .arg(dir.join("manifest.json"))
        .arg("--matrix")
        .arg(dir.join("matrix.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(plan["shards"][0]["flows"], serde_json::json!(["a.yaml"]));
}

#[test]
fn negated_glob_excludes_matching_flows() {
    let dir = std::env::temp_dir().join(format!("fleet-plan-negation-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["a.yaml", "b.yaml"] {
        std::fs::write(dir.join(name), "appId: example\n---\n- launchApp\n").unwrap();
    }
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml', '!b.yaml']\n").unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"))
        .args(["plan", "--config"])
        .arg(dir.join("config.yaml"))
        .args(["--shards", "1", "--manifest"])
        .arg(dir.join("manifest.json"))
        .arg("--matrix")
        .arg(dir.join("matrix.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(plan["shards"][0]["flows"], serde_json::json!(["a.yaml"]));
}

#[test]
fn plan_records_config_path_and_digest() {
    let dir = std::env::temp_dir().join(format!("fleet-plan-config-digest-{}", std::process::id()));
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join("a.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"))
        .current_dir(&dir)
        .args([
            "plan",
            "--config",
            "config.yaml",
            "--shards",
            "1",
            "--manifest",
            "manifest.json",
            "--matrix",
            "matrix.json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(plan["configPath"], "config.yaml");
    assert_eq!(plan["configSha256"].as_str().unwrap().len(), 64);
}

/// Runs `fleet-planner plan` on `dir/config.yaml` with `extra` arguments and returns the output.
fn plan_in(dir: &std::path::Path, shards: &str, extra: &[&str]) -> std::process::Output {
    std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"))
        .args(["plan", "--config"])
        .arg(dir.join("config.yaml"))
        .args(["--shards", shards, "--manifest"])
        .arg(dir.join("manifest.json"))
        .arg("--matrix")
        .arg(dir.join("matrix.json"))
        .args(extra)
        .output()
        .expect("running fleet-planner")
}

fn fresh_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fleet-plan-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creating the test directory");
    dir
}

#[test]
fn a_workspace_with_no_flows_is_an_error_and_writes_nothing() {
    let dir = fresh_dir("no-flows");
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    let output = plan_in(&dir, "1", &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no flows match"));
    assert!(!dir.join("manifest.json").exists());
}

#[test]
fn a_shard_that_would_get_no_flows_is_an_error_and_writes_nothing() {
    let dir = fresh_dir("empty-shard");
    std::fs::write(dir.join("a.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    let output = plan_in(&dir, "2", &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("would run no flows"));
    assert!(!dir.join("manifest.json").exists());
    assert!(!dir.join("matrix.json").exists());
}

#[test]
fn execution_order_names_that_match_no_flow_are_ignored() {
    let dir = fresh_dir("order-unknown");
    std::fs::write(dir.join("a.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(dir.join("b.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(dir.join("c.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        "flows: ['*.yaml']\nexecutionOrder:\n  flowsOrder: [missing, c, a]\n",
    )
    .unwrap();
    let output = plan_in(&dir, "1", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        plan["shards"][0]["flows"],
        serde_json::json!(["c.yaml", "a.yaml", "b.yaml"]),
        "the sequence keeps its order and no missing.yaml is invented"
    );
}

#[test]
fn history_that_cannot_be_read_falls_back_to_an_even_split() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 8192];
        let _ = socket.read(&mut request).unwrap();
        socket
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    let dir = fresh_dir("history-down");
    for name in ["a.yaml", "b.yaml"] {
        std::fs::write(dir.join(name), "appId: example\n---\n- launchApp\n").unwrap();
    }
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    let output = plan_in(
        &dir,
        "2",
        &[
            "--history-server",
            &format!("http://{address}"),
            "--project",
            "app",
            "--token",
            "reader",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("planning without it"));
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(plan["shards"][0]["flows"], serde_json::json!(["a.yaml"]));
    assert_eq!(plan["shards"][1]["flows"], serde_json::json!(["b.yaml"]));
    server.join().unwrap();
}

#[test]
fn an_unpublished_plan_still_writes_its_manifest() {
    let dir = fresh_dir("publish-down");
    std::fs::write(dir.join("a.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    // Nothing listens on this port: the publish fails, and the plan must still be written.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let output = plan_in(
        &dir,
        "1",
        &[
            "--server",
            &format!("http://127.0.0.1:{port}"),
            "--token",
            "t",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("was not published"));
    assert!(dir.join("manifest.json").exists());
}

#[test]
fn the_token_is_never_sent_in_cleartext_to_a_remote_host() {
    let dir = fresh_dir("cleartext");
    std::fs::write(dir.join("a.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    // The rule refuses before any connection is made, so this needs no network.
    let output = plan_in(
        &dir,
        "1",
        &[
            "--server",
            "http://fleet.example.invalid",
            "--token",
            "secret",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("need https://"), "{stderr}");
    assert!(!stderr.contains("secret"), "the token must not be echoed");
    assert!(dir.join("manifest.json").exists());
}
