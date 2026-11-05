// SPDX-License-Identifier: AGPL-3.0-only
//! Schema generation and stale-output checks through the xtask CLI.

use std::{fs, process::Command};

#[test]
fn generated_schemas_are_json_and_stale_outputs_fail_the_check() {
    let dir = std::env::temp_dir().join(format!("fleet-schema-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("test fixture should be valid");
    let run = |check: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command.args(["schema", "--output-dir"]).arg(&dir);
        if check {
            command.arg("--check");
        }
        command.output().expect("test fixture should be valid")
    };
    let generated = run(false);
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let file = dir.join("client-frame.json");
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(&file).expect("test fixture should be valid"))
            .expect("test fixture should be valid");
    assert_eq!(
        value["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert!(run(true).status.success());
    fs::write(file, "{}\n").expect("test fixture should be valid");
    assert!(
        !run(true).status.success(),
        "stale schema must fail the check"
    );
    fs::remove_dir_all(dir).expect("test fixture should be valid");
}
