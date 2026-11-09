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
    time::UNIX_EPOCH,
};

use anyhow::Context;
use fleet_core::{
    claims::FlowState,
    flaky::{DEVICE_LOST, Exclusion, exclusion_of},
    redact::Redactor,
};
use fleet_maestro::{ReportState, bundle::Commands, log};
use fleet_protocol::journal::{ClockSample, EventEnvelope};
use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    journal::{self, Truncation},
    session::{IdGen, Observation, Session},
};

/// One flow in the result.
#[derive(Debug, Clone, Serialize)]
pub struct FlowResult {
    /// The flow execution's ID, which the dashboard's screen D is addressed by. Not printed.
    #[serde(skip)]
    pub(crate) id: Uuid,
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
    /// Set when the device was lost during a failed flow, so the failure is the device's (issue
    /// #145): the lost-device exception it names. The outcome stays `failed`. The rule is the
    /// dashboard's: the failure names the device and the agent recorded that the flow was cut short.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub infrastructure: Option<String>,
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
    if !dir.join("events.jsonl").exists()
        && let (Some(journal), None) = (&journal, saved_log(dir))
    {
        return Ok(from_journal(journal));
    }
    let mut counter: u128 = 0;
    let ids = Box::new(move || {
        counter += 1;
        Uuid::from_u128(counter)
    });
    let mut replayed = replay(dir, ids)?;
    let end = replayed.at(replayed.last_ms);
    replayed.session.end(end, 0);
    let mut session = replayed.session;
    let events = session.drain();
    let mut event_types = std::collections::BTreeMap::new();
    for e in &events {
        *event_types.entry(e.kind.clone()).or_default() += 1;
    }
    let inspection = Inspection {
        flows: flow_results(&session),
        events: events.len(),
        event_types,
        unrecognized_stdout: session.unrecognized_stdout,
        journal: journal.as_ref().map(journal_status),
    };
    Ok((inspection, events))
}

/// A saved run fed through a live session. The session is not ended: the caller ends it, after
/// whatever it still has to add (publish uploads its artifacts first).
pub(crate) struct Replayed {
    /// The session that has seen every observation.
    pub(crate) session: Session,
    /// The time of the last observation, in milliseconds since the run started.
    pub(crate) last_ms: u64,
    /// Maestro's debug output directory: the upload candidates are relative to it.
    pub(crate) debug: Option<PathBuf>,
    start: OffsetDateTime,
}

impl Replayed {
    /// The clock of the run `ms` milliseconds after it started.
    pub(crate) fn at(&self, ms: u64) -> ClockSample {
        clock_at(self.start, ms)
    }
}

fn clock_at(start: OffsetDateTime, ms: u64) -> ClockSample {
    ClockSample {
        wall: start + time::Duration::milliseconds(i64::try_from(ms).unwrap_or(i64::MAX)),
        mono_ms: ms,
    }
}

/// Feeds the saved run in `dir` through a session, in the order the observer would have seen it:
/// a capture's recorded timeline, or a saved debug directory's files in file order.
///
/// # Errors
/// As [`inspect`]: the directory has no run to read, or a file it should read cannot be read.
pub(crate) fn replay(dir: &Path, ids: IdGen) -> anyhow::Result<Replayed> {
    let source = if dir.join("events.jsonl").exists() {
        Replay::from_capture(dir)?
    } else {
        Replay::from_saved_run(dir)?
    };
    let start = source.start;
    let mut session = Session::new(&source.argv, None, source.redactor, ids, clock_at(start, 0));
    let mut last_ms = 0;
    for (ms, obs) in source.timeline {
        last_ms = ms;
        session.observe(clock_at(start, ms), obs);
    }
    Ok(Replayed {
        session,
        last_ms,
        debug: source.debug,
        start,
    })
}

/// Every flow execution the session saw, resolved, in the order first seen.
pub(crate) fn flow_results(session: &Session) -> Vec<FlowResult> {
    session
        .summary()
        .into_iter()
        .map(|(f, r)| {
            // The same rule as the dashboard's (issue #145): a failure that names a lost device
            // (the rule flaky analysis uses, ADR-0009), and a flow whose commands were cut short.
            let lost = match r.state {
                FlowState::Ended(o) => exclusion_of(o, f.failure.as_deref()),
                FlowState::Pending | FlowState::Running => None,
            } == Some(Exclusion::Infrastructure)
                && f.cut_short.is_some();
            FlowResult {
                id: f.id,
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
                infrastructure: lost.then(|| DEVICE_LOST.to_owned()),
            }
        })
        .collect()
}

/// The reason a failed flow's row gives: the lost device first, then Maestro's message.
fn reason_of(f: &FlowResult) -> String {
    let message = f.failure.as_deref().unwrap_or_default();
    match f.infrastructure {
        Some(_) => format!("infrastructure (device lost): {message}"),
        None => message.to_owned(),
    }
}

/// Most flows the job summary lists; the rest are counted, not listed.
const SUMMARY_ROWS: usize = 50;

/// The job summary: Markdown for `GITHUB_STEP_SUMMARY`. It has the outcome counts and the flows
/// that did not pass with their provenance and reason. With a server (`server`, its address ending
/// in `/`), each flow links to its screen D through the server's `flows/{id}/page` redirect, and
/// the summary links to the dashboard.
#[must_use]
pub(crate) fn job_summary(flows: &[FlowResult], server: Option<&str>) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("## Fleet\n\n");
    if flows.is_empty() {
        out.push_str("No flow results were recorded.\n");
    } else {
        let mut counts = std::collections::BTreeMap::<&str, usize>::new();
        for f in flows {
            *counts.entry(f.outcome.as_str()).or_default() += 1;
        }
        let parts: Vec<String> = counts
            .iter()
            .map(|(outcome, n)| format!("{n} {outcome}"))
            .collect();
        let _ = writeln!(
            out,
            "Flows: {} ({} in total).",
            parts.join(", "),
            flows.len()
        );
    }
    let open: Vec<&FlowResult> = flows.iter().filter(|f| f.outcome != "passed").collect();
    if !open.is_empty() {
        out.push_str("\n| Flow | Outcome | Provenance | Reason |\n|---|---|---|---|\n");
        for f in open.iter().take(SUMMARY_ROWS) {
            let provenance = match &f.source {
                Some(source) => format!("{} ({source})", f.finality),
                None => f.finality.clone(),
            };
            let name = table_cell(&f.name, 80);
            let name = match server {
                Some(server) => format!(
                    "[{}]({server}api/v1/flows/{}/page)",
                    name.replace('[', "\\[").replace(']', "\\]"),
                    f.id
                ),
                None => name,
            };
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                name,
                f.outcome,
                provenance,
                table_cell(&reason_of(f), 200),
            );
        }
        if open.len() > SUMMARY_ROWS {
            let _ = writeln!(out, "\n{} more not listed.", open.len() - SUMMARY_ROWS);
        }
    }
    if let Some(server) = server {
        let _ = writeln!(out, "\n[Open the Fleet dashboard]({server})");
    }
    out
}

/// One Markdown table cell: a single line, pipes escaped, at most `max` characters.
fn table_cell(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line
        .chars()
        .take(max)
        .collect::<String>()
        .replace('|', "\\|")
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
        let failure: String = reason_of(f).chars().take(60).collect();
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
    debug: Option<PathBuf>,
}

impl Replay {
    /// A capture: `meta.json`, `events.jsonl`, `exit.json`, `run/…`.
    fn from_capture(dir: &Path) -> anyhow::Result<Self> {
        let meta: Value = serde_json::from_str(
            &fs::read_to_string(dir.join("meta.json")).context("reading meta.json")?,
        )?;
        let command: Vec<String> = meta["command"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        let argv = maestro_argv(&command);
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
        let redactor = fleet_maestro::redact::redactor_for_run(
            &argv,
            env.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        );
        let start_ms = meta["started_at_unix_ms"].as_u64().unwrap_or(0);
        let start = OffsetDateTime::from_unix_timestamp_nanos(i128::from(start_ms) * 1_000_000)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);

        let session_log = find_session_log(dir);
        let session_rel = session_log
            .as_deref()
            .and_then(|p| p.strip_prefix(dir).ok())
            .map(|p| p.to_string_lossy().into_owned());
        // The flow bundles sit where the layout model says, which is not always beside the session
        // log (`--test-output-dir`, `--debug-output` apart): see [`bundle_dir_rel`].
        let debug_rel = bundle_dir_rel(dir, &argv, session_rel.as_deref()).or_else(|| {
            session_rel
                .as_deref()
                .and_then(|r| r.rsplit_once('/'))
                .map(|(d, _)| format!("{d}/"))
        });

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
            debug: debug_rel.map(|d| dir.join(d)),
        })
    }

    /// A saved debug output directory. Its observations carry the times the run's own files
    /// record: each `maestro.log` record's Unix milliseconds (when the log has the machine
    /// pattern), and a flow's `commands.json` at the end of its last command, since the file is
    /// final by then. The run starts at the earliest of those. A directory that records no time at
    /// all falls back to the modification time of its log, and then to the Unix epoch.
    fn from_saved_run(dir: &Path) -> anyhow::Result<Self> {
        let log_path =
            saved_log(dir).with_context(|| format!("{} has no maestro.log", dir.display()))?;
        let debug = log_path.parent().unwrap_or(dir).to_path_buf();
        // Each observation with the Unix milliseconds its own file records, when it records any.
        let mut timed: Vec<(Option<u64>, Observation)> = Vec::new();
        let mut asm = log::Assembler::default();
        let mut records: Vec<log::Record> = fs::read_to_string(&log_path)?
            .lines()
            .filter_map(|line| asm.push(line))
            .collect();
        records.extend(asm.finish());
        let mut at = None;
        for record in records {
            // A record without the machine pattern takes the time of the record before it.
            at = record.unix_ms.or(at);
            timed.push((at, Observation::Log(record)));
        }

        let mut flows: Vec<PathBuf> = fs::read_dir(&debug)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        flows.sort_unstable();
        for flow in flows {
            let name = flow
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let commands = fs::read(flow.join("commands.json")).ok();
            // A flow's files are final once its last command has ended.
            let flow_at = commands
                .as_deref()
                .and_then(command_end_ms)
                .or_else(|| modified_ms(&flow.join("commands.json")));
            if let Some(bytes) = commands {
                timed.push((
                    flow_at,
                    Observation::File {
                        rel: format!("{name}/commands.json"),
                        contents: Some(bytes),
                    },
                ));
                if flow.join("manifest.json").exists() {
                    timed.push((
                        flow_at,
                        Observation::File {
                            rel: format!("{name}/manifest.json"),
                            contents: None,
                        },
                    ));
                }
            }
            if let Ok(bytes) = fs::read(flow.join("logs").join("crash-report.txt")) {
                timed.push((
                    flow_at,
                    Observation::File {
                        rel: format!("{name}/logs/crash-report.txt"),
                        contents: Some(bytes),
                    },
                ));
            }
        }
        let report = [dir.join("report.xml"), debug.join("report.xml")]
            .into_iter()
            .find_map(|p| fs::read(p).ok());

        let known = timed.iter().filter_map(|(t, _)| *t);
        let start_ms = known
            .clone()
            .min()
            .or_else(|| modified_ms(&log_path))
            .unwrap_or(0);
        let end_ms = known.max().unwrap_or(start_ms);
        // Relative to the start, a record without a time keeps the one before it, so the clock
        // never runs backwards in the list. The sort is stable: observations at one moment keep
        // their order (the log's records, then the flows' files in name order).
        let mut last = start_ms;
        let mut timeline: Vec<(u64, Observation)> = timed
            .into_iter()
            .map(|(t, obs)| {
                let abs = t.unwrap_or(last);
                last = abs;
                (abs.saturating_sub(start_ms), obs)
            })
            .collect();
        timeline.sort_by_key(|(ms, _)| *ms);
        timeline.push((end_ms.saturating_sub(start_ms), Observation::Report(report)));
        Ok(Self {
            argv: vec!["maestro".into(), "test".into()],
            redactor: Redactor::new([]),
            start: unix_ms_time(start_ms),
            timeline,
            debug: Some(debug),
        })
    }
}

/// The time `ms` Unix milliseconds after the epoch.
fn unix_ms_time(ms: u64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// When a file was last modified, in Unix milliseconds, if the file system says.
fn modified_ms(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    u64::try_from(modified.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

/// When a flow's last command ended, in Unix milliseconds: the latest `timestamp + duration` in its
/// `commands.json`. `None` when the file has no command with a timestamp.
fn command_end_ms(bytes: &[u8]) -> Option<u64> {
    let records = match fleet_maestro::bundle::parse_commands(bytes) {
        Commands::Final(r) | Commands::Aborted(r) | Commands::Unverified(r) => r,
        Commands::Empty | Commands::Invalid => return None,
    };
    records
        .iter()
        .filter_map(|c| Some(c.timestamp_ms? + c.duration_ms.unwrap_or(0)))
        .max()
}

/// A saved debug output directory's `maestro.log`, at the top or under `debug/`.
fn saved_log(dir: &Path) -> Option<PathBuf> {
    [dir.join("maestro.log"), dir.join("debug/maestro.log")]
        .into_iter()
        .find(|p| p.exists())
}

/// The observations recorded in a capture's `events.jsonl`, and the last timestamp.
/// The Maestro command inside a captured command line. Captures under `script` or a shell wrapper
/// carry more than that: the words from Maestro's program on are the command. A wrapper that runs
/// Maestro from a script (`… exec maestro …`) keeps the script's words after `exec`.
pub(crate) fn maestro_argv(command: &[String]) -> Vec<String> {
    if let Some(pos) = command
        .iter()
        .position(|a| Path::new(a).file_name().is_some_and(|n| n == "maestro"))
    {
        return command[pos..].to_vec();
    }
    command
        .iter()
        .find_map(|a| {
            a.split_once("exec ")
                .filter(|(_, rest)| rest.starts_with("maestro "))
                .map(|(_, rest)| rest.split_whitespace().map(str::to_owned).collect())
        })
        .unwrap_or_else(|| command.to_vec())
}

/// The directory, relative to the capture, that holds the captured run's flow bundles, as the live
/// watcher finds them: the layout model's bundle root (`output_layout`, the rule `watch.rs` follows)
/// with the session log's stamp, or the root itself when it is flat. The capture's paths are made
/// absolute, and the workspace config's `testOutputDir` is read from the capture's `run` directory,
/// the directory the captured wrappers ran Maestro in. `None` when the model cannot say (a flattened
/// run with no `--debug-output`, whose home the capture does not record).
fn bundle_dir_rel(dir: &Path, argv: &[String], session_rel: Option<&str>) -> Option<String> {
    let stamp = session_rel
        .and_then(|r| Path::new(r).parent())?
        .file_name()?;
    let run = dir.join("run");
    let absolute: Vec<String> = argv
        .iter()
        .map(|a| a.replace("<CAPTURE>", &dir.display().to_string()))
        .collect();
    let workspace = crate::layout::workspace_test_dir(&absolute, Some(&run))
        .map(|w| run.join(w).display().to_string());
    let layout = fleet_maestro::invocation::output_layout(
        &fleet_maestro::invocation::parse(&absolute),
        Some(&dir.join("state/maestro/tests")),
        None,
        workspace.as_deref(),
    )?;
    let root = match layout.bundles {
        fleet_maestro::invocation::OutputRoot::Flat(root) => root,
        fleet_maestro::invocation::OutputRoot::Stamped(base) => base.join(stamp),
    };
    let rel = root.strip_prefix(dir).ok()?;
    Some(format!("{}/", rel.display()))
}

fn recorded_timeline(
    dir: &Path,
    session_rel: Option<&str>,
    debug_rel: Option<&str>,
) -> anyhow::Result<(Vec<(u64, Observation)>, u64)> {
    let mut timeline = Vec::new();
    let mut asm = log::Assembler::default();
    // Files whose bytes the session reads: each is read once, at its first appearance.
    let mut read_once = std::collections::HashSet::new();
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
                let contents =
                    if rel.ends_with("commands.json") || rel.ends_with("logs/crash-report.txt") {
                        // Read once, at its first appearance; the capture kept the final bytes.
                        if !read_once.insert(rel.to_owned()) {
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
