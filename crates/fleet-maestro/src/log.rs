// SPDX-License-Identifier: AGPL-3.0-only
//! The session `maestro.log` (evidence lane A, the primary live source; research 08 Q2–Q3, Q8).
//!
//! Two line formats: the machine pattern the agent sets
//! (`MFLT1|<unix ms>|<thread>|<LEVEL>|<logger>|<message>`) and Maestro's default
//! (`HH:mm:ss.SSS [LEVEL] <logger>.<method>: <message>`). Any other line (a stack trace, which
//! log4j writes outside the pattern) continues the previous record. Attribution uses the
//! `[shard N]` message prefix, never the thread.

/// The agent's machine log pattern, set through `MAESTRO_CLI_LOG_PATTERN_FILE`.
pub const MACHINE_PATTERN: &str = "MFLT1|%d{UNIX_MILLIS}|%t|%level|%logger|%enc{%msg}{CRLF}%n";

/// One log record: a header line plus any continuation lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Unix milliseconds, from the machine pattern only.
    pub unix_ms: Option<u64>,
    /// Level, upper case and trimmed (`INFO`).
    pub level: String,
    /// Logger name without the method (`maestro.cli.runner.CliConsoleListener`).
    pub logger: String,
    /// The message.
    pub message: String,
    /// Lines that followed outside the pattern, for example a stack trace.
    pub continuation: Vec<String>,
}

/// Turns lines into records. Feed lines as they arrive; a record is complete when the next
/// header line starts (or on [`Assembler::finish`]).
#[derive(Debug, Default)]
pub struct Assembler {
    pending: Option<Record>,
    /// Lines before the first header, which belong to no record.
    pub orphan_lines: u32,
}

impl Assembler {
    /// Adds a line (without its line ending) and returns the record it completed, if any.
    pub fn push(&mut self, line: &str) -> Option<Record> {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(header) = parse_header(line) {
            return self.pending.replace(header);
        }
        match self.pending.as_mut() {
            Some(r) => r.continuation.push(line.to_owned()),
            None => self.orphan_lines += 1,
        }
        None
    }

    /// The record still being assembled, if any.
    pub fn finish(&mut self) -> Option<Record> {
        self.pending.take()
    }
}

fn parse_header(line: &str) -> Option<Record> {
    if let Some(rest) = line.strip_prefix("MFLT1|") {
        let mut f = rest.splitn(5, '|');
        let unix_ms = f.next()?.parse().ok();
        let _thread = f.next()?;
        let level = f.next()?.trim().to_owned();
        let logger = f.next()?.to_owned();
        let message = unescape_crlf(f.next()?);
        return Some(Record {
            unix_ms,
            level,
            logger,
            message,
            continuation: Vec::new(),
        });
    }
    // HH:mm:ss.SSS [LEVEL] logger.method: message
    let (time, rest) = line.split_once(' ')?;
    if time.len() != 12
        || !time
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b':' || b == b'.')
    {
        return None;
    }
    let rest = rest.strip_prefix('[')?;
    let (level, rest) = rest.split_once("] ")?;
    let (logger_method, message) = rest
        .split_once(": ")
        .unwrap_or((rest.trim_end_matches(':'), ""));
    let logger = logger_method
        .rsplit_once('.')
        .map_or(logger_method, |(l, _)| l)
        .to_owned();
    Some(Record {
        unix_ms: None,
        level: level.trim().to_owned(),
        logger,
        message: message.to_owned(),
        continuation: Vec::new(),
    })
}

/// Undoes log4j's `%enc{…}{CRLF}`, which writes CR and LF as `\r` and `\n`.
fn unescape_crlf(s: &str) -> String {
    if s.contains('\\') {
        s.replace("\\r", "\r").replace("\\n", "\n")
    } else {
        s.to_owned()
    }
}

/// A step's status in a `CliConsoleListener` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    /// Queued (Maestro lists upcoming steps); not evidence of anything.
    Pending,
    /// Started.
    Running,
    /// Finished.
    Completed,
    /// Failed.
    Failed,
    /// An optional step failed.
    Warned,
    /// Not run.
    Skipped,
}

/// What a record means for the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `[shard N] Selected device <serial> using port <P> with execution plan ExecutionPlan(flowsToRun=[…], …)`.
    ShardAssigned {
        /// 1-based shard number.
        shard: u32,
        /// Device serial.
        serial: String,
        /// Planned flow file paths, as Maestro wrote them.
        flows: Vec<String>,
    },
    /// `Running flow <name>`.
    FlowStarted {
        /// Shard, when the run is sharded.
        shard: Option<u32>,
        /// Flow name.
        name: String,
    },
    /// `<description> RUNNING|COMPLETED|FAILED|WARNED|SKIPPED|PENDING`.
    Step {
        /// Shard, when the run is sharded.
        shard: Option<u32>,
        /// Maestro's description of the step.
        description: String,
        /// Its state.
        state: StepState,
    },
    /// `MAESTRO.logSystemInfo: Maestro Version: 2.12.1`, written once near the start of a session log.
    MaestroVersion {
        /// The version, as Maestro printed it.
        version: String,
    },
    /// `Attempt N failed for retry command`.
    RetryAttemptFailed {
        /// Attempt number.
        attempt: u32,
    },
    /// `Failed to complete flow`, with the exception that ended it.
    FlowCrashed {
        /// The exception's first line, for example `maestro.android.DeviceServerDiedException: …`.
        exception: Option<String>,
    },
}

const CONSOLE_LISTENER: &str = "maestro.cli.runner.CliConsoleListener";
const SUITE_INTERACTOR: &str = "maestro.cli.runner.TestSuiteInteractor";
/// Logger of Maestro's system info (`MAESTRO.logSystemInfo`, the method is dropped).
const SYSTEM_INFO: &str = "MAESTRO";

/// Reads the meaning of a record, if it has one Fleet uses.
#[must_use]
pub fn classify(record: &Record) -> Option<Event> {
    let (shard, msg) = split_shard(&record.message);
    if record.logger.ends_with("TestCommand") {
        return shard_assigned(shard?, msg);
    }
    if record.logger == SUITE_INTERACTOR {
        if let Some(name) = msg.trim_start().strip_prefix("Running flow ") {
            return Some(Event::FlowStarted {
                shard,
                name: name.trim().to_owned(),
            });
        }
        if msg.starts_with("Failed to complete flow") {
            let exception = record.continuation.first().map(|l| l.trim().to_owned());
            return Some(Event::FlowCrashed { exception });
        }
        return None;
    }
    if record.logger == CONSOLE_LISTENER {
        let (description, state) = msg.rsplit_once(' ')?;
        let state = match state {
            "PENDING" => StepState::Pending,
            "RUNNING" => StepState::Running,
            "COMPLETED" => StepState::Completed,
            "FAILED" => StepState::Failed,
            "WARNED" => StepState::Warned,
            "SKIPPED" => StepState::Skipped,
            _ => return None,
        };
        return Some(Event::Step {
            shard,
            description: description.to_owned(),
            state,
        });
    }
    if record.logger == SYSTEM_INFO {
        let version = msg
            .strip_prefix("Maestro Version: ")?
            .split_whitespace()
            .next()?;
        return Some(Event::MaestroVersion {
            version: version.to_owned(),
        });
    }
    if record.logger.ends_with("Orchestra") {
        let attempt = msg
            .strip_prefix("Attempt ")?
            .strip_suffix(" failed for retry command")?
            .parse()
            .ok()?;
        return Some(Event::RetryAttemptFailed { attempt });
    }
    None
}

/// Splits a leading `[shard N] ` off a message.
#[must_use]
pub fn split_shard(msg: &str) -> (Option<u32>, &str) {
    msg.strip_prefix("[shard ")
        .and_then(|r| r.split_once("] "))
        .and_then(|(n, rest)| Some((Some(n.parse().ok()?), rest)))
        .unwrap_or((None, msg))
}

/// Whether a console step is a `retry` block: Maestro describes it as `Retry 2 times`.
#[must_use]
pub fn is_retry_block(description: &str) -> bool {
    description
        .strip_prefix("Retry ")
        .and_then(|rest| rest.strip_suffix(" times"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn shard_assigned(shard: u32, msg: &str) -> Option<Event> {
    let rest = msg.strip_prefix("Selected device ")?;
    let (serial, rest) = rest.split_once(' ')?;
    // `executionOrder` flows run first, in order, then the rest (research 05 §8.1).
    let sequence = rest
        .split_once("sequence=FlowSequence(flows=[")
        .map(|(_, r)| bracket_list(r))
        .unwrap_or_default();
    let to_run = bracket_list(rest.split_once("flowsToRun=[")?.1);
    let rest_flows: Vec<String> = to_run
        .into_iter()
        .filter(|f| !sequence.contains(f))
        .collect();
    let mut flows = sequence;
    flows.extend(rest_flows);
    Some(Event::ShardAssigned {
        shard,
        serial: serial.to_owned(),
        flows,
    })
}

/// The comma-separated items up to the closing `]`.
fn bracket_list(s: &str) -> Vec<String> {
    let list = s.split_once(']').map_or(s, |(l, _)| l);
    list.split(", ")
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(text: &str) -> Vec<Record> {
        let mut a = Assembler::default();
        let mut out: Vec<Record> = text.lines().filter_map(|l| a.push(l)).collect();
        out.extend(a.finish());
        out
    }

    #[test]
    fn machine_lines_and_continuations() {
        let r = records(
            "MFLT1|1793752573057|DefaultDispatcher-worker-3|ERROR|maestro.cli.runner.TestSuiteInteractor|Failed to complete flow\n\
             maestro.android.DeviceServerDiedException: Device server died\n\
             \tat maestro.android.AndroidDeviceConnection.onGrpcDeath(AndroidDeviceConnection.kt:319)\n\
             MFLT1|1|main|INFO|x.Y|a\\nb",
        );
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].unix_ms, Some(1_793_752_573_057));
        assert_eq!(r[0].continuation.len(), 2);
        assert_eq!(
            classify(&r[0]),
            Some(Event::FlowCrashed {
                exception: Some(
                    "maestro.android.DeviceServerDiedException: Device server died".into()
                )
            })
        );
        assert_eq!(r[1].message, "a\nb", "CRLF encoding undone");
    }

    #[test]
    fn default_pattern_lines() {
        let r = records(
            "02:24:10.638 [ INFO] maestro.cli.runner.TestSuiteInteractor.runFlow:  Running flow pass",
        );
        assert_eq!(
            (r[0].level.as_str(), r[0].logger.as_str()),
            ("INFO", "maestro.cli.runner.TestSuiteInteractor")
        );
        assert_eq!(
            classify(&r[0]),
            Some(Event::FlowStarted {
                shard: None,
                name: "pass".into()
            })
        );
    }

    #[test]
    fn shard_plan_and_steps() {
        let r = records(
            "MFLT1|1|w-2|INFO|maestro.cli.command.TestCommand|[shard 2] Selected device emulator-5602 using port 64775 with execution plan ExecutionPlan(flowsToRun=[<REPO>/s/charlie.yaml, <REPO>/s/alpha.yaml], sequence=FlowSequence(flows=[], continueOnFailure=true))\n\
             MFLT1|2|w-2|INFO|maestro.cli.runner.CliConsoleListener|[shard 2] Tap on \"Storage\" RUNNING\n\
             MFLT1|3|w-3|INFO|maestro.cli.runner.CliConsoleListener|Assert that (Optional) \"x\" is visible WARNED\n\
             MFLT1|4|w-3|INFO|maestro.orchestra.Orchestra|Attempt 1 failed for retry command\n\
             MFLT1|5|w-3|INFO|maestro.cli.runner.CliConsoleListener|Scrolling UP until \"x\" is visible with speed 40",
        );
        assert_eq!(
            classify(&r[0]),
            Some(Event::ShardAssigned {
                shard: 2,
                serial: "emulator-5602".into(),
                flows: vec!["<REPO>/s/charlie.yaml".into(), "<REPO>/s/alpha.yaml".into()]
            })
        );
        assert_eq!(
            classify(&r[1]),
            Some(Event::Step {
                shard: Some(2),
                description: "Tap on \"Storage\"".into(),
                state: StepState::Running
            })
        );
        assert!(matches!(
            classify(&r[2]),
            Some(Event::Step {
                state: StepState::Warned,
                shard: None,
                ..
            })
        ));
        assert_eq!(
            classify(&r[3]),
            Some(Event::RetryAttemptFailed { attempt: 1 })
        );
        assert_eq!(classify(&r[4]), None, "a progress message is not a step");
        let ordered = records(
            "MFLT1|1|w|INFO|maestro.cli.command.TestCommand|[shard 1] Selected device emulator-5600 using port 1 with execution plan ExecutionPlan(flowsToRun=[], sequence=FlowSequence(flows=[<R>/first.yaml, <R>/second.yaml], continueOnFailure=false), workspaceConfig=WorkspaceConfig(flows=null))",
        );
        assert_eq!(
            classify(&ordered[0]),
            Some(Event::ShardAssigned {
                shard: 1,
                serial: "emulator-5600".into(),
                flows: vec!["<R>/first.yaml".into(), "<R>/second.yaml".into()]
            }),
            "sequence flows are planned too"
        );
    }

    #[test]
    fn the_session_log_names_the_maestro_version_once_in_either_pattern() {
        for text in [
            include_str!("../../../fixtures/maestro/2.12.1/pass/run/debug/maestro.log"),
            include_str!("../../../fixtures/maestro/2.12.1/pass-machine-log/run/debug/maestro.log"),
        ] {
            let versions: Vec<_> = records(text)
                .iter()
                .filter_map(classify)
                .filter_map(|e| match e {
                    Event::MaestroVersion { version } => Some(version),
                    _ => None,
                })
                .collect();
            assert_eq!(versions, ["2.12.1"]);
        }
    }

    #[test]
    fn retry_blocks_are_named_by_their_count_only() {
        assert!(is_retry_block("Retry 2 times"));
        assert!(is_retry_block("Retry 1 times"));
        for not in [
            "Retry  times",
            "Retry 2 times ",
            "Retry x times",
            "Repeat 2 times",
            "Run subflows/retry.yaml",
        ] {
            assert!(!is_retry_block(not), "{not:?}");
        }
    }

    #[test]
    fn garbage_never_panics() {
        for line in [
            "",
            "MFLT1|",
            "MFLT1|x|y",
            "12:00:00.000 [",
            "[shard ] x",
            "[shard 9999999999] x",
            "é",
            "\u{0}",
            "MFLT1|1|main|INFO|MAESTRO|Maestro Version: ",
        ] {
            let r = records(line);
            for rec in &r {
                let _ = classify(rec);
            }
        }
        let mut a = Assembler::default();
        assert_eq!(a.push("no header yet"), None);
        assert_eq!(a.orphan_lines, 1);
    }
}
