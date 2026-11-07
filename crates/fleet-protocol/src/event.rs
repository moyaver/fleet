// SPDX-License-Identifier: AGPL-3.0-only
//! Typed journal event bodies (research 05 §5.2, refined by research 08).
//!
//! Classes: context (declares entities), claims (evidence about outcomes), artifacts, run
//! lifecycle and diagnostics. Outcomes are claims: they never overwrite each other, the claim
//! lattice in `fleet-core` decides (invariant I3).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A flow outcome as claimed by one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FlowOutcome {
    /// The flow passed.
    Passed,
    /// The flow failed.
    Failed,
    /// The flow was deliberately not run (for example an aborted `executionOrder` sequence).
    Skipped,
    /// The flow was cut short by a signal, a crash or a lost device.
    Interrupted,
    /// Nothing reliable is known.
    Unknown,
    /// A value this build doesn't know: treated as unknown, never as passed.
    #[serde(other)]
    Unrecognized,
}

/// A step's state as Maestro reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Started and not finished.
    Running,
    /// Finished successfully.
    Completed,
    /// Failed.
    Failed,
    /// Failed an optional check: a warning.
    Warned,
    /// Not run (a condition was false).
    Skipped,
    /// A value this build doesn't know.
    #[serde(other)]
    Unknown,
}

/// A flow as Maestro names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FlowRef {
    /// The flow's name (`name:` in the YAML, or the file name).
    pub name: String,
    /// Path of the flow file relative to the repository, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// How a suite is sharded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ShardMode {
    /// One shard.
    None,
    /// `--shard-split N`: flows spread across shards.
    Split,
    /// `--shard-all N`: every flow on every shard.
    All,
}

/// How a Maestro exit status reads (research 08 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ExitClass {
    /// Exit 0.
    Passed,
    /// Exit 1 with a report, or with a flow seen to fail (stdout, bundle or log): test failures.
    TestFailures,
    /// Exit 1 with no failed flow seen and an expected report missing: Maestro or infrastructure
    /// error. With no report requested, exit 1 is `Other` instead.
    MaestroError,
    /// Exit 2: bad usage.
    UsageError,
    /// Killed by a signal, or 130/143.
    Interrupted,
    /// Anything else.
    Other,
}

/// Whether and how the final report could be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    /// Parsed completely.
    Complete,
    /// Truncated; complete test cases were salvaged.
    Partial,
    /// Present but unreadable.
    Invalid,
    /// Not written (normal after a signal or crash).
    Absent,
}

/// How a report test case was matched to a live flow execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MatchMethod {
    /// Same position and same flow, the normal case.
    Positional,
    /// Matched by alignment on path and name.
    Aligned,
    /// Matched by name only.
    Name,
    /// No live flow: created from the report.
    Unmatched,
}

/// The Maestro build the run's session log named: the payload of `maestro.detected`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MaestroBuild {
    /// The version as Maestro logged it, for example `2.12.1`.
    pub version: String,
    /// Whether the agent's parsers were built for this version. `false` marks the run degraded.
    pub profile_supported: bool,
}

/// `run.started`: the agent began supervising a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RunStarted {
    /// The command line, redacted.
    pub argv: Vec<String>,
    /// Agent version.
    pub agent_version: String,
    /// Which evidence sources the prepared command will produce. Absent from older agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<EvidenceSources>,
}

/// Why the run has, or has not, a JUnit report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReportReason {
    /// The agent added `--format junit` (and `--output` unless the user gave one).
    Injected,
    /// The user asked for `--format junit`.
    UserJunit,
    /// The user chose another `--format`: no JUnit report by choice.
    UserFormat,
    /// Not added: a single flow, where `--format` would change Maestro's execution mode.
    SkippedSingleFlow,
    /// Not added: `-c`/`--continuous`, which the report path rejects.
    SkippedContinuous,
    /// Not added: the flow paths couldn't be classified.
    SkippedUnclassified,
    /// Not added: the agent doesn't rewrite commands (`--fleet-rewrite` off).
    RewriteDisabled,
    /// A value this build doesn't know: the report is treated as not expected.
    #[serde(other)]
    Unrecognized,
}

/// Where the JUnit report will be, if anywhere, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReportSource {
    /// The report file, redacted. `None`: no report is expected.
    pub path: Option<String>,
    /// Why.
    pub reason: ReportReason,
}

/// The evidence sources a run will have, so "no report" can be told from "report missing".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EvidenceSources {
    /// The JUnit report.
    pub report: ReportSource,
    /// The debug output directory (session log and flow bundles), when it is known.
    pub debug_output: Option<String>,
    /// Whether Maestro's log is made machine-readable by the agent.
    pub maestro_log: bool,
}

/// `suite.started`: one Maestro invocation began.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SuiteStarted {
    /// Sharding mode, from the command line.
    pub shard_mode: ShardMode,
    /// Number of shards requested.
    pub shards: u32,
}

/// `shard.assigned`: Maestro picked a device for a shard and planned its flows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ShardAssigned {
    /// 1-based shard number as Maestro prints it.
    pub shard_number: u32,
    /// The device serial (`emulator-5554`, `127.0.0.1:5555`, a USB serial).
    pub device_serial: String,
    /// The flows planned for this shard, in order.
    pub planned: Vec<FlowRef>,
}

/// `flow.started`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FlowStarted {
    /// Which flow.
    pub flow: FlowRef,
    /// 1-based shard number.
    pub shard_number: u32,
    /// 0-based execution order within the shard.
    pub ordinal: u32,
}

/// `step.started`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StepStarted {
    /// Maestro's description of the step, redacted.
    pub description: String,
}

/// `step.ended`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct StepEnded {
    /// The step's final status.
    pub status: StepStatus,
}

/// Why a flow failed, as Maestro said it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FailureInfo {
    /// The message, redacted and truncated.
    pub message: String,
}

/// `flow.ended`: a claim about a flow's outcome from a live source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FlowEnded {
    /// The claimed outcome.
    pub outcome: FlowOutcome,
    /// Maestro's own duration, when it printed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureInfo>,
    /// Which flow, set when no `flow.started` named it first (a planned flow Maestro skipped), so
    /// the server can create the flow execution from the claim alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<FlowRef>,
}

/// One `<testcase>` of the final report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JunitTestcase {
    /// `id` attribute.
    pub id: Option<String>,
    /// `name` attribute.
    pub name: String,
    /// `file` attribute.
    pub file: Option<String>,
    /// `time`, in milliseconds.
    pub time_ms: Option<u64>,
    /// `timestamp`: a local date-time without a zone, as written.
    pub timestamp_local: Option<String>,
    /// Maestro's non-standard `status` attribute.
    pub status: Option<String>,
    /// `<failure>` text, redacted.
    pub failure: Option<String>,
}

/// `flow.reported`: the final report's claim about one flow execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FlowReported {
    /// 0-based `<testsuite>` index (= shard order).
    pub testsuite_index: u32,
    /// 0-based `<testcase>` index within that suite.
    pub testcase_index: u32,
    /// How it was matched to a live flow.
    pub matched: MatchMethod,
    /// The test case as written.
    pub junit: JunitTestcase,
    /// Repository-relative identity, when the agent can establish one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    /// Outcome mapped from Maestro's status.
    pub outcome: FlowOutcome,
}

/// `report.parsed`: what the final report said overall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ReportParsed {
    /// Whether it could be read.
    pub status: ReportStatus,
    /// Number of `<testsuite>` elements.
    pub suites: u32,
    /// Total test cases.
    pub tests: u32,
    /// Test cases with status `ERROR`.
    pub failures: u32,
}

/// `process.exited`: how Maestro ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProcessExited {
    /// Exit code, if it exited normally.
    pub code: Option<i32>,
    /// Signal, if one killed it.
    pub signal: Option<i32>,
    /// What that means.
    pub class: ExitClass,
}

/// `run.signal`: the agent received a signal and forwarded it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RunSignal {
    /// Signal number.
    pub signal: i32,
}

/// `run.ended`: the agent is done with this run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RunEnded {
    /// Whether a signal cut the run short.
    pub interrupted: bool,
    /// Output bytes the observer didn't see because it fell behind (invariant I5).
    pub output_bytes_dropped: u64,
}

/// `artifact.registered`: a file worth keeping appeared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArtifactRegistered {
    /// What kind of file.
    pub class: String,
    /// Path relative to the run's output directory.
    pub path: String,
}

/// `artifact.uploaded`: the server verified and stored a file (the agent journals the server's
/// answer to `upload_complete`, or to an `exists` grant).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArtifactUploaded {
    /// The artifact class (see [`crate::artifact::ArtifactClass`]).
    pub class: String,
    /// Path relative to the run's output directory, as in `artifact.registered`.
    pub path: String,
    /// The server's artifact id.
    pub artifact_id: Uuid,
    /// Size in bytes.
    pub size: u64,
}

/// `artifact.rejected`: a file was not uploaded, and why. The agent journals it when the server
/// refuses the file, and when the agent itself decides not to send it (for example a log it
/// cannot redact reliably: invariant I6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArtifactRejected {
    /// The artifact class.
    pub class: String,
    /// Path relative to the run's output directory.
    pub path: String,
    /// A stable code: a [`crate::artifact::RejectReason`] code from the server, or an agent code
    /// such as `not_redactable`.
    pub reason: String,
}

/// `app.crashed`: the app under test died during a flow, as Maestro's `crash-report.txt` records it
/// (issue #75). It is evidence on the flow. It does not change the flow's verdict, which stays
/// Maestro's own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppCrashed {
    /// The app's package, for example `com.example.app`.
    pub package: String,
    /// The exception's first line: its class, then its message.
    pub exception: String,
}

/// `plan.assigned`: this run executes one shard of a Fleet plan (ADR-0013).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PlanAssigned {
    /// The plan.
    pub plan_id: String,
    /// The shard of the plan this run executes.
    pub shard_id: u32,
    /// Flow files assigned to it, in order.
    pub flows: Vec<String>,
}

/// adb's view of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AdbState {
    /// Online and usable.
    Device,
    /// Connected but not responding.
    Offline,
    /// Needs authorization on the device.
    Unauthorized,
    /// Gone from adb's list.
    Absent,
    /// Another state adb reported.
    #[serde(other)]
    Other,
}

/// `device.state_observed`: adb reported a device's state (research 02 §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceStateObserved {
    /// The serial.
    pub serial: String,
    /// Its state.
    pub state: AdbState,
}

/// `agent.warning`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentWarning {
    /// A stable code, for example `report_absent`.
    pub code: String,
    /// Human-readable detail, redacted.
    pub message: String,
}

/// `journal.gap`: events the agent had to drop, so the loss is explicit (invariant I5).
///
/// The agent sequences the marker in the hole it reports: its own `seq` is `to_seq`, so
/// `from_seq..=to_seq` covers the dropped events and the marker's own slot, and the journal
/// stays contiguous for the server (ADR-0005, amendment 2026-11-04).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JournalGap {
    /// First missing sequence number.
    pub from_seq: u64,
    /// Last missing sequence number; the marker's own `seq` when it covers a hole.
    pub to_seq: u64,
    /// Why.
    pub reason: String,
}

/// `parser.unrecognized`: lines a parser couldn't read (Maestro output drift).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ParserUnrecognized {
    /// Which parser.
    pub parser: String,
    /// How many lines.
    pub count: u32,
}

/// Every event type this build knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", content = "data")]
pub enum EventBody {
    /// See [`RunStarted`].
    #[serde(rename = "run.started")]
    RunStarted(RunStarted),
    /// See [`SuiteStarted`].
    #[serde(rename = "suite.started")]
    SuiteStarted(SuiteStarted),
    /// See [`ShardAssigned`].
    #[serde(rename = "shard.assigned")]
    ShardAssigned(ShardAssigned),
    /// See [`FlowStarted`].
    #[serde(rename = "flow.started")]
    FlowStarted(FlowStarted),
    /// See [`StepStarted`].
    #[serde(rename = "step.started")]
    StepStarted(StepStarted),
    /// See [`StepEnded`].
    #[serde(rename = "step.ended")]
    StepEnded(StepEnded),
    /// See [`FlowEnded`].
    #[serde(rename = "flow.ended")]
    FlowEnded(FlowEnded),
    /// See [`FlowReported`].
    #[serde(rename = "flow.reported")]
    FlowReported(FlowReported),
    /// See [`ReportParsed`].
    #[serde(rename = "report.parsed")]
    ReportParsed(ReportParsed),
    /// See [`ProcessExited`].
    #[serde(rename = "process.exited")]
    ProcessExited(ProcessExited),
    /// See [`ArtifactRegistered`].
    #[serde(rename = "artifact.registered")]
    ArtifactRegistered(ArtifactRegistered),
    /// See [`ArtifactUploaded`].
    #[serde(rename = "artifact.uploaded")]
    ArtifactUploaded(ArtifactUploaded),
    /// See [`ArtifactRejected`].
    #[serde(rename = "artifact.rejected")]
    ArtifactRejected(ArtifactRejected),
    /// See [`RunSignal`].
    #[serde(rename = "run.signal")]
    RunSignal(RunSignal),
    /// `run.finalizing`: Maestro exited; the agent is reconciling and flushing.
    #[serde(rename = "run.finalizing")]
    RunFinalizing,
    /// See [`RunEnded`].
    #[serde(rename = "run.ended")]
    RunEnded(RunEnded),
    /// See [`PlanAssigned`].
    #[serde(rename = "plan.assigned")]
    PlanAssigned(PlanAssigned),
    /// See [`DeviceStateObserved`].
    #[serde(rename = "device.state_observed")]
    DeviceStateObserved(DeviceStateObserved),
    /// See [`AgentWarning`].
    #[serde(rename = "agent.warning")]
    AgentWarning(AgentWarning),
    /// See [`JournalGap`].
    #[serde(rename = "journal.gap")]
    JournalGap(JournalGap),
    /// See [`AppCrashed`].
    #[serde(rename = "app.crashed")]
    AppCrashed(AppCrashed),
    /// See [`ParserUnrecognized`].
    #[serde(rename = "parser.unrecognized")]
    ParserUnrecognized(ParserUnrecognized),
    /// See [`MaestroBuild`].
    #[serde(rename = "maestro.detected")]
    MaestroDetected(MaestroBuild),
}

/// Payload versions of the known types. Bump one when its payload changes incompatibly.
const VERSIONS: &[(&str, u16)] = &[
    ("run.started", 1),
    ("suite.started", 1),
    ("shard.assigned", 1),
    ("flow.started", 1),
    ("step.started", 1),
    ("step.ended", 1),
    ("flow.ended", 1),
    ("flow.reported", 1),
    ("report.parsed", 1),
    ("process.exited", 1),
    ("artifact.registered", 1),
    ("artifact.uploaded", 1),
    ("artifact.rejected", 1),
    ("run.signal", 1),
    ("run.finalizing", 1),
    ("run.ended", 1),
    ("plan.assigned", 1),
    ("device.state_observed", 1),
    ("agent.warning", 1),
    ("app.crashed", 1),
    ("journal.gap", 1),
    ("parser.unrecognized", 1),
    ("maestro.detected", 1),
];

impl EventBody {
    /// The wire type name, for example `flow.ended`.
    #[must_use]
    pub fn kind(&self) -> String {
        self.to_parts().0
    }

    /// The payload version this build writes.
    #[must_use]
    pub fn version(&self) -> u16 {
        let kind = self.kind();
        VERSIONS
            .iter()
            .find(|(k, _)| *k == kind)
            .map_or(1, |(_, v)| *v)
    }

    /// Splits into the wire type name and payload.
    pub(crate) fn to_parts(&self) -> (String, serde_json::Value) {
        // A tagged enum of plain data always serializes; an empty object stands in if it can't.
        let value = serde_json::to_value(self).unwrap_or_default();
        let kind = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let data = value
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        (kind, data)
    }

    /// The typed body for a known type and version, `None` otherwise.
    pub(crate) fn from_parts(
        kind: &str,
        v: u16,
        data: &serde_json::Value,
    ) -> Result<Option<Self>, serde_json::Error> {
        if !VERSIONS.iter().any(|(k, known)| *k == kind && *known == v) {
            return Ok(None);
        }
        let tagged = if data.is_null() {
            serde_json::json!({ "type": kind })
        } else {
            serde_json::json!({ "type": kind, "data": data })
        };
        serde_json::from_value(tagged).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_started_without_evidence_still_parses_and_omits_it() {
        let old: RunStarted =
            serde_json::from_str(r#"{"argv":["maestro"],"agent_version":"0.0.0"}"#).unwrap();
        assert_eq!(old.evidence, None);
        assert!(!serde_json::to_string(&old).unwrap().contains("evidence"));
    }

    fn samples() -> Vec<EventBody> {
        let flow = FlowRef {
            name: "login".into(),
            path: Some("flows/login.yaml".into()),
        };
        vec![
            EventBody::RunStarted(RunStarted {
                argv: vec!["maestro".into()],
                agent_version: "0.0.0".into(),
                evidence: Some(EvidenceSources {
                    report: ReportSource {
                        path: None,
                        reason: ReportReason::SkippedSingleFlow,
                    },
                    debug_output: Some("/run/artifacts".into()),
                    maestro_log: true,
                }),
            }),
            EventBody::SuiteStarted(SuiteStarted {
                shard_mode: ShardMode::Split,
                shards: 2,
            }),
            EventBody::ShardAssigned(ShardAssigned {
                shard_number: 1,
                device_serial: "emulator-5554".into(),
                planned: vec![flow.clone()],
            }),
            EventBody::FlowStarted(FlowStarted {
                flow,
                shard_number: 1,
                ordinal: 0,
            }),
            EventBody::StepStarted(StepStarted {
                description: "Tap on \"Login\"".into(),
            }),
            EventBody::StepEnded(StepEnded {
                status: StepStatus::Warned,
            }),
            EventBody::FlowEnded(FlowEnded {
                outcome: FlowOutcome::Failed,
                duration_ms: Some(19_000),
                failure: Some(FailureInfo {
                    message: "Assertion is false".into(),
                }),
                flow: None,
            }),
            EventBody::ReportParsed(ReportParsed {
                status: ReportStatus::Absent,
                suites: 0,
                tests: 0,
                failures: 0,
            }),
            EventBody::ProcessExited(ProcessExited {
                code: None,
                signal: Some(15),
                class: ExitClass::Interrupted,
            }),
            EventBody::ArtifactUploaded(ArtifactUploaded {
                class: "junit".into(),
                path: "report.xml".into(),
                artifact_id: Uuid::nil(),
                size: 1024,
            }),
            EventBody::ArtifactRejected(ArtifactRejected {
                class: "maestro_log".into(),
                path: "maestro.log".into(),
                reason: "not_redactable".into(),
            }),
            EventBody::RunSignal(RunSignal { signal: 2 }),
            EventBody::RunFinalizing,
            EventBody::RunEnded(RunEnded {
                interrupted: true,
                output_bytes_dropped: 0,
            }),
            EventBody::JournalGap(JournalGap {
                from_seq: 4,
                to_seq: 9,
                reason: "spool_full".into(),
            }),
            EventBody::MaestroDetected(MaestroBuild {
                version: "2.12.1".into(),
                profile_supported: true,
            }),
        ]
    }

    #[test]
    fn every_known_type_round_trips_through_parts() {
        for body in samples() {
            let (kind, data) = body.to_parts();
            assert!(
                VERSIONS.iter().any(|(k, _)| *k == kind),
                "{kind} missing from VERSIONS"
            );
            assert_eq!(
                EventBody::from_parts(&kind, body.version(), &data).unwrap(),
                Some(body)
            );
        }
    }

    #[test]
    fn versions_list_has_no_duplicates() {
        let mut kinds: Vec<_> = VERSIONS.iter().map(|(k, _)| *k).collect();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), VERSIONS.len());
    }
}
