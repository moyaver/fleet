// SPDX-License-Identifier: AGPL-3.0-only
//! The offline bundle (issue #83): a run with no server keeps what it would have sent, and the
//! bundle holds no secret the run did not show (I6). Maestro's output and exit status are the same
//! as without it (I1, I2).

use std::{collections::BTreeMap, fs, io::Read, path::Path};

use fleet_core::redact::MARKER;
use serde_json::Value;

use std::process::{Command, Stdio};

use super::stream::{Mode, fixture, run_agent, server, types};

/// The secret the `fail` fixture's log, commands and JUnit report all show.
const PLANTED: &str = "Fleet never shows this";

/// Every entry of a bundle, by name.
fn entries(bundle: &Path) -> BTreeMap<String, Vec<u8>> {
    let file = fs::File::open(bundle).unwrap();
    let decoder = ruzstd::decoding::StreamingDecoder::new(file).unwrap();
    let mut archive = tar::Archive::new(decoder);
    let mut out = BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        out.insert(name, bytes);
    }
    out
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[test]
fn an_offline_run_keeps_a_bundle_and_its_exit_status() {
    let (out, _, run_dir) = run_agent("fail", None, &[], &[]);
    assert_eq!(out.status.code(), Some(1), "Maestro's own exit status (I1)");
    assert_eq!(out.stdout, fs::read(fixture("fail/stdout.bin")).unwrap());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("publish it later with `fleet-agent publish`"),
        "{stderr}"
    );

    let bundle = run_dir.join("fleet-bundle.tar.zst");
    assert!(bundle.is_file(), "no bundle in {}", run_dir.display());
    let all = entries(&bundle);
    let manifest: Value = serde_json::from_slice(&all["manifest.json"]).unwrap();
    assert_eq!(manifest["format"], 1);
    let classes: Vec<&str> = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["class"].as_str().unwrap())
        .collect();
    for class in ["failure_screenshot", "commands", "maestro_log"] {
        assert!(classes.contains(&class), "{class} in {classes:?}");
    }
    // The journal is the run's events, in order, and it ends the run.
    let journal = String::from_utf8(all["journal.jsonl"].clone()).unwrap();
    let last: Value = serde_json::from_str(journal.lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "run.ended", "{journal}");
    // Every artifact in the manifest is in the bundle, with the size and digest it names.
    for a in manifest["artifacts"].as_array().unwrap() {
        let bytes = &all[a["file"].as_str().unwrap()];
        assert_eq!(bytes.len() as u64, a["size"].as_u64().unwrap());
    }
}

#[test]
fn the_bundle_holds_no_planted_secret_and_shows_where_it_was_redacted() {
    let (out, _, run_dir) = run_agent("fail", None, &[], &[("MAESTRO_PLANTED", PLANTED)]);
    assert_eq!(out.status.code(), Some(1));
    let all = entries(&run_dir.join("fleet-bundle.tar.zst"));
    for (name, bytes) in &all {
        assert!(!contains(bytes, PLANTED), "{name} holds the planted value");
    }
    let manifest: Value = serde_json::from_slice(&all["manifest.json"]).unwrap();
    let log = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["class"] == "maestro_log")
        .expect("the maestro log");
    let text = String::from_utf8_lossy(&all[log["file"].as_str().unwrap()]).into_owned();
    assert!(
        text.contains(MARKER),
        "the log should show where it was redacted"
    );
}

/// `fleet-agent publish <path>` against the fake server, in a cleared environment.
fn publish(path: &Path, ws: &str) -> std::process::Output {
    Command::new(assert_cmd::cargo::cargo_bin!("fleet-agent"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", std::env::var_os("HOME").unwrap_or_default())
        .env("TMPDIR", std::env::temp_dir())
        .env("FLEET_SERVER", ws)
        .env("FLEET_FLUSH_TIMEOUT", "15")
        // The fake server never answers an upload: the uploads are given up on at once.
        .env("FLEET_UPLOAD_TIMEOUT", "2")
        .arg("publish")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn a_bundle_publishes_its_journal_as_the_run_it_recorded() {
    let (out, _, run_dir) = run_agent("fail", None, &[], &[]);
    assert_eq!(out.status.code(), Some(1));
    let journal = fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
    let recorded: Vec<String> = journal
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|v| v["type"].as_str().unwrap().to_owned())
        .collect();

    let srv = server(Mode::Normal);
    let ws = format!("ws://{}", srv.addr);
    let out = publish(&run_dir.join("fleet-bundle.tar.zst"), &ws);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.starts_with("published run "), "{stdout}");

    let s = srv.state.lock().unwrap();
    // Every recorded event reached the server, in order and gap-free, and the run ended last.
    let seqs: Vec<u64> = s.committed.keys().copied().collect();
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<_>>(),
        "I5: gap-free from 1"
    );
    let sent: Vec<String> = s
        .committed
        .values()
        .map(|e| e["type"].as_str().unwrap().to_owned())
        .collect();
    let kept: Vec<&String> = sent
        .iter()
        .filter(|t| !t.starts_with("artifact."))
        .collect();
    let recorded_kept: Vec<&String> = recorded
        .iter()
        .filter(|t| !t.starts_with("artifact."))
        .collect();
    assert_eq!(kept, recorded_kept, "the recorded events, in order");
    assert_eq!(
        sent.last().map(String::as_str),
        Some("run.ended"),
        "{sent:?}"
    );
    assert_eq!(s.hellos[0]["ci"]["job_key"], "job");
    assert_eq!(types(&s).get("run.ended"), Some(&1));
}

#[test]
fn a_maestro_output_directory_publishes_its_run_end_last() {
    let srv = server(Mode::Normal);
    let ws = format!("ws://{}", srv.addr);
    let out = publish(&fixture("fail"), &ws);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = srv.state.lock().unwrap();
    let t = types(&s);
    assert_eq!(t.get("flow.reported"), Some(&1), "{t:?}");
    assert_eq!(t.get("run.ended"), Some(&1), "{t:?}");
    let seqs: Vec<u64> = s.committed.keys().copied().collect();
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<_>>(),
        "I5: gap-free from 1"
    );
    let last = s.committed.values().last().unwrap();
    assert_eq!(last["type"], "run.ended");
    // The agent run is new: it is not the one a live run of this directory would have had.
    assert_ne!(s.hellos[0]["agent_run_id"], Value::Null);
}
