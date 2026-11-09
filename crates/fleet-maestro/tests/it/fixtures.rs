// SPDX-License-Identifier: AGPL-3.0-only
//! One snapshot per fixture of what every parser extracts. Review changes with
//! `cargo insta review`; a changed snapshot means a parser now reads real output differently.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use fleet_maestro::{
    bundle,
    invocation::{parse, requested_html_report},
    junit, log, stdout,
};
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
    let mut runner_version = None;
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
            Some(log::Event::Step { state, .. } | log::Event::CommandStep { state, .. }) => {
                *step_ends.entry(format!("{state:?}")).or_default() += 1;
            }
            Some(log::Event::MaestroVersion { version }) => runner_version = Some(version),
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
        "runner_version": runner_version,
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
    assert_eq!(names.len(), 45, "{names:?}");
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

/// Each captured HTML run names, through `requested_html_report` on its own command line, the file
/// the capture holds: at `--output` when there is one, and otherwise at Maestro's default
/// `report.html` in its working directory (the default-path captures run Maestro from `run/`).
#[test]
fn the_html_report_a_command_names_is_the_file_maestro_wrote() {
    for name in [
        "html-report",
        "html-report-default-path",
        "html-detailed",
        "html-detailed-default-path",
        "html-test-output-dir",
        "html-no-debug-flags",
    ] {
        let dir = fixtures_root().join(name);
        let meta: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap();
        let command: Vec<String> = meta["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        // The default-path captures run Maestro through `sh -c 'cd … && exec "$@"'`: its argv
        // starts at the Maestro binary.
        let maestro = command
            .iter()
            .position(|a| a.ends_with("/maestro"))
            .unwrap();
        let named = requested_html_report(&parse(&command[maestro..]))
            .unwrap_or_else(|| panic!("{name}: no HTML report named"));
        let file = match named.strip_prefix("<CAPTURE>/") {
            Some(rel) => dir.join(rel),
            None => dir.join("run").join(&named),
        };
        assert!(
            file.is_file(),
            "{name}: {named} is not in the capture ({})",
            file.display()
        );
    }
}

/// Every `commands.json` under `dir`, at any depth.
fn commands_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            commands_files(&p, out);
        } else if p.file_name().is_some_and(|n| n == "commands.json") {
            out.push(p);
        }
    }
}

/// Every flow's console steps, as the log started them: the RUNNING descriptions, in order, keyed
/// by flow name and shard. Compared with the flow's `commands.json`, so the step-to-kind mapping
/// is checked against what Maestro really wrote (issue #148).
#[test]
fn a_step_description_names_the_kind_of_the_command_it_runs() {
    let mut names: Vec<_> = fs::read_dir(fixtures_root())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    names.sort();
    let mut checked = 0;
    let mut unchecked = Vec::new();
    for dir in names {
        let Some(path) = session_log(&dir) else {
            continue;
        };
        let mut a = log::Assembler::default();
        let mut records: Vec<log::Record> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter_map(|l| a.push(l))
            .collect();
        records.extend(a.finish());
        let mut running: BTreeMap<(String, u32), Vec<String>> = BTreeMap::new();
        let mut current: BTreeMap<u32, String> = BTreeMap::new();
        for r in &records {
            match log::classify(r) {
                Some(log::Event::FlowStarted { shard, name }) => {
                    current.insert(shard.unwrap_or(1), name);
                }
                Some(log::Event::Step {
                    shard,
                    description,
                    state: log::StepState::Running,
                }) => {
                    let shard = shard.unwrap_or(1);
                    if let Some(name) = current.get(&shard) {
                        running
                            .entry((name.clone(), shard))
                            .or_default()
                            .push(description);
                    }
                }
                _ => {}
            }
        }
        let mut files = Vec::new();
        commands_files(&dir, &mut files);
        for file in files {
            let flow_dir = file
                .parent()
                .and_then(Path::file_name)
                .unwrap()
                .to_string_lossy();
            let (name, shard) = bundle::split_shard_suffix(&flow_dir);
            let bundle::Commands::Final(commands) =
                bundle::parse_commands(&fs::read(&file).unwrap())
            else {
                continue;
            };
            let key = (name.to_owned(), shard.unwrap_or(1));
            let Some(steps) = running.get(&key) else {
                // Maestro's default log format (no machine pattern) has no console step lines.
                let default_format = dir.file_name().is_some_and(|n| {
                    n.to_string_lossy().starts_with("paths-") || n == "single-flow-no-format"
                });
                if !default_format {
                    unchecked.push(format!("{}: {name} has no logged steps", dir.display()));
                }
                continue;
            };
            if steps.len() != commands.len() {
                unchecked.push(format!(
                    "{}: {name} logged {} steps for {} commands",
                    dir.display(),
                    steps.len(),
                    commands.len()
                ));
                continue;
            }
            for (description, command) in steps.iter().zip(&commands) {
                assert_eq!(
                    log::command_kind(description),
                    command.kind.as_deref(),
                    "{}: {description:?} is command {}",
                    dir.display(),
                    command.sequence
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 200, "only {checked} steps were checked");
    assert_eq!(unchecked, Vec::<String>::new());
}

/// Maestro 2.12.1 rejects a repeated `--flatten-debug-output` with exit 2 and a usage message, so
/// the agent must never add a second one to a command that has it (issue #148).
#[test]
fn a_repeated_flatten_flag_is_a_usage_error_in_maestro() {
    let dir = fixtures_root().join("paths-flatten-repeated");
    let exit: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("exit.json")).unwrap()).unwrap();
    assert_eq!(exit["code"], 2, "picocli's usage error");
    let stderr = fs::read_to_string(dir.join("stderr.bin")).unwrap();
    assert!(
        stderr.contains("option '--flatten-debug-output' should be specified only once"),
        "{stderr}"
    );
    assert!(
        !dir.join("run").exists(),
        "Maestro wrote nothing: the command never ran"
    );
}
