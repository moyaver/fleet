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

#[test]
fn execution_order_matches_flows_by_their_header_name() {
    let dir = fresh_dir("order-names");
    std::fs::write(
        dir.join("a.yaml"),
        "name: second-step\nappId: example\n---\n- launchApp\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("b.yaml"),
        "name: first-step\nappId: example\n---\n- launchApp\n",
    )
    .unwrap();
    std::fs::write(dir.join("c.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        "flows: ['*.yaml']\nexecutionOrder:\n  flowsOrder: [first-step, second-step]\n",
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
        serde_json::json!(["b.yaml", "a.yaml", "c.yaml"]),
        "the flows named by the sequence stay together, in order, even though their file names differ"
    );
}

#[test]
fn execution_order_whose_first_name_has_no_flow_is_an_error() {
    let dir = fresh_dir("order-missing");
    for name in ["a.yaml", "c.yaml"] {
        std::fs::write(dir.join(name), "appId: example\n---\n- launchApp\n").unwrap();
    }
    std::fs::write(
        dir.join("config.yaml"),
        "flows: ['*.yaml']\nexecutionOrder:\n  flowsOrder: [missing, c, a]\n",
    )
    .unwrap();
    let output = plan_in(&dir, "1", &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("names no flow for missing"));
    assert!(!dir.join("manifest.json").exists());
}

#[test]
fn execution_order_with_a_repeated_name_is_not_a_sequence() {
    // Maestro's check fails when the written order and its de-duplicated prefix differ, so the
    // flows run as normal flows and can be split across shards.
    let dir = fresh_dir("order-repeated");
    for name in ["a.yaml", "b.yaml"] {
        std::fs::write(dir.join(name), "appId: example\n---\n- launchApp\n").unwrap();
    }
    std::fs::write(
        dir.join("config.yaml"),
        "flows: ['*.yaml']\nexecutionOrder:\n  flowsOrder: [a, a, b]\n",
    )
    .unwrap();
    let output = plan_in(&dir, "2", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(plan["shards"][0]["flows"], serde_json::json!(["a.yaml"]));
    assert_eq!(plan["shards"][1]["flows"], serde_json::json!(["b.yaml"]));
}

#[test]
fn a_flow_without_an_estimate_weighs_the_median_of_the_others() {
    let dir = fresh_dir("median");
    for name in ["a.yaml", "b.yaml", "c.yaml", "d.yaml"] {
        std::fs::write(dir.join(name), "appId: example\n---\n- launchApp\n").unwrap();
    }
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    std::fs::write(
        dir.join("estimates.json"),
        r#"{"a.yaml":30,"b.yaml":10,"d.yaml":10}"#,
    )
    .unwrap();
    let output = plan_in(
        &dir,
        "2",
        &["--estimates", dir.join("estimates.json").to_str().unwrap()],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    // c.yaml has no estimate, so it weighs the median (10 s). Counted as 1 s it would have been
    // packed with the 10 s flows, and the shards would be 30 s and 21 s instead of 30 s each.
    assert_eq!(plan["shards"][0]["flows"], serde_json::json!(["a.yaml"]));
    assert_eq!(plan["shards"][0]["estimatedSeconds"], 30);
    assert_eq!(
        plan["shards"][1]["flows"],
        serde_json::json!(["b.yaml", "c.yaml", "d.yaml"])
    );
    assert_eq!(plan["shards"][1]["estimatedSeconds"], 30);
}

/// Reads one HTTP request (headers, then a Content-Length body) and returns it as text.
fn read_request(stream: &mut impl std::io::Read) -> String {
    let mut bytes = Vec::new();
    let mut buf = [0_u8; 4096];
    while let Ok(n) = stream.read(&mut buf) {
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn fleet_requests_verify_against_fleet_ca_file_and_refuse_unknown_issuers() {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    use std::io::Write;
    use std::sync::Arc;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let tls_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tls");
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(tls_dir.join("cert.pem"))
            .unwrap()
            .map(Result::unwrap)
            .collect();
    let key = PrivateKeyDer::from_pem_file(tls_dir.join("key.pem")).unwrap();
    let server_config = Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap(),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Connection 0 trusts the test CA through FLEET_CA_FILE and must get the plan. Connection 1
    // runs without it, so the handshake must fail and nothing is sent.
    let server = std::thread::spawn(move || {
        let mut first = String::new();
        for connection in 0..2 {
            let (socket, _) = listener.accept().unwrap();
            let Ok(session) = rustls::ServerConnection::new(Arc::clone(&server_config)) else {
                continue;
            };
            let mut stream = rustls::StreamOwned::new(session, socket);
            let request = read_request(&mut stream);
            if connection == 0 {
                first = request;
                let _ = stream.write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
            }
        }
        first
    });
    let dir = fresh_dir("tls");
    std::fs::write(dir.join("a.yaml"), "appId: example\n---\n- launchApp\n").unwrap();
    std::fs::write(dir.join("config.yaml"), "flows: ['*.yaml']\n").unwrap();
    let server_url = format!("https://localhost:{port}");
    let run = |ca: Option<&std::path::Path>| {
        let mut command =
            std::process::Command::new(assert_cmd::cargo::cargo_bin!("fleet-planner"));
        command
            .args(["plan", "--config"])
            .arg(dir.join("config.yaml"))
            .args(["--shards", "1", "--manifest"])
            .arg(dir.join("manifest.json"))
            .arg("--matrix")
            .arg(dir.join("matrix.json"))
            .args(["--server", &server_url, "--token", "test-token"])
            .env_remove("FLEET_CA_FILE");
        if let Some(ca) = ca {
            command.env("FLEET_CA_FILE", ca);
        }
        command.output().expect("running fleet-planner")
    };
    let trusted = run(Some(&tls_dir.join("ca.pem")));
    let stderr = String::from_utf8_lossy(&trusted.stderr).into_owned();
    assert!(trusted.status.success(), "{stderr}");
    assert!(!stderr.contains("was not published"), "{stderr}");
    let untrusted = run(None);
    let stderr = String::from_utf8_lossy(&untrusted.stderr).into_owned();
    assert!(untrusted.status.success(), "{stderr}");
    assert!(
        stderr.contains("was not published"),
        "an unknown issuer must not verify: {stderr}"
    );
    let first = server.join().unwrap();
    assert!(first.starts_with("POST /api/v1/plans "), "{first}");
}
