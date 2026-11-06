// SPDX-License-Identifier: AGPL-3.0-only
//! `fleet-agent inspect <dir>`: run the live session over a saved run, offline (PLAN §6.1).
//!
//! A capture from `cargo xtask fixtures capture` has a recorded timeline (`events.jsonl`), which is
//! replayed in order — exactly what the observer would have fed the session live. A plain saved
//! run (a debug output directory with `maestro.log`, flow bundles and `report.xml`) is fed in file
//! order: log, bundles, report.
//!
//! A run directory with a `journal.jsonl` (the agent's own record of the run, see
//! [`crate::journal`]) also reports what that journal holds. When the directory has no saved
//! Maestro output to replay, the journal is all there is to inspect, and its events are the
//! result. A journal that stopped early (at its size cap, or on a failed write) says so, and from
//! which `seq` events are missing.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context;
use fleet_core::{claims::FlowState, redact::Redactor};
use fleet_maestro::{ReportState, log};
use fleet_protocol::journal::{ClockSample, EventEnvelope};
use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    journal::{self, Truncation},
    session::{Observation, Session},
};

/// One flow in the result.
#[derive(Debug, Clone, Serialize)]
pub struct FlowResult {
    /// 1-based shard.
    pub shard: u32,
    /// Flow name.
    pub name: String,
    /// Flow file, when known.
    pub path: Option<String>,
    /// Resolved state: `pending`, `running`, or the outcome.
    pub outcome: String,
    /// How settled: `provisional`, `verified`, `unverified_final`, `inferred`.
    pub finality: String,
    /// The winning source.
    pub source: Option<String>,
    /// Two parsed sources disagreed.
    pub conflict: bool,
    /// Steps seen live.
    pub steps: u32,
    /// Failure message (redacted).
    pub failure: Option<String>,
}

/// What `inspect` found.
#[derive(Debug, Clone, Serialize)]
pub struct Inspection {
    /// Every flow execution, in the order first seen.
    pub flows: Vec<FlowResult>,
    /// Journal events the session produced.
    pub events: usize,
    /// Event types and counts.
    pub event_types: std::collections::BTreeMap<String, usize>,
    /// Stdout lines no parser recognised (Maestro output drift).
    pub unrecognized_stdout: u32,
    /// What `journal.jsonl` holds, when the directory has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal: Option<JournalStatus>,
}

/// What a run directory's `journal.jsonl` holds.
#[derive(Debug, Clone, Serialize)]
pub struct JournalStatus {
    /// Events in the journal, before any truncation marker.
    pub events: usize,
    /// Set when the journal stopped early (its size cap, or a failed write): it holds no event
    /// from `first_missing_seq` on, and those events are missing from it.
    pub truncated: Option<Truncation>,
}

/// Inspects a saved run directory.
///
/// # Errors
/// Returns an error when the directory has neither a capture timeline, a `maestro.log` nor a
/// `journal.jsonl`, or when a file it should read cannot be read.
pub fn inspect(dir: &Path) -> anyhow::Result<(Inspection, Vec<EventEnvelope>)> {
    let journal = read_journal(dir)?;
    let replay = if dir.join("events.jsonl").exists() {
        Replay::from_capture(dir)?
    } else if let (Some(journal), None) = (&journal, saved_log(dir)) {
        return Ok(from_journal(journal));
    } else {
        Replay::from_saved_run(dir)?
    };
    let mut counter: u128 = 0;
    let ids = Box::new(move || {
        counter += 1;
        Uuid::from_u128(counter)
    });
    let t0 = replay.start;
    let at = |ms: u64| ClockSample {
        wall: t0 + time::Duration::milliseconds(i64::try_from(ms).unwrap_or(i64::MAX)),
        mono_ms: ms,
    };
    let mut session = Session::new(&replay.argv, None, replay.redactor, ids, at(0));
    let mut last = 0;
    for (ms, obs) in replay.timeline {
        last = ms;
        session.observe(at(ms), obs);
    }
    session.end(at(last), 0);
    let events = session.drain();
    let mut event_types = std::collections::BTreeMap::new();
    for e in &events {
        *event_types.entry(e.kind.clone()).or_default() += 1;
    }
    let flows = session
        .summary()
        .into_iter()
        .map(|(f, r)| FlowResult {
            shard: f.shard,
            name: f.name,
            path: f.path,
            outcome: match r.state {
                FlowState::Pending => "pending".into(),
                FlowState::Running => "running".into(),
                FlowState::Ended(o) => snake(&format!("{o:?}")),
            },
            finality: snake(&format!("{:?}", r.finality)),
            source: r.source.map(|s| snake(&format!("{s:?}"))),
            conflict: r.conflict,
            steps: f.steps,
            failure: f.failure,
        })
        .collect();
    let inspection = Inspection {
        flows,
        events: events.len(),
        event_types,
        unrecognized_stdout: session.unrecognized_stdout,
        journal: journal.as_ref().map(journal_status),
    };
    Ok((inspection, events))
}

/// The directory's `journal.jsonl`, if it has one.
fn read_journal(dir: &Path) -> anyhow::Result<Option<journal::Journal>> {
    let path = dir.join("journal.jsonl");
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(journal::parse(&text)))
}

fn journal_status(journal: &journal::Journal) -> JournalStatus {
    JournalStatus {
        events: journal.events.len(),
        truncated: journal.truncated.clone(),
    }
}

/// A directory with only a journal: its events, as they were recorded, are the inspection.
fn from_journal(journal: &journal::Journal) -> (Inspection, Vec<EventEnvelope>) {
    let mut event_types = std::collections::BTreeMap::new();
    for e in &journal.events {
        *event_types.entry(e.kind.clone()).or_default() += 1;
    }
    let inspection = Inspection {
        flows: Vec::new(),
        events: journal.events.len(),
        event_types,
        unrecognized_stdout: 0,
        journal: Some(journal_status(journal)),
    };
    (inspection, journal.events.clone())
}

/// A human-readable table of an inspection.
#[must_use]
pub fn table(i: &Inspection) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<5} {:<28} {:<12} {:<16} {:<13} FAILURE",
        "SHARD", "FLOW", "OUTCOME", "FINALITY", "SOURCE"
    );
    for f in &i.flows {
        let failure: String = f
            .failure
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
        let conflict = if f.conflict { " (conflict)" } else { "" };
        let _ = writeln!(
            out,
            "{:<5} {:<28} {:<12} {:<16} {:<13} {failure}{conflict}",
            f.shard,
            f.name,
            f.outcome,
            f.finality,
            f.source.as_deref().unwrap_or("-")
        );
    }
    let _ = writeln!(
        out,
        "{} events, {} unrecognized stdout lines",
        i.events, i.unrecognized_stdout
    );
    if let Some(j) = &i.journal {
        match &j.truncated {
            None => {
                let _ = writeln!(out, "journal.jsonl: {} events, complete", j.events);
            }
            Some(t) => {
                let why = if t.reason == journal::REASON_BUDGET {
                    "at its size cap"
                } else {
                    "on a write error"
                };
                let _ = writeln!(
                    out,
                    "journal.jsonl is truncated: it stopped {why} ({}) after {} bytes, holding {} events. Events from seq {} on are missing from it.",
                    t.reason, t.bytes_written, j.events, t.first_missing_seq
                );
            }
        }
    }
    out
}

fn snake(debug: &str) -> String {
    let mut out = String::new();
    for (i, c) in debug.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

struct Replay {
    argv: Vec<String>,
    redactor: Redactor,
    start: OffsetDateTime,
    timeline: Vec<(u64, Observation)>,
}

impl Replay {
    /// A capture: `meta.json`, `events.jsonl`, `exit.json`, `run/…`.
    fn from_capture(dir: &Path) -> anyhow::Result<Self> {
        let meta: Value = serde_json::from_str(
            &fs::read_to_string(dir.join("meta.json")).context("reading meta.json")?,
        )?;
        let mut argv: Vec<String> = meta["command"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        // Captures under `script` wrap the real command.
        if let Some(pos) = argv
            .iter()
            .position(|a| a.ends_with("/maestro") || a.ends_with("bin/maestro"))
        {
            argv.drain(..pos);
        }
        let env: Vec<(String, String)> = meta["env"]
            .as_array()
            .into_iter()
            .chain(meta["default_env"].as_array())
            .flatten()
            .filter_map(|e| {
                Some((
                    e["name"].as_str()?.to_owned(),
                    e["value"].as_str()?.to_owned(),
                ))
            })
            .collect();
        let redactor = Redactor::for_run(&argv, env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let start_ms = meta["started_at_unix_ms"].as_u64().unwrap_or(0);
        let start = OffsetDateTime::from_unix_timestamp_nanos(i128::from(start_ms) * 1_000_000)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);

        let session_log = find_session_log(dir);
        let session_rel = session_log
            .as_deref()
            .and_then(|p| p.strip_prefix(dir).ok())
            .map(|p| p.to_string_lossy().into_owned());
        let debug_rel = session_rel
            .as_deref()
            .and_then(|r| r.rsplit_once('/'))
            .map(|(d, _)| format!("{d}/"));

        let (mut timeline, last_ms) =
            recorded_timeline(dir, session_rel.as_deref(), debug_rel.as_deref())?;
        let exit: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("exit.json")).unwrap_or_default())
                .unwrap_or_default();
        let code = exit["code"].as_i64().and_then(|c| i32::try_from(c).ok());
        let signal = exit["signal"].as_i64().and_then(|c| i32::try_from(c).ok());
        let end = exit["duration_ms"].as_u64().unwrap_or(last_ms).max(last_ms);
        let report = report_path(&argv, dir).and_then(|p| fs::read(p).ok());
        timeline.push((
            end,
            Observation::Exited {
                code,
                signal,
                report: match (&report, report_path(&argv, dir)) {
                    (Some(_), _) => ReportState::Present,
                    (None, Some(_)) => ReportState::Absent,
                    (None, None) => ReportState::NotRequested,
                },
            },
        ));
        timeline.push((end, Observation::Report(report)));
        Ok(Self {
            argv,
            redactor,
            start,
            timeline,
        })
    }

    /// A saved debug output directory.
    fn from_saved_run(dir: &Path) -> anyhow::Result<Self> {
        let log_path =
            saved_log(dir).with_context(|| format!("{} has no maestro.log", dir.display()))?;
        let debug = log_path.parent().unwrap_or(dir).to_path_buf();
        let mut timeline = Vec::new();
        let mut asm = log::Assembler::default();
        for line in fs::read_to_string(&log_path)?.lines() {
            if let Some(r) = asm.push(line) {
                timeline.push((0, Observation::Log(r)));
            }
        }
        timeline.extend(asm.finish().map(|r| (0, Observation::Log(r))));
        for e in fs::read_dir(&debug)?
            .flatten()
            .filter(|e| e.path().is_dir())
        {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Ok(bytes) = fs::read(e.path().join("commands.json")) {
                timeline.push((
                    0,
                    Observation::File {
                        rel: format!("{name}/commands.json"),
                        contents: Some(bytes),
                    },
                ));
                if e.path().join("manifest.json").exists() {
                    timeline.push((
                        0,
                        Observation::File {
                            rel: format!("{name}/manifest.json"),
                            contents: None,
                        },
                    ));
                }
            }
        }
        let report = [dir.join("report.xml"), debug.join("report.xml")]
            .into_iter()
            .find_map(|p| fs::read(p).ok());
        timeline.push((0, Observation::Report(report)));
        Ok(Self {
            argv: vec!["maestro".into(), "test".into()],
            redactor: Redactor::new([]),
            start: OffsetDateTime::UNIX_EPOCH,
            timeline,
        })
    }
}

/// A saved debug output directory's `maestro.log`, at the top or under `debug/`.
fn saved_log(dir: &Path) -> Option<PathBuf> {
    [dir.join("maestro.log"), dir.join("debug/maestro.log")]
        .into_iter()
        .find(|p| p.exists())
}

/// The observations recorded in a capture's `events.jsonl`, and the last timestamp.
fn recorded_timeline(
    dir: &Path,
    session_rel: Option<&str>,
    debug_rel: Option<&str>,
) -> anyhow::Result<(Vec<(u64, Observation)>, u64)> {
    let mut timeline = Vec::new();
    let mut asm = log::Assembler::default();
    let mut seen_commands = std::collections::HashSet::new();
    let mut last_ms = 0;
    for line in fs::read_to_string(dir.join("events.jsonl"))
        .context("reading events.jsonl")?
        .lines()
    {
        let Ok(e) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ms = e["t_ms"].as_u64().unwrap_or(last_ms);
        last_ms = ms;
        match e["src"].as_str() {
            Some("stdout") => timeline.push((
                ms,
                Observation::Stdout(e["line"].as_str().unwrap_or("").to_owned()),
            )),
            Some("log") if session_rel == e["file"].as_str() => {
                if let Some(r) = asm.push(e["line"].as_str().unwrap_or("")) {
                    timeline.push((ms, Observation::Log(r)));
                }
            }
            Some("fs") => {
                let (Some(debug), Some(path)) = (debug_rel, e["path"].as_str()) else {
                    continue;
                };
                let Some(rel) = path.strip_prefix(debug) else {
                    continue;
                };
                let contents = if rel.ends_with("commands.json") {
                    // Read once, at its first appearance; the capture kept the final bytes.
                    if !seen_commands.insert(rel.to_owned()) {
                        continue;
                    }
                    fs::read(dir.join(path)).ok()
                } else if e["event"] != "created" {
                    continue;
                } else {
                    None
                };
                timeline.push((
                    ms,
                    Observation::File {
                        rel: rel.to_owned(),
                        contents,
                    },
                ));
            }
            Some("signal") => {
                if let Some(sig) = signal_number(e["signal"].as_str().unwrap_or("")) {
                    timeline.push((ms, Observation::Signal(sig)));
                }
            }
            _ => {}
        }
    }
    if let Some(r) = asm.finish() {
        timeline.push((last_ms, Observation::Log(r)));
    }
    Ok((timeline, last_ms))
}

fn signal_number(name: &str) -> Option<i32> {
    match name {
        "INT" => Some(2),
        "TERM" => Some(15),
        "KILL" => Some(9),
        _ => None,
    }
}

/// The session `maestro.log` (not a per-flow `logs/maestro.log`).
fn find_session_log(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.join("run"), dir.join("state")];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() && p.file_name().is_some_and(|n| n != "logs") {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n == "maestro.log")
                && !p.parent().is_some_and(|q| q.ends_with("logs"))
            {
                return Some(p);
            }
        }
    }
    None
}

/// Where the capture's command wrote its JUnit report (`<CAPTURE>` is the capture directory).
fn report_path(argv: &[String], dir: &Path) -> Option<PathBuf> {
    let out = fleet_maestro::invocation::requested_report(&fleet_maestro::invocation::parse(argv))?;
    let rel = out.split_once("/run/").map_or(out.as_str(), |(_, r)| r);
    Some(dir.join("run").join(rel))
}
