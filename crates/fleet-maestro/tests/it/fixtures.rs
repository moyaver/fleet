// SPDX-License-Identifier: AGPL-3.0-only
//! One snapshot per fixture of what every parser extracts. Review changes with
//! `cargo insta review`; a changed snapshot means a parser now reads real output differently.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use fleet_maestro::{bundle, junit, log, stdout};
use serde_json::{Value, json};

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/maestro/2.12.1")
}

/// The session `maestro.log` of a fixture, wherever the scenario's flags put it.
fn session_log(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.join("run"), dir.join("state")];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() && p.file_name().is_some_and(|n| n != "logs") {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n == "maestro.log")
                && !p.parent()?.ends_with("logs")
            {
                return Some(p);
            }
        }
    }
    None
}

fn parse_log(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap();
    let mut a = log::Assembler::default();
    let mut records: Vec<log::Record> = text.lines().filter_map(|l| a.push(l)).collect();
    records.extend(a.finish());
    let mut kinds: BTreeMap<String, u32> = BTreeMap::new();
    let mut shards = Vec::new();
    let mut flows = Vec::new();
    let mut step_ends: BTreeMap<String, u32> = BTreeMap::new();
    let mut crashes = Vec::new();
    for r in &records {
        match log::classify(r) {
            Some(log::Event::ShardAssigned {
                shard,
                serial,
                flows,
            }) => shards.push(json!({ "shard": shard, "serial": serial, "planned": flows.len() })),
            Some(log::Event::FlowStarted { shard, name }) => {
                flows.push(json!({ "shard": shard, "name": name }));
            }
            Some(log::Event::Step { state, .. }) => {
                *step_ends.entry(format!("{state:?}")).or_default() += 1;
            }
            Some(log::Event::RetryAttemptFailed { attempt }) => {
                *kinds
                    .entry(format!("retry_attempt_failed_{attempt}"))
                    .or_default() += 1;
            }
            Some(log::Event::FlowCrashed { exception }) => crashes.push(
                exception
                    .unwrap_or_default()
                    .split(':')
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
            ),
            None => {}
        }
    }
    json!({
        "records": records.len(),
        "machine_pattern": records.iter().all(|r| r.unix_ms.is_some()),
        "with_continuation": records.iter().filter(|r| !r.continuation.is_empty()).count(),
        "orphan_lines": a.orphan_lines,
        "shards": shards,
        "flows_started": flows,
        "steps": step_ends,
        "other": kinds,
        "crashes": crashes,
    })
}

fn parse_stdout(path: &Path) -> Value {
    let text = String::from_utf8_lossy(&fs::read(path).unwrap()).into_owned();
    let mut results = Vec::new();
    let mut other = Vec::new();
    let mut unrecognized = 0;
    for line in text.lines() {
        match stdout::classify(&strip_ansi(line)) {
            stdout::Line::FlowResult {
                shard,
                name,
                passed,
                duration_ms,
                message,
            } => {
                results.push(json!({ "shard": shard, "name": name, "passed": passed, "duration_ms": duration_ms, "message": message }));
            }
            stdout::Line::Noise => {}
            stdout::Line::Unrecognized => unrecognized += 1,
            l => other.push(format!("{l:?}")),
        }
    }
    json!({ "flow_results": results, "other": other, "unrecognized": unrecognized })
}

/// Strips CSI escape sequences (the TTY fixtures have colour).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_bundles(debug: &Path) -> Value {
    let mut flows = BTreeMap::new();
    let Ok(entries) = fs::read_dir(debug) else {
        return json!({});
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let bundle::Entry::FlowDir { flow, shard } = bundle::classify(&name) else {
            continue;
        };
        let commands = match fs::read(e.path().join("commands.json")) {
            Ok(b) => match bundle::parse_commands(&b) {
                bundle::Commands::Final(v) => format!("final({})", v.len()),
                bundle::Commands::Aborted(v) => format!("aborted({})", v.len()),
                bundle::Commands::Unverified(v) => format!("unverified({})", v.len()),
                bundle::Commands::Empty => "empty".into(),
                bundle::Commands::Invalid => "invalid".into(),
            },
            Err(_) => "absent".into(),
        };
        let shots = fs::read_dir(e.path().join("screenshots")).map(|d| {
            d.flatten()
                .filter(|s| {
                    matches!(
                        bundle::classify(&format!(
                            "{name}/screenshots/{}",
                            s.file_name().to_string_lossy()
                        )),
                        bundle::Entry::StepScreenshot { .. }
                    )
                })
                .count()
        });
        flows.insert(
            name.clone(),
            json!({
                "flow": flow, "shard": shard, "commands": commands,
                "manifest": e.path().join("manifest.json").exists(),
                "failure_screenshots": shots.unwrap_or(0),
            }),
        );
    }
    json!(flows)
}

fn parse_report(path: &Path) -> Value {
    let Ok(bytes) = fs::read(path) else {
        return json!("absent");
    };
    let r = junit::parse(&bytes);
    json!({
        "status": format!("{:?}", r.status),
        "suites": r.suites.iter().map(|s| json!({
            "device": s.device,
            "cases": s.cases.iter().map(|c| json!({ "name": c.name, "outcome": format!("{:?}", junit::outcome(c.status.as_deref())), "has_failure": c.failure.is_some() })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

#[test]
fn every_fixture_parses() {
    let mut names: Vec<_> = fs::read_dir(fixtures_root())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names.len(), 24, "{names:?}");
    for name in names {
        let dir = fixtures_root().join(&name);
        let debug = dir.join("run/debug");
        let summary = json!({
            "exit": serde_json::from_str::<Value>(&fs::read_to_string(dir.join("exit.json")).unwrap()).unwrap()["code"],
            "log": session_log(&dir).map_or(json!(null), |p| parse_log(&p)),
            "stdout": parse_stdout(&dir.join("stdout.bin")),
            "bundles": parse_bundles(&debug),
            "report": parse_report(&dir.join("run/report.xml")),
        });
        insta::assert_json_snapshot!(name, summary);
    }
}
