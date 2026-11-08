// SPDX-License-Identifier: AGPL-3.0-only
//! Gap 1 (issue #148): a live run settles its steps wherever the user's own flags put Maestro's
//! files. The fake Maestro writes each layout where the real CLI does (research 01 §4.1; the
//! captured trees in `fixtures/maestro/2.12.1`), and the agent's watcher must follow it there.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
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

/// One `fleet-agent run -- maestro test …` whose fake Maestro replays `fixture_name`. `{ROOT}` in a
/// flag is the test's root directory. The environment is hermetic: `HOME` is the test's, and
/// `user.home` is pinned to the same directory with `JAVA_OPTS`, as the real JVM would take it.
struct Run {
    root: PathBuf,
    name: String,
    fixture_name: &'static str,
    flags: Vec<String>,
    dir: PathBuf,
    /// `FLEET_FAKE_STAMP`: the run directory's name, when a test needs a particular one.
    stamp: Option<&'static str>,
    /// `FLEET_FAKE_DELAY`: seconds before the fake makes its directory.
    delay: Option<&'static str>,
    /// `FLEET_FAKE_LINGER`: seconds the fake runs after its output. The agent finds a run's directory
    /// by the process that holds its log, so a fake that exits before the agent's first look is not
    /// attached; a second is ample for the first look, as Maestro runs for seconds.
    linger: Option<&'static str>,
}

impl Run {
    fn new(root: &Path, name: &str, fixture_name: &'static str, flags: &[&str]) -> Self {
        let root_text = root.to_string_lossy().into_owned();
        Self {
            root: root.to_path_buf(),
            name: name.to_owned(),
            fixture_name,
            flags: flags
                .iter()
                .map(|f| f.replace("{ROOT}", &root_text))
                .collect(),
            dir: root.join(format!("run-{name}")),
            stamp: None,
            delay: None,
            linger: None,
        }
    }

    fn spawn(&self, rewrite_off: bool, config_test_dir: Option<&str>) -> Child {
        let bin = self.root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let fake = bin.join("maestro");
        if !fake.exists() {
            std::os::unix::fs::symlink(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/it/fake-maestro.sh"),
                &fake,
            )
            .unwrap();
        }
        // A directory of flows: a suite, as in the other live tests. A config in it is the
        // workspace Maestro reads for `testOutputDir`.
        let flows = self.root.join(format!("flows-{}", self.name));
        fs::create_dir_all(&flows).unwrap();
        if let Some(dir) = config_test_dir {
            fs::write(flows.join("config.yaml"), format!("testOutputDir: {dir}\n")).unwrap();
        }
        let home = self.root.join("home");
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("fleet-agent"));
        // Hermetic: the environment is cleared, then only what the run needs is kept.
        cmd.env_clear();
        for keep in ["PATH", "TMPDIR"] {
            if let Some(v) = std::env::var_os(keep) {
                cmd.env(keep, v);
            }
        }
        cmd.env("HOME", &home)
            .env(
                "JAVA_OPTS",
                format!("-Duser.home={}", home.to_string_lossy()),
            )
            .env("FLEET_FAKE_FIXTURE", fixture(self.fixture_name))
            .env("FLEET_FAKE_LINGER", self.linger.unwrap_or("1"))
            .env("FLEET_FLUSH_TIMEOUT", "8")
            .env("FLEET_REWRITE", if rewrite_off { "off" } else { "auto" })
            .args(["run", "--finalize-timeout", "30", "--run-dir"])
            .arg(&self.dir)
            .arg("--")
            .arg(fake)
            .arg("test")
            .args(&self.flags)
            .arg(&flows)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(stamp) = self.stamp {
            cmd.env("FLEET_FAKE_STAMP", stamp);
        }
        if let Some(delay) = self.delay {
            cmd.env("FLEET_FAKE_DELAY", delay);
        }
        cmd.spawn().unwrap()
    }
}

/// The exit status and the journal the agent wrote for a finished run.
fn finish(run: &Run, child: Child) -> (Option<i32>, Vec<EventEnvelope>) {
    let out = child.wait_with_output().unwrap();
    let text = fs::read_to_string(run.dir.join("journal.jsonl")).unwrap();
    let events = text
        .lines()
        .map(|l| serde_json::from_str::<EventEnvelope>(l).unwrap())
        .collect();
    (out.status.code(), events)
}

/// Runs one layout to the end: `flags` after `test`, with `{ROOT}` in them.
fn live(name: &str, flags: &[&str], rewrite_off: bool) -> (Option<i32>, Vec<EventEnvelope>) {
    let root = std::env::temp_dir().join(format!("fleet-layout-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let run = Run::new(&root, name, "pass", flags);
    let child = run.spawn(rewrite_off, None);
    finish(&run, child)
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
    let cases: [(&str, &[&str], bool); 11] = [
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
        // paths-debug-and-test-output-apart: the log follows --debug-output, the bundles --test-output-dir.
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
        // paths-flatten-alone: --flatten-debug-output alone writes into user.home.
        (
            "flattened-alone-rewrite-off",
            &["--flatten-debug-output"],
            true,
        ),
        // The agent adds its own debug directory, and must not add a second flatten flag: Maestro
        // rejects a repeated one with exit 2 (paths-flatten-repeated).
        (
            "flattened-alone-rewrite-on",
            &["--flatten-debug-output"],
            false,
        ),
        // paths-flatten-debug-and-test-output: flattened, --test-output-dir is ignored.
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
        // paths-flatten-test-output-dir: flattened with only --test-output-dir (rewrite off): user.home.
        (
            "flattened-with-test-output-dir-alone",
            &["--flatten-debug-output", "--test-output-dir", "{ROOT}/T"],
            true,
        ),
        // paths-debug-and-test-output-apart with the default log and bundles split by the agent.
        (
            "test-output-dir-only",
            &["--test-output-dir", "{ROOT}/T2"],
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

/// A workspace config's `testOutputDir` is the bundles' directory, as the flag is (paths-config-*).
#[test]
fn a_workspace_config_sets_the_bundles_directory_and_the_run_follows_it() {
    let root = std::env::temp_dir().join(format!("fleet-layout-{}-config", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let run = Run::new(&root, "config", "pass", &[]);
    let child = run.spawn(
        false,
        Some(
            "{ROOT}/C"
                .replace("{ROOT}", &root.to_string_lossy())
                .as_str(),
        ),
    );
    let (code, events) = finish(&run, child);
    assert_eq!(code, Some(0), "Maestro's exit status (I1)");
    let settled = settled(&events);
    assert!(!settled.is_empty(), "the config's directory settles steps");
    assert!(
        settled.iter().all(|(_, s)| s == "completed"),
        "the pass fixture completes every step: {settled:?}"
    );
}

/// Two runs in one tree at once (issue #148): each settles its own flow, not the other's. The
/// second run's directory is made late, and the first run's is made first, so "the oldest directory
/// created since the spawn" would give the second run the first run's files. The pass fixture and
/// the fail fixture settle different statuses, so a wrong directory shows.
#[test]
fn two_runs_in_one_tree_each_settle_their_own_flow() {
    let root = std::env::temp_dir().join(format!("fleet-layout-{}-concurrent", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    // The solo runs each have a tree of their own: two runs in one tree within a second would share
    // a directory, which is Maestro's own collision and not this test's subject.
    let solo_flags = |name: &str| {
        let tree = format!("{}/solo-{name}", root.to_string_lossy());
        [String::from("--debug-output"), tree]
    };
    let flags_of = |tree: &str| ["--debug-output".to_owned(), tree.to_owned()];
    let solo_pass = live_with("solo-pass", "pass", &solo_flags("pass"));
    let solo_fail = live_with("solo-fail", "fail", &solo_flags("fail"));
    assert_ne!(
        settled(&solo_pass.1),
        settled(&solo_fail.1),
        "the two fixtures settle different steps"
    );

    let tree = format!("{}/D", root.to_string_lossy());
    let shared = flags_of(&tree);
    let shared: Vec<&str> = shared.iter().map(String::as_str).collect();
    // Both runs stay alive a while: the agent looks for its own directory by the process that holds
    // the log, so a run that exits before its look is found is not attached (as in life, Maestro
    // runs for seconds after it makes its directory).
    let mut late = Run::new(&root, "late", "fail", &shared);
    late.stamp = Some("2026-11-08_100002");
    late.delay = Some("1.5");
    late.linger = Some("2");
    let mut early = Run::new(&root, "early", "pass", &shared);
    early.stamp = Some("2026-11-08_100001");
    early.linger = Some("2");
    let late_child = late.spawn(false, None);
    thread::sleep(Duration::from_millis(200));
    let early_child = early.spawn(false, None);
    let (_, early_events) = finish(&early, early_child);
    let (_, late_events) = finish(&late, late_child);
    assert_eq!(
        settled(&early_events),
        settled(&solo_pass.1),
        "the first run settles its own flow"
    );
    assert_eq!(
        settled(&late_events),
        settled(&solo_fail.1),
        "the second run settles its own flow, not the first run's"
    );
}

/// One run of `fixture_name` with the flags given, in a root of its own.
fn live_with(
    name: &str,
    fixture_name: &'static str,
    flags: &[String],
) -> (Option<i32>, Vec<EventEnvelope>) {
    let root = std::env::temp_dir().join(format!("fleet-layout-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let run = Run::new(&root, name, fixture_name, &flags);
    let child = run.spawn(false, None);
    finish(&run, child)
}
