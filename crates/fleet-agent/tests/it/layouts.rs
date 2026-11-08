// SPDX-License-Identifier: AGPL-3.0-only
//! Gap 1 (issue #148): a live run settles its steps wherever the user's own flags put Maestro's
//! files. The fake Maestro writes each layout where the real CLI does (research 01 §4.1; the
//! captured trees in `fixtures/maestro/2.12.1`), and the agent's watcher must follow it there.

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use fleet_protocol::journal::{EventEnvelope, SourceKind};

use super::stream::fixture;

/// The steps a run settled from its files: `(step index, status)`, in journal order.
fn settled(events: &[EventEnvelope]) -> Vec<(u32, String)> {
    events
        .iter()
        .filter(|e| e.kind == "step.ended" && e.src.kind == SourceKind::ArtifactFile)
        .map(|e| {
            let index = e.subject.step.as_ref().map_or(u32::MAX, |s| s.index);
            (
                index,
                e.data["status"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// Runs `fleet-agent run -- maestro test <flags> <suite>` with the fake replaying the `pass`
/// fixture. `{ROOT}` in a flag is the run's own directory. HOME is the run's, so Maestro's state
/// directory (`~/.maestro/tests`) and the home directory are private to it. Returns the exit code
/// and the journal the agent wrote.
fn live(name: &str, flags: &[&str], rewrite_off: bool) -> (Option<i32>, Vec<EventEnvelope>) {
    let root = std::env::temp_dir().join(format!("fleet-layout-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/it/fake-maestro.sh"),
        bin.join("maestro"),
    )
    .unwrap();
    // A directory of flows: a suite, as in the other live tests.
    let flows = root.join("flows");
    fs::create_dir_all(&flows).unwrap();
    let run = root.join("run");
    let root_text = root.to_string_lossy().into_owned();
    let flags: Vec<String> = flags
        .iter()
        .map(|f| f.replace("{ROOT}", &root_text))
        .collect();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("fleet-agent"));
    // Hermetic: the environment is cleared, then only what the run needs is kept.
    cmd.env_clear();
    for keep in ["PATH", "TMPDIR"] {
        if let Some(v) = std::env::var_os(keep) {
            cmd.env(keep, v);
        }
    }
    cmd.env("HOME", root.join("home"))
        .env("FLEET_FAKE_FIXTURE", fixture("pass"))
        .env("FLEET_FAKE_LINGER", "0.2")
        .env("FLEET_FLUSH_TIMEOUT", "8")
        .env("FLEET_REWRITE", if rewrite_off { "off" } else { "auto" })
        .args(["run", "--finalize-timeout", "30", "--run-dir"])
        .arg(&run)
        .arg("--")
        .arg(bin.join("maestro"))
        .arg("test")
        .args(&flags)
        .arg(&flows)
        .stdin(Stdio::null());
    let out = cmd.output().unwrap();
    let text = fs::read_to_string(run.join("journal.jsonl")).unwrap();
    let events = text
        .lines()
        .map(|l| serde_json::from_str::<EventEnvelope>(l).unwrap())
        .collect();
    (out.status.code(), events)
}

/// Every layout settles the same steps with the same statuses as the default one, which the agent
/// injects a flattened directory for (fixtures `html-report-default-path`, `CANON`).
#[test]
fn every_layout_settles_the_steps_the_default_layout_settles() {
    let (code, events) = live("default", &[], false);
    let want = settled(&events);
    assert!(!want.is_empty(), "the default layout settles steps");
    assert!(
        want.iter().all(|(_, status)| status == "completed"),
        "the pass fixture completes every step: {want:?}"
    );
    let cases: [(&str, &[&str], bool); 9] = [
        // paths-none, single-flow-no-format: no flags at all, rewrite off.
        ("no-flags-rewrite-off", &[], true),
        // paths-test-output-dir: the bundles under the test output, the log under the state directory.
        ("test-output-dir", &["--test-output-dir", "{ROOT}/T"], false),
        // paths-debug-output: both under the debug directory's own `.maestro/tests`.
        ("debug-output", &["--debug-output", "{ROOT}/D"], false),
        // paths-debug-and-test-output-same: the two trees split even when the paths are equal.
        (
            "debug-and-test-output-same",
            &[
                "--debug-output",
                "{ROOT}/X",
                "--test-output-dir",
                "{ROOT}/X",
            ],
            false,
        ),
        // Different paths: the log follows --debug-output, the bundles --test-output-dir.
        (
            "debug-and-test-output-apart",
            &[
                "--debug-output",
                "{ROOT}/D",
                "--test-output-dir",
                "{ROOT}/T",
            ],
            false,
        ),
        // html-report-default-path: the flattened debug directory holds the log and the flows.
        (
            "flattened-debug-output",
            &["--flatten-debug-output", "--debug-output", "{ROOT}/F"],
            false,
        ),
        // --flatten-debug-output alone (rewrite off): Maestro writes into the home directory.
        (
            "flattened-alone-rewrite-off",
            &["--flatten-debug-output"],
            true,
        ),
        // Flattened, --test-output-dir is ignored (research 01 §4.1, `[V]`): the debug directory wins.
        (
            "flattened-with-test-output-dir",
            &[
                "--flatten-debug-output",
                "--debug-output",
                "{ROOT}/F",
                "--test-output-dir",
                "{ROOT}/T",
            ],
            false,
        ),
        // Flattened with only --test-output-dir (rewrite off): still the home directory.
        (
            "flattened-with-test-output-dir-alone",
            &["--flatten-debug-output", "--test-output-dir", "{ROOT}/T"],
            true,
        ),
    ];
    for (name, flags, rewrite_off) in cases {
        let (case_code, case_events) = live(name, flags, rewrite_off);
        assert_eq!(case_code, code, "{name}: the exit status is Maestro's");
        assert_eq!(
            settled(&case_events),
            want,
            "{name}: the live run settles the same steps"
        );
    }
}
