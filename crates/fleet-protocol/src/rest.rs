// SPDX-License-Identifier: AGPL-3.0-only
//! REST read models of the dashboard's screens A–D (spec §8; PLAN §10.3; invariant I4).
//!
//! Screen A (overview) reads [`OverviewView`], B (pipeline detail) adds [`TimelineView`] to the
//! live [`Snapshot`](crate::live::Snapshot), C (worker and device) reads [`WorkerView`],
//! [`DeviceDetailView`] and [`MetricsView`], D (flow investigation) reads [`FlowDetailView`].
//! JSON is `snake_case`. Nothing here is invented: a value the agent didn't report is `None` or
//! `available: false` with a reason, never zero (acceptance criteria 4 and 5), and every outcome
//! carries the source it rests on.
//!
//! Screens E and F (history and compare) read the models in [`history`].

pub mod history;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::live::{AgentRunView, AttemptView, DeviceView, FlowView, JobView, Liveness, SuiteView};

/// Jobs of one attempt, by lifecycle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct JobCounts {
    /// Waiting to start.
    pub pending: i64,
    /// Starting up.
    pub starting: i64,
    /// Running.
    pub running: i64,
    /// Finalizing: the run ended and the server is closing it.
    pub finalizing: i64,
    /// Closed.
    pub closed: i64,
}

/// Flow executions of one attempt, by state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FlowCounts {
    /// Planned, not started.
    pub pending: i64,
    /// Started, no outcome yet.
    pub running: i64,
    /// Passed, by a parsed Maestro source.
    pub passed: i64,
    /// Failed.
    pub failed: i64,
    /// Deliberately not run.
    pub skipped: i64,
    /// Cut short.
    pub interrupted: i64,
    /// Nothing reliable is known.
    pub unknown: i64,
}

/// An attempt with an open job, and how far along it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ActiveAttemptView {
    /// The attempt (`planned_total` is `None` when no plan is known: progress is uncertain).
    pub attempt: AttemptView,
    /// Its jobs by lifecycle.
    pub jobs: JobCounts,
    /// Its flows by state.
    pub flows: FlowCounts,
}

/// An attempt whose jobs are all closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RecentAttemptView {
    /// The attempt.
    pub attempt: AttemptView,
    /// When its last job closed.
    #[serde(with = "time::serde::rfc3339")]
    pub finished_at: OffsetDateTime,
    /// Its jobs, each with the outcome and the source it rests on.
    pub jobs: Vec<JobView>,
    /// Its flows by state.
    pub flows: FlowCounts,
}

/// Workers by display liveness.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WorkerCounts {
    /// Frames are arriving.
    pub online: i64,
    /// Silent for longer than the stale threshold.
    pub stale: i64,
    /// Its agent's socket is gone and the run has not ended.
    pub disconnected: i64,
    /// Every run on it ended.
    pub completed: i64,
}

/// Screen A: what is running now, what just finished, and how the runners are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OverviewView {
    /// When the server built this.
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    /// Active attempts, newest first (at most 100).
    pub active: Vec<ActiveAttemptView>,
    /// How many attempts are active in all; more than `active.len()` when it was cut.
    pub active_total: i64,
    /// Recently finished attempts, newest first (bounded by the `recent` parameter).
    pub recent: Vec<RecentAttemptView>,
    /// Workers seen in the last day, by liveness.
    pub worker_counts: WorkerCounts,
    /// Those workers, most recently seen first (at most 50).
    pub workers: Vec<WorkerSummaryView>,
}

/// A worker (a runner machine) at a glance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WorkerSummaryView {
    /// ID.
    pub id: Uuid,
    /// Runner name.
    pub runner_name: Option<String>,
    /// Operating system the agent reported.
    pub os: Option<String>,
    /// CPU architecture the agent reported.
    pub arch: Option<String>,
    /// First contact.
    #[serde(with = "time::serde::rfc3339")]
    pub first_seen_at: OffsetDateTime,
    /// Last persisted contact.
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: OffsetDateTime,
    /// Display liveness, set by the server from live contact.
    pub liveness: Option<Liveness>,
    /// Agent runs on it that may still be alive.
    pub active_runs: i64,
    /// Its newest host sample if one was taken in the last hour: runner health and how fresh
    /// the data is. `None` means no recent data, not a healthy machine.
    pub latest: Option<LatestReadings>,
}

/// One metric's value at one moment, with the same labels and units as a series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MetricReading {
    /// Stable identifier, for example `host_cpu_pct`.
    pub metric: String,
    /// What to call it on screen.
    pub label: String,
    /// What it is measured in.
    pub unit: MetricUnit,
    /// What it is about.
    pub scope: MetricScope,
    /// The value; `None` exactly when `available` is false.
    pub value: Option<f64>,
    /// Whether the agent reported a value.
    pub available: bool,
    /// Why there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A worker's newest host sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LatestReadings {
    /// When the sample was taken.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// Every host metric: a value, or why there is none.
    pub readings: Vec<MetricReading>,
}

/// One device's lane on the swimlane timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LaneView {
    /// The device session: what bars refer to.
    pub id: Uuid,
    /// adb serial.
    pub serial: String,
    /// `discovered`, `booting`, `ready`, `busy`, `offline`, `released`, `lost`.
    pub state: String,
    /// The agent run the device belongs to.
    pub agent_run_id: Uuid,
    /// The job it ran for.
    pub job_id: Uuid,
    /// That job's key.
    pub job_key: String,
    /// The runner the agent ran on.
    pub runner_name: Option<String>,
    /// When the agent first saw the device.
    #[serde(with = "time::serde::rfc3339")]
    pub first_seen_at: OffsetDateTime,
    /// When the agent last saw it.
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: OffsetDateTime,
}

/// One started flow execution as a bar on a lane. Provenance travels with it (I4): the winning
/// claim's source and how settled the outcome is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BarView {
    /// The flow execution.
    pub flow_id: Uuid,
    /// The device session it ran on; `None` when no event named one.
    pub lane_id: Option<Uuid>,
    /// Flow name.
    pub name: String,
    /// 1-based shard.
    pub shard_number: Option<i32>,
    /// Order within the shard.
    pub ordinal: Option<i32>,
    /// `running`, `passed`, `failed`, `skipped`, `interrupted`, `unknown`.
    pub state: String,
    /// The winning claim's source; `None` while no claim exists.
    pub outcome_source: Option<String>,
    /// `provisional`, `verified`, `unverified_final`, `inferred`.
    pub finality: String,
    /// Two parsed sources disagreed.
    pub has_conflict: bool,
    /// When it started.
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    /// When its outcome arrived; `None` while it runs.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub ended_at: Option<OffsetDateTime>,
    /// Maestro's or the report's duration.
    pub duration_ms: Option<i64>,
}

/// Screen B's swimlane: device lanes and flow bars of one attempt, at `version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TimelineView {
    /// The attempt.
    pub attempt_id: Uuid,
    /// Its live version: apply live deltas with a higher one.
    pub version: i64,
    /// When the server built this (the right edge for running bars).
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    /// One lane per device session, in the order the devices appeared.
    pub lanes: Vec<LaneView>,
    /// Every flow that started, by start time.
    pub bars: Vec<BarView>,
    /// Flows planned or claimed that never started: they have no place on the timeline.
    pub not_started: i64,
}

/// One agent run on a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WorkerRunView {
    /// The run, with its display liveness.
    pub run: AgentRunView,
    /// The pipeline attempt its job belongs to.
    pub pipeline_attempt_id: Uuid,
    /// Its job's key.
    pub job_key: String,
    /// When the run started.
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    /// When it ended; `None` while it may still be alive.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub ended_at: Option<OffsetDateTime>,
}

/// A page of a worker's runs, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WorkerRunPage {
    /// The runs.
    pub items: Vec<WorkerRunView>,
    /// Pass this as `cursor` for the next page; `None` on the last one.
    pub next_cursor: Option<String>,
}

/// What kind of device a session ran on. Filled from adb properties when the agent reports them;
/// absent until then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeviceProfileView {
    /// `android` or `ios`.
    pub platform: String,
    /// `emulator`, `simulator` or `physical`.
    pub kind: String,
    /// Device model.
    pub model: Option<String>,
    /// AVD name, or the simulator runtime.
    pub avd_or_runtime: Option<String>,
    /// OS version.
    pub os_version: Option<String>,
    /// Android API level.
    pub api_level: Option<i32>,
    /// CPU ABI.
    pub abi: Option<String>,
}

/// One device as one agent run saw it, with where it belongs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeviceSessionView {
    /// The device session (ID, serial, state).
    pub device: DeviceView,
    /// The worker it was attached to.
    pub worker_id: Option<Uuid>,
    /// That worker's runner name.
    pub runner_name: Option<String>,
    /// The pipeline attempt it served.
    pub pipeline_attempt_id: Uuid,
    /// The job it served.
    pub job_id: Uuid,
    /// That job's key.
    pub job_key: String,
    /// When the agent first saw it.
    #[serde(with = "time::serde::rfc3339")]
    pub first_seen_at: OffsetDateTime,
    /// When the agent last saw it.
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: OffsetDateTime,
    /// What kind of device it is, when known.
    pub profile: Option<DeviceProfileView>,
}

/// Screen C for a worker: the machine, its latest runs and the devices it hosted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WorkerView {
    /// The worker.
    pub worker: WorkerSummaryView,
    /// Its 10 latest runs; page further with `/workers/{id}/runs`.
    pub recent_runs: Vec<WorkerRunView>,
    /// Its 20 latest device sessions.
    pub devices: Vec<DeviceSessionView>,
}

/// Screen C for a device session: the device and the flows that ran on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeviceDetailView {
    /// The device session.
    pub session: DeviceSessionView,
    /// Its 100 latest flow executions, newest first (running ones first).
    pub flows: Vec<FlowView>,
    /// How many flow executions it hosted in all.
    pub flow_count: i64,
}

/// What a metric is measured in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum MetricUnit {
    /// Percent (CPU: of all cores for the host, of one core for a process).
    Percent,
    /// Bytes.
    Bytes,
    /// Degrees Celsius.
    Celsius,
    /// A plain number with no unit: a load average, a core count.
    Number,
}

/// What a metric is about. Host, process and guest figures are never mixed: a host CPU
/// percentage and an emulator guest's CPU percentage are different things (spec §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum MetricScope {
    /// The machine.
    Host,
    /// The Maestro process.
    Maestro,
    /// The Fleet agent itself.
    Agent,
    /// The emulator process, as the host sees it.
    Emulator,
    /// Inside the emulated device (guest metrics, explicitly labelled).
    Guest,
    /// A device's own sensors.
    Device,
}

/// Where a series' points come from, chosen by the range: raw samples up to a day, then
/// one-minute aggregates up to 30 days, then one-hour aggregates (PLAN §9 retention).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum MetricResolution {
    /// The agent's samples.
    #[serde(rename = "raw")]
    Raw,
    /// One-minute aggregates.
    #[serde(rename = "1m")]
    OneMinute,
    /// One-hour aggregates.
    #[serde(rename = "1h")]
    OneHour,
}

/// How an aggregated point was computed from its bucket's samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum MetricAggregate {
    /// The bucket's mean.
    Avg,
    /// The bucket's maximum.
    Max,
}

/// What a series describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum MetricSubjectKind {
    /// A worker (machine) and the processes on it.
    Worker,
    /// A device session.
    Device,
}

/// The worker or device a [`MetricsView`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MetricSubject {
    /// Worker or device.
    pub kind: MetricSubjectKind,
    /// Its ID.
    pub id: Uuid,
}

/// One reading, or the fact that there is none (acceptance criterion 5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MetricPoint {
    /// When it was taken (or the bucket began), unix milliseconds.
    pub at_ms: i64,
    /// The value; `None` exactly when `available` is false.
    pub value: Option<f64>,
    /// Whether the agent reported a value.
    pub available: bool,
    /// Why there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One metric over the requested range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SeriesView {
    /// Stable identifier, for example `host_cpu_pct`.
    pub metric: String,
    /// What to call it on screen.
    pub label: String,
    /// What it is measured in.
    pub unit: MetricUnit,
    /// What it is about.
    pub scope: MetricScope,
    /// The agent run a process series is about. A worker that ran several runs at once has one
    /// Maestro series per run; `None` for series about the worker or device as a whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<Uuid>,
    /// How points were aggregated; `None` for raw samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<MetricAggregate>,
    /// Whether at least one point has a value.
    pub available: bool,
    /// Why the series has no values, when it hasn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Points, ascending in time, at most the requested number. A run of unavailable readings
    /// is one unavailable point where it begins, so charts can draw the gap.
    pub points: Vec<MetricPoint>,
}

/// Screen C's metric series for a worker or a device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MetricsView {
    /// What the series describe.
    pub subject: MetricSubject,
    /// Start of the range, inclusive.
    #[serde(with = "time::serde::rfc3339")]
    pub from: OffsetDateTime,
    /// End of the range, exclusive.
    #[serde(with = "time::serde::rfc3339")]
    pub to: OffsetDateTime,
    /// Where the points come from.
    pub resolution: MetricResolution,
    /// More samples matched than one response carries: the series stop short of `to`.
    pub truncated: bool,
    /// One series per requested metric.
    pub series: Vec<SeriesView>,
}

/// One source's claim about a flow's outcome (invariant I4: all of them, not just the winner).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ClaimView {
    /// Where the evidence came from (`live_console`, `debug_log`, `artifact_file`,
    /// `junit_final`, `process_exit`, `agent_observed`, `server_inferred`, `declared`).
    pub source: String,
    /// What it claimed.
    pub outcome: String,
    /// When the source observed it.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub observed_at: Option<OffsetDateTime>,
    /// When the server stored it.
    #[serde(with = "time::serde::rfc3339")]
    pub recorded_at: OffsetDateTime,
    /// The parser and Maestro version profile that produced it, when it was parsed.
    pub parser: Option<String>,
    /// This claim decided the flow's displayed outcome.
    pub winning: bool,
}

/// One step of a flow execution, in the order Maestro ran them.
///
/// Steps are flat: the journal doesn't carry a step's parent or retry count yet, so nesting
/// (`runFlow` depth) and attempts can't be shown without inventing them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct StepView {
    /// Position among the steps as observed.
    pub index: i32,
    /// Maestro's description of the step, redacted.
    pub label: Option<String>,
    /// `running`, `completed`, `failed`, `warned`, `skipped`, `unknown`.
    pub status: String,
    /// Where the status came from.
    pub source: String,
    /// When it started.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    /// When it ended.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub ended_at: Option<OffsetDateTime>,
    /// `ended_at - started_at`, when both are known (agent wall clock).
    pub duration_ms: Option<i64>,
}

/// Why a flow failed, and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FailureView {
    /// The failure message, redacted.
    pub message: Option<String>,
    /// The first failed step.
    pub failed_step_index: Option<i32>,
    /// The last completed step before it (or the last one, when none failed).
    pub last_successful_step_index: Option<i32>,
}

/// An earlier execution of the same flow (same file, or same name when no file is known).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PreviousExecutionView {
    /// The flow execution.
    pub flow_id: Uuid,
    /// Its pipeline attempt.
    pub pipeline_attempt_id: Uuid,
    /// Its state.
    pub state: String,
    /// The winning claim's source.
    pub outcome_source: Option<String>,
    /// How settled the outcome is.
    pub finality: String,
    /// When it started.
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    /// Its duration.
    pub duration_ms: Option<i64>,
}

/// An artifact related to a flow: the short form on [`FlowDetailView`]. The full form, with a
/// link to open it, is [`ArtifactView`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ArtifactRefView {
    /// ID.
    pub id: Uuid,
    /// The artifact's class (screenshot, log, report, …).
    pub class: String,
    /// File name.
    pub name: String,
}

/// A short-lived signed link to an artifact on the usercontent origin. Mint a new one when it
/// lapses: ask for the list again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ArtifactLinkView {
    /// The absolute URL on the usercontent origin.
    pub url: String,
    /// When it stops working.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// One artifact of a flow, as listed for the dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ArtifactView {
    /// ID.
    pub id: Uuid,
    /// The flow it is evidence for; `None` for a file of the whole run (the JUnit report, the
    /// Maestro log), which is listed with every flow of the run.
    pub flow_execution_id: Option<Uuid>,
    /// The 0-based index of the flow step that produced the file, as Maestro names it. `None` when
    /// no step produced it, and for uploads that predate step links.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<u32>,
    /// The class: `failure_screenshot`, `failure_hierarchy`, `commands`, `junit`, `maestro_log` or `html_report`.
    pub class: String,
    /// File name.
    pub name: String,
    /// The path relative to the run's output directory (redacted by the agent).
    pub path: String,
    /// What the verified content is (for example `image/png`); for a refused artifact, what the
    /// uploader declared.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// SHA-256, lowercase hex.
    pub sha256: String,
    /// `available`, or `rejected` (verification refused it).
    pub state: String,
    /// Why it was refused: an artifact `RejectReason` code.
    pub reject_reason: Option<String>,
    /// The retention class (`failure_evidence`, `success_evidence`, `report`, `debug_log`, …).
    pub retention_class: String,
    /// When the upload was requested.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// How to open it. `None` for a refused artifact, and when the server has no usercontent
    /// origin configured (artifacts are stored but cannot be opened).
    pub link: Option<ArtifactLinkView>,
}

/// `GET /api/v1/projects/{project}/flows/{id}/artifacts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ArtifactListView {
    /// The flow's artifacts, oldest first.
    pub artifacts: Vec<ArtifactView>,
    /// The usercontent origin the links point at (for example
    /// `https://fleet-usercontent.example.net`): the one origin the dashboard may embed and
    /// fetch artifacts from. `None` when not configured.
    pub usercontent_url: Option<String>,
}

/// Screen D: everything about one flow execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FlowDetailView {
    /// The flow, with its resolved outcome, source and finality.
    pub flow: FlowView,
    /// When it started.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    /// When its outcome arrived.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub ended_at: Option<OffsetDateTime>,
    /// The CI run it belongs to.
    pub attempt: AttemptView,
    /// The CI job.
    pub job: JobView,
    /// The Maestro invocation.
    pub suite: SuiteView,
    /// The agent run, with its display liveness.
    pub agent_run: AgentRunView,
    /// The device it ran on.
    pub device: Option<DeviceSessionView>,
    /// Every claim about its outcome, in arrival order.
    pub claims: Vec<ClaimView>,
    /// Its steps.
    pub steps: Vec<StepView>,
    /// Why it failed; `None` for a flow that hasn't failed.
    pub failure: Option<FailureView>,
    /// Up to 10 earlier executions of the same flow, newest first.
    pub previous: Vec<PreviousExecutionView>,
    /// Related artifacts that are available; [`ArtifactListView`] has their links.
    pub artifacts: Vec<ArtifactRefView>,
}

/// The signed-in person, as `GET /api/v1/auth/session` tells the dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SessionUser {
    /// The user's ID.
    pub id: Uuid,
    /// Email, when the identity provider gave one.
    pub email: Option<String>,
    /// Display name, when the identity provider gave one.
    pub name: Option<String>,
}

/// A role the session holds on one project, or on every project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SessionRole {
    /// The project's slug; `None` for every project.
    pub project: Option<String>,
    /// `viewer`, `maintainer` or `admin`.
    pub role: String,
}

/// The signed-in session: who, what they may do, and the CSRF token to echo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SessionView {
    /// The person.
    pub user: SessionUser,
    /// Their roles by project.
    pub roles: Vec<SessionRole>,
    /// Send as `X-CSRF-Token` on every mutating request.
    pub csrf_token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_parses_from_the_documented_wire_form() {
        let wire = r#"{"user":{"id":"00000000-0000-0000-0000-000000000001","email":"a@b.c","name":null},
            "roles":[{"project":null,"role":"admin"},{"project":"app","role":"viewer"}],
            "csrf_token":"tok"}"#;
        let s: SessionView = serde_json::from_str(wire).unwrap();
        assert_eq!(s.user.email.as_deref(), Some("a@b.c"));
        assert_eq!(s.roles[1].project.as_deref(), Some("app"));
        assert_eq!(s.csrf_token, "tok");
    }
}
