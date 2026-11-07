// SPDX-License-Identifier: AGPL-3.0-only
//! What screen A shows, computed from the projects' [`OverviewView`]s and the failed flows of the
//! last day. No Leptos, no clock: the view passes "now" in where elapsed time is needed.
//!
//! A pipeline's verdict is derived from its flows (Foundations, rule 3), and the readouts count
//! pipelines by that verdict and by their flows.

use fleet_protocol::{
    live::{JobView, ProjectView},
    rest::{
        ActiveAttemptView, FlowCounts, MetricReading, MetricScope, MetricUnit, OverviewView,
        RecentAttemptView, StepView, WorkerSummaryView, history::HistoryEntryView,
    },
};
use fleet_ui::{
    charts::format::Unit,
    mark::Mark,
    provenance::{Provenance, STALE_AFTER_MS},
};
use uuid::Uuid;

use crate::{
    clock::unix_ms,
    format::{platform, ref_name, short_commit},
    live::socket::Connection,
    paths::{self, ci_run_url},
    progress::Progress,
    status::{Pill, flow_provenance, liveness_pill, source_provenance},
};

/// A project and its overview.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectOverview {
    /// The project.
    pub project: ProjectView,
    /// Its overview.
    pub overview: OverviewView,
}

/// Everything screen A loads: each project's overview and the failed flows of the last day.
#[derive(Debug, Clone, PartialEq)]
pub struct Board {
    /// Every project the person may read.
    pub projects: Vec<ProjectOverview>,
    /// The newest failed flows, every project, newest first.
    pub failed: Vec<FailedFlow>,
}

/// A failed flow, with what its detail adds to the history line.
#[derive(Debug, Clone, PartialEq)]
pub struct FailedFlow {
    /// The project's slug.
    pub project: String,
    /// The history line: the flow, its failure message and its run.
    pub entry: HistoryEntryView,
    /// The step that failed, as a label; `None` when the detail did not say.
    pub step: Option<String>,
    /// The device serial it ran on; `None` when no device was recorded.
    pub device: Option<String>,
}

/// The verdict of a pipeline, derived from its flows (Foundations, rule 3). Always labelled
/// Derived on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Some flow failed.
    Failed,
    /// Open, and no flow has failed.
    Running,
    /// Finished with a plan's total, every flow passed and every job's outcome is final.
    Passed,
    /// Finished, nothing failed, and the total is not known (or not every flow passed).
    NoFailures,
}

impl Verdict {
    /// The word and its mark.
    #[must_use]
    pub fn pill(self) -> Pill {
        match self {
            Self::Failed => Pill {
                label: "Failed",
                mark: Mark::Failed,
            },
            Self::Running => Pill {
                label: "Running",
                mark: Mark::Running,
            },
            Self::Passed => Pill {
                label: "Passed",
                mark: Mark::Passed,
            },
            Self::NoFailures => Pill {
                label: "No failures",
                mark: Mark::End,
            },
        }
    }
}

/// Derives a verdict. A failed flow decides it first, even while the pipeline is still open.
/// `all_final` says every job's outcome rests on a final source (a report or an artifact).
#[must_use]
pub fn verdict(open: bool, flows: &FlowCounts, planned: Option<i64>, all_final: bool) -> Verdict {
    let every_flow_passed = planned.is_some_and(|total| {
        total > 0
            && flows.passed == total
            && flows.pending == 0
            && flows.running == 0
            && flows.skipped == 0
            && flows.interrupted == 0
            && flows.unknown == 0
    });
    if flows.failed > 0 {
        Verdict::Failed
    } else if open {
        Verdict::Running
    } else if all_final && every_flow_passed {
        Verdict::Passed
    } else {
        Verdict::NoFailures
    }
}

/// A pipeline attempt, open or finished, as one row of the Pipelines table.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineRow {
    /// The project's slug.
    pub project: String,
    /// The attempt.
    pub attempt_id: Uuid,
    /// Where its page is.
    pub href: String,
    /// The CI run's key.
    pub run_key: String,
    /// Repository, branch and commit, as the CI reported them.
    pub run: String,
    /// Re-run attempt, from 1.
    pub attempt_number: i32,
    /// Whether any job is still open.
    pub open: bool,
    /// The verdict, derived from the flows.
    pub verdict: Verdict,
    /// Flows by state.
    pub flows: FlowCounts,
    /// Flow progress: known only with a plan.
    pub progress: Progress,
    /// How settled the results are; `None` when they are final.
    pub provenance: Option<Provenance>,
    /// First seen, unix ms.
    pub started_ms: i64,
    /// When the last job closed, unix ms; `None` while open.
    pub finished_ms: Option<i64>,
    /// The CI provider's page for the run, when known.
    pub ci_url: Option<String>,
}

impl PipelineRow {
    /// Whether this row is in the given readout.
    #[must_use]
    pub fn in_filter(&self, filter: Filter) -> bool {
        match filter {
            Filter::All => true,
            Filter::Running => self.open,
            Filter::Failed => self.flows.failed > 0,
            Filter::NoFailures => !self.open && self.flows.failed == 0,
        }
    }
}

/// The readouts above the Pipelines table, which filter it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Filter {
    /// Every pipeline.
    All,
    /// Pipelines with an open job.
    Running,
    /// Pipelines with a failed flow.
    Failed,
    /// Finished pipelines with no failed flow.
    NoFailures,
}

impl Filter {
    /// The filter a `filter=` query value names; anything else is [`Filter::All`].
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("running") => Self::Running,
            Some("failed") => Self::Failed,
            Some("no-failures") => Self::NoFailures,
            _ => Self::All,
        }
    }

    /// The value in the URL; `All` has none.
    #[must_use]
    pub const fn value(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Running => "running",
            Self::Failed => "failed",
            Self::NoFailures => "no-failures",
        }
    }

    /// What the table says when the filter leaves nothing to show.
    #[must_use]
    pub const fn empty_title(self) -> &'static str {
        match self {
            Self::All => "No pipelines yet",
            Self::Running => "No active pipelines",
            Self::Failed => "No failed pipelines",
            Self::NoFailures => "No finished pipelines without failures",
        }
    }
}

/// The readouts' counts. Pipelines by filter; flows by state, summed over the pipelines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Readouts {
    /// Every pipeline shown.
    pub all: u64,
    /// Pipelines with an open job.
    pub running: u64,
    /// Flows running right now, over the open pipelines.
    pub running_flows: u64,
    /// Pipelines with a failed flow.
    pub failed: u64,
    /// Flows that failed, over every pipeline shown.
    pub failed_flows: u64,
    /// Finished pipelines with no failed flow.
    pub no_failures: u64,
    /// Emulators active across the projects: the server's count, not a count of pipelines, so the
    /// readouts filter nothing by it.
    pub emulators: u64,
}

impl Readouts {
    /// Counts the rows.
    #[must_use]
    pub fn of(rows: &[PipelineRow]) -> Self {
        let mut readouts = Self {
            all: count_rows(rows.len()),
            ..Self::default()
        };
        for row in rows {
            if row.in_filter(Filter::Running) {
                readouts.running += 1;
                readouts.running_flows += count(row.flows.running);
            }
            if row.in_filter(Filter::Failed) {
                readouts.failed += 1;
            }
            if row.in_filter(Filter::NoFailures) {
                readouts.no_failures += 1;
            }
            readouts.failed_flows += count(row.flows.failed);
        }
        readouts
    }
}

/// Runners by liveness, summed over the projects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunnerCounts {
    /// Frames are arriving.
    pub online: u64,
    /// Silent for longer than the stale threshold.
    pub stale: u64,
    /// The agent's socket is gone and its run has not ended.
    pub disconnected: u64,
    /// Every run on it ended.
    pub completed: u64,
}

impl RunnerCounts {
    /// The counts that are not zero, in words: "1 online · 1 stale". Zero counts are left out,
    /// and so is the completed count, which is not a problem to point at.
    #[must_use]
    pub fn words(self) -> String {
        [
            (self.online, "online"),
            (self.stale, "stale"),
            (self.disconnected, "disconnected"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, word)| format!("{n} {word}"))
        .collect::<Vec<_>>()
        .join(" · ")
    }
}

/// One runner's row.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerRow {
    /// Where its page is.
    pub href: String,
    /// Runner name, else a short id.
    pub name: String,
    /// `linux/x86_64`, when reported.
    pub platform: Option<String>,
    /// Liveness.
    pub liveness: Pill,
    /// Agent runs that may still be alive: its current workload.
    pub active_runs: i64,
    /// Host CPU in the newest readings, as text; `None` when not reported.
    pub cpu: Option<String>,
    /// When the readings were taken, unix ms.
    pub readings_at_ms: Option<i64>,
}

/// A failed flow for "Needs attention", with where it is and what it said.
#[derive(Debug, Clone, PartialEq)]
pub struct AttentionRow {
    /// Where the flow's page is.
    pub flow_href: String,
    /// The flow's name.
    pub name: String,
    /// The failure message, redacted by the server.
    pub message: Option<String>,
    /// When it ended, unix ms.
    pub at_ms: i64,
    /// How settled the failure is; `None` when it is verified.
    pub provenance: Option<Provenance>,
    /// The failed step's label.
    pub step: Option<String>,
    /// The device it ran on.
    pub device: Option<String>,
    /// Where its pipeline's page is.
    pub run_href: String,
    /// The pipeline's repository, branch and commit.
    pub run: String,
}

/// Screen A as a whole.
#[derive(Debug, Clone, PartialEq)]
pub struct OverviewModel {
    /// The readouts' counts.
    pub readouts: Readouts,
    /// Open pipelines first, newest first, then the finished ones, newest first.
    pub pipelines: Vec<PipelineRow>,
    /// Failed flows, newest first.
    pub attention: Vec<AttentionRow>,
    /// Runners by liveness, every project.
    pub runners: RunnerCounts,
    /// Runners, every project.
    pub workers: Vec<WorkerRow>,
}

impl OverviewModel {
    /// Builds screen A from the board.
    #[must_use]
    pub fn build(board: &Board) -> Self {
        let mut runners = RunnerCounts::default();
        let mut open = Vec::new();
        let mut finished = Vec::new();
        let mut workers = Vec::new();
        for p in &board.projects {
            let slug = &p.project.slug;
            let o = &p.overview;
            runners.online += count(o.worker_counts.online);
            runners.stale += count(o.worker_counts.stale);
            runners.disconnected += count(o.worker_counts.disconnected);
            runners.completed += count(o.worker_counts.completed);
            open.extend(o.active.iter().map(|a| active_row(slug, a)));
            finished.extend(o.recent.iter().map(|r| finished_row(slug, r)));
            workers.extend(o.workers.iter().map(|w| worker_row(slug, w)));
        }
        open.sort_by_key(|r| std::cmp::Reverse(r.started_ms));
        finished.sort_by_key(|r| std::cmp::Reverse(r.finished_ms));
        open.extend(finished);
        let pipelines = open;
        let mut attention: Vec<AttentionRow> = board.failed.iter().map(attention_row).collect();
        attention.sort_by_key(|r| std::cmp::Reverse(r.at_ms));
        Self {
            readouts: Readouts {
                emulators: board
                    .projects
                    .iter()
                    .map(|p| count(p.overview.active_emulators))
                    .sum(),
                ..Readouts::of(&pipelines)
            },
            pipelines,
            attention,
            runners,
            workers,
        }
    }
}

/// Whether the overview's socket side says Live, Reconnecting or Connecting. Nothing is
/// connected before the first load, and a project with no socket has nothing to follow once the
/// list is in.
#[must_use]
pub fn feed(states: &[Connection], loaded: bool) -> Connection {
    if !loaded {
        Connection::Connecting
    } else if states.contains(&Connection::Reconnecting) {
        Connection::Reconnecting
    } else if states.iter().all(|s| *s == Connection::Live) {
        Connection::Live
    } else {
        Connection::Connecting
    }
}

/// The flows of a pipeline in words, non-zero counts only: "4 passed · 1 failed".
#[must_use]
pub fn composition(f: &FlowCounts) -> String {
    [
        (f.passed, "passed"),
        (f.failed, "failed"),
        (f.running, "running"),
        (f.skipped, "skipped"),
        (f.interrupted, "interrupted"),
        (f.unknown, "unknown"),
        (f.pending, "not started"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, word)| format!("{n} {word}"))
    .collect::<Vec<_>>()
    .join(" · ")
}

/// The step that failed, as its label, or "Step N" (1-based) when Maestro gave no description.
/// `None` when the flow did not say which step failed.
#[must_use]
pub fn failed_step(index: Option<i32>, steps: &[StepView]) -> Option<String> {
    let index = index?;
    let described = StepView::flatten(steps)
        .into_iter()
        .find(|s| s.index == index)
        .and_then(|s| s.label.clone());
    Some(described.unwrap_or_else(|| format!("Step {}", index.saturating_add(1))))
}

fn active_row(slug: &str, a: &ActiveAttemptView) -> PipelineRow {
    let at = &a.attempt;
    let flows = a.flows.clone();
    let planned = at.planned_total;
    PipelineRow {
        project: slug.to_owned(),
        attempt_id: at.id,
        href: paths::pipeline_href(slug, at.id),
        run_key: at.run_key.clone(),
        run: run_line(
            at.repository.as_deref(),
            at.git_ref.as_deref(),
            at.commit_sha.as_deref(),
        ),
        attempt_number: at.attempt,
        open: true,
        verdict: verdict(true, &flows, planned, false),
        progress: Progress::from_counts(&flows, planned),
        flows,
        // Read live: a verdict on an open run may still change.
        provenance: Some(Provenance::Provisional),
        started_ms: unix_ms(at.first_seen_at),
        finished_ms: None,
        ci_url: ci_run_url(&at.provider, at.repository.as_deref(), &at.run_key),
    }
}

fn finished_row(slug: &str, r: &RecentAttemptView) -> PipelineRow {
    let at = &r.attempt;
    let flows = r.flows.clone();
    let planned = at.planned_total;
    let all_final = !r.jobs.is_empty()
        && r.jobs
            .iter()
            .all(|j| source_provenance(j.outcome_source.as_deref()) == Provenance::Final);
    PipelineRow {
        project: slug.to_owned(),
        attempt_id: at.id,
        href: paths::pipeline_href(slug, at.id),
        run_key: at.run_key.clone(),
        run: run_line(
            at.repository.as_deref(),
            at.git_ref.as_deref(),
            at.commit_sha.as_deref(),
        ),
        attempt_number: at.attempt,
        open: false,
        verdict: verdict(false, &flows, planned, all_final),
        progress: Progress::from_counts(&flows, planned),
        provenance: weakest_job(&r.jobs),
        flows,
        started_ms: unix_ms(at.first_seen_at),
        finished_ms: Some(unix_ms(r.finished_at)),
        ci_url: ci_run_url(&at.provider, at.repository.as_deref(), &at.run_key),
    }
}

/// The first job whose outcome does not rest on a final source: the finished row's mark.
fn weakest_job(jobs: &[JobView]) -> Option<Provenance> {
    jobs.iter()
        .map(|j| source_provenance(j.outcome_source.as_deref()))
        .find(|p| *p != Provenance::Final)
}

fn attention_row(f: &FailedFlow) -> AttentionRow {
    let e = &f.entry;
    let provenance = match flow_provenance(&e.finality, &e.state, None) {
        Provenance::Final => None,
        other => Some(other),
    };
    AttentionRow {
        flow_href: paths::flow_href(&f.project, e.flow_id),
        name: e.name.clone(),
        message: e.failure_message.clone(),
        at_ms: unix_ms(e.at),
        provenance,
        step: f.step.clone(),
        device: f.device.clone(),
        run_href: paths::pipeline_href(&f.project, e.attempt_id),
        run: run_line(
            e.repository.as_deref(),
            e.git_ref.as_deref(),
            e.commit_sha.as_deref(),
        ),
    }
}

/// "repo · branch · commit", the parts that are known.
#[must_use]
pub fn run_line(
    repository: Option<&str>,
    git_ref: Option<&str>,
    commit_sha: Option<&str>,
) -> String {
    [
        repository.map(str::to_owned),
        git_ref.map(|r| ref_name(r).to_owned()),
        commit_sha.map(short_commit),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ")
}

fn worker_row(slug: &str, w: &WorkerSummaryView) -> WorkerRow {
    WorkerRow {
        href: paths::worker_href(slug, w.id),
        name: w
            .runner_name
            .clone()
            .unwrap_or_else(|| short_commit(&w.id.to_string())),
        platform: platform(w.os.as_deref(), w.arch.as_deref()),
        liveness: liveness_pill(w.liveness),
        active_runs: w.active_runs,
        cpu: w.latest.as_ref().and_then(|l| cpu_now(&l.readings)),
        readings_at_ms: w.latest.as_ref().map(|l| unix_ms(l.at)),
    }
}

/// How fresh a worker's readings are at `now_ms`: observed until the freshness threshold, then
/// stale; missing when there are none.
#[must_use]
pub fn freshness(readings_at_ms: Option<i64>, now_ms: i64) -> Provenance {
    match readings_at_ms {
        None => Provenance::Missing,
        Some(at) => Provenance::Observed.displayed(
            u64::try_from(now_ms.saturating_sub(at)).unwrap_or(0),
            STALE_AFTER_MS,
        ),
    }
}

/// The host metric that stands for a runner's utilization.
const CPU_METRIC: &str = "host_cpu_pct";

/// The host CPU as the agent reported it, in words; `None` when it did not.
fn cpu_now(readings: &[MetricReading]) -> Option<String> {
    readings
        .iter()
        .find(|r| r.scope == MetricScope::Host && r.metric == CPU_METRIC)
        .and_then(|r| match (r.available, r.value) {
            (true, Some(v)) => Some(unit(r.unit).format(v)),
            _ => None,
        })
}

fn unit(u: MetricUnit) -> Unit {
    match u {
        MetricUnit::Percent => Unit::Percent,
        MetricUnit::Bytes => Unit::Bytes,
        MetricUnit::Celsius => Unit::Celsius,
        MetricUnit::Number => Unit::Number,
    }
}

fn count(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn count_rows(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn attempt(id: &str, run_key: &str, provider: &str, planned: &Value) -> Value {
        json!({
            "id": id, "project_id": "00000000-0000-0000-0000-0000000000a1",
            "provider": provider, "run_key": run_key, "repository": "acme/app",
            "commit_sha": "0123456789abcdef", "git_ref": "refs/heads/main", "attempt": 2,
            "version": 9, "planned_total": planned, "jobs": 2, "open_jobs": 1,
            "first_seen_at": "2026-01-01T00:00:10Z"
        })
    }

    fn reading(metric: &str, label: &str, unit: &str, scope: &str, value: &Value) -> Value {
        let available = !value.is_null();
        json!({
            "metric": metric, "label": label, "unit": unit, "scope": scope, "value": value,
            "available": available,
            "reason": if available { Value::Null } else { json!("the agent did not report it") }
        })
    }

    fn worker(id: &str, liveness: &str, cpu: &Value) -> Value {
        json!({
            "id": id, "runner_name": "runner-1", "os": "linux", "arch": "x86_64",
            "first_seen_at": "2026-01-01T00:00:00Z", "last_seen_at": "2026-01-01T00:01:00Z",
            "liveness": liveness, "active_runs": 2,
            "latest": { "at": "2026-01-01T00:01:00Z", "readings": [
                reading("host_cpu_pct", "Host CPU", "percent", "host", cpu),
                reading("host_mem_used_bytes", "Host memory used", "bytes", "host",
                        &json!(4_294_967_296_u64)),
                reading("maestro_cpu_pct", "Maestro CPU", "percent", "maestro", &json!(80.0)),
            ]}
        })
    }

    fn overview() -> OverviewView {
        serde_json::from_value(json!({
            "generated_at": "2026-01-01T00:01:00Z",
            "active": [{
                "attempt": attempt("00000000-0000-0000-0000-000000000001", "acme/app:9001",
                                   "github_actions", &json!(6)),
                "jobs": {"pending": 0, "starting": 0, "running": 1, "finalizing": 0, "closed": 1},
                "flows": {"pending": 1, "running": 1, "passed": 3, "failed": 1, "skipped": 0,
                          "interrupted": 0, "unknown": 0}
            }, {
                "attempt": attempt("00000000-0000-0000-0000-000000000002", "loadgen/app:7",
                                   "generic", &Value::Null),
                "jobs": {"pending": 0, "starting": 0, "running": 2, "finalizing": 0, "closed": 0},
                "flows": {"pending": 0, "running": 2, "passed": 0, "failed": 0, "skipped": 0,
                          "interrupted": 0, "unknown": 0}
            }],
            "active_total": 2,
            "recent": [{
                "attempt": attempt("00000000-0000-0000-0000-000000000003", "acme/app:8000",
                                   "github_actions", &json!(2)),
                "finished_at": "2026-01-01T00:00:50Z",
                "jobs": [{
                    "id": "00000000-0000-0000-0000-0000000000b1",
                    "pipeline_attempt_id": "00000000-0000-0000-0000-000000000003",
                    "job_key": "e2e", "name": "E2E", "lifecycle": "closed",
                    "outcome": "failed", "outcome_source": "process_exit",
                    "reopened_count": 0, "rev": 4
                }],
                "flows": {"pending": 0, "running": 0, "passed": 1, "failed": 1, "skipped": 0,
                          "interrupted": 0, "unknown": 0}
            }, {
                "attempt": attempt("00000000-0000-0000-0000-000000000004", "acme/app:8001",
                                   "github_actions", &json!(2)),
                "finished_at": "2026-01-01T00:00:40Z",
                "jobs": [{
                    "id": "00000000-0000-0000-0000-0000000000b2",
                    "pipeline_attempt_id": "00000000-0000-0000-0000-000000000004",
                    "job_key": "e2e", "name": "E2E", "lifecycle": "closed",
                    "outcome": "completed", "outcome_source": "junit_final",
                    "reopened_count": 0, "rev": 4
                }],
                "flows": {"pending": 0, "running": 0, "passed": 2, "failed": 0, "skipped": 0,
                          "interrupted": 0, "unknown": 0}
            }],
            "active_emulators": 2,
            "worker_counts": {"online": 1, "stale": 1, "disconnected": 0, "completed": 0},
            "workers": [
                worker("00000000-0000-0000-0000-0000000000c1", "online", &json!(40.0)),
                worker("00000000-0000-0000-0000-0000000000c2", "stale", &json!(90.0)),
            ]
        }))
        .unwrap()
    }

    fn project() -> ProjectView {
        ProjectView {
            id: Uuid::from_u128(0xa1),
            slug: "app".into(),
            name: "App".into(),
        }
    }

    fn failed_entry() -> HistoryEntryView {
        serde_json::from_value(json!({
            "flow_id": "00000000-0000-0000-0000-0000000000f1",
            "flow": "flows/search.yaml", "name": "Sport filter shows the Vespa",
            "attempt_id": "00000000-0000-0000-0000-000000000003",
            "repository": "acme/app", "commit_sha": "0123456789abcdef",
            "git_ref": "refs/heads/main", "state": "failed", "outcome_source": "process_exit",
            "finality": "provisional", "at": "2026-01-01T00:00:45Z", "duration_ms": 4000,
            "failure_message": "Assertion is false: \"Vespa\" is visible",
            "failure_fingerprint": null, "condition_fingerprint": null
        }))
        .unwrap()
    }

    fn board() -> Board {
        Board {
            projects: vec![ProjectOverview {
                project: project(),
                overview: overview(),
            }],
            failed: vec![FailedFlow {
                project: "app".into(),
                entry: failed_entry(),
                step: Some("Assert visible: Vespa".into()),
                device: Some("emulator-5554".into()),
            }],
        }
    }

    fn model() -> OverviewModel {
        OverviewModel::build(&board())
    }

    #[test]
    fn open_pipelines_come_first_then_the_finished_ones_newest_first() {
        let m = model();
        let order: Vec<&str> = m.pipelines.iter().map(|r| r.run_key.as_str()).collect();
        assert_eq!(
            order,
            [
                "acme/app:9001",
                "loadgen/app:7",
                "acme/app:8000",
                "acme/app:8001"
            ]
        );
        assert!(m.pipelines[0].open && m.pipelines[1].open);
        assert!(!m.pipelines[2].open);
        assert_eq!(
            m.pipelines[2].finished_ms,
            Some(1_767_225_650_000),
            "8000 ended later"
        );
    }

    #[test]
    fn the_readouts_count_pipelines_by_verdict_and_flows_by_state() {
        let r = model().readouts;
        assert_eq!(
            r,
            Readouts {
                all: 4,
                running: 2,
                running_flows: 3,
                failed: 2,
                failed_flows: 2,
                no_failures: 1,
                emulators: 2,
            },
            "the failed open run counts as failed too; finished without failures is one"
        );
    }

    #[test]
    fn a_failed_flow_decides_the_verdict_even_while_the_run_is_open() {
        let m = model();
        assert_eq!(m.pipelines[0].verdict, Verdict::Failed);
        assert_eq!(m.pipelines[1].verdict, Verdict::Running);
        assert_eq!(m.pipelines[2].verdict, Verdict::Failed);
        assert_eq!(
            m.pipelines[3].verdict,
            Verdict::Passed,
            "two of two, junit final"
        );
    }

    #[test]
    fn passed_needs_a_plan_total_every_flow_passed_and_final_outcomes() {
        let flows = FlowCounts {
            passed: 2,
            ..FlowCounts::default()
        };
        assert_eq!(verdict(false, &flows, Some(2), true), Verdict::Passed);
        assert_eq!(
            verdict(false, &flows, None, true),
            Verdict::NoFailures,
            "no plan"
        );
        assert_eq!(
            verdict(false, &flows, Some(3), true),
            Verdict::NoFailures,
            "a flow missing"
        );
        assert_eq!(
            verdict(false, &flows, Some(2), false),
            Verdict::NoFailures,
            "an observed outcome is not verified"
        );
    }

    #[test]
    fn an_open_run_is_provisional_and_a_finished_run_carries_the_weakest_job_source() {
        let m = model();
        assert_eq!(m.pipelines[0].provenance, Some(Provenance::Provisional));
        assert_eq!(
            m.pipelines[2].provenance,
            Some(Provenance::Observed),
            "a process exit is observed, so the row says so"
        );
        assert_eq!(
            m.pipelines[3].provenance, None,
            "a junit report is final: silent"
        );
    }

    #[test]
    fn an_open_row_links_to_its_page_and_marks_unplanned_progress_uncertain() {
        let m = model();
        let planned = &m.pipelines[0];
        assert_eq!(
            planned.href,
            "/p/app/pipelines/00000000-0000-0000-0000-000000000001"
        );
        assert_eq!(planned.run_key, "acme/app:9001");
        assert_eq!(planned.run, "acme/app · main · 0123456");
        assert_eq!(planned.progress.text(), "4 of 6 flows finished · 1 running");
        assert_eq!(
            planned.ci_url.as_deref(),
            Some("https://github.com/acme/app/actions/runs/9001")
        );
        assert_eq!(planned.started_ms, 1_767_225_610_000);

        let unplanned = &m.pipelines[1];
        assert!(
            unplanned.progress.is_uncertain(),
            "no planned_total: never guessed"
        );
        assert_eq!(unplanned.ci_url, None);
    }

    #[test]
    fn a_filter_keeps_the_rows_its_readout_counts() {
        let m = model();
        let kept = |f: Filter| {
            m.pipelines
                .iter()
                .filter(|r| r.in_filter(f))
                .map(|r| r.run_key.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(kept(Filter::Running), ["acme/app:9001", "loadgen/app:7"]);
        assert_eq!(kept(Filter::Failed), ["acme/app:9001", "acme/app:8000"]);
        assert_eq!(kept(Filter::NoFailures), ["acme/app:8001"]);
        assert_eq!(kept(Filter::All).len(), 4);
        assert_eq!(Filter::parse(Some("no-failures")), Filter::NoFailures);
        assert_eq!(Filter::parse(Some("bogus")), Filter::All);
        assert_eq!(Filter::Running.empty_title(), "No active pipelines");
    }

    #[test]
    fn an_attention_row_carries_its_message_step_device_and_run_and_its_own_mark() {
        let m = model();
        let a = &m.attention[0];
        assert_eq!(
            a.flow_href,
            "/flows/00000000-0000-0000-0000-0000000000f1?project=app"
        );
        assert_eq!(
            a.message.as_deref(),
            Some("Assertion is false: \"Vespa\" is visible")
        );
        assert_eq!(a.step.as_deref(), Some("Assert visible: Vespa"));
        assert_eq!(a.device.as_deref(), Some("emulator-5554"));
        assert_eq!(
            a.run_href,
            "/p/app/pipelines/00000000-0000-0000-0000-000000000003"
        );
        assert_eq!(a.run, "acme/app · main · 0123456");
        assert_eq!(
            a.provenance,
            Some(Provenance::Provisional),
            "not verified yet: marked"
        );
    }

    #[test]
    fn a_verified_failure_is_silent() {
        let mut b = board();
        b.failed[0].entry.finality = "verified".into();
        assert_eq!(OverviewModel::build(&b).attention[0].provenance, None);
    }

    #[test]
    fn a_runner_shows_its_cpu_now_and_only_the_host_figure() {
        let m = model();
        let w = &m.workers[0];
        assert_eq!(
            w.href,
            "/workers/00000000-0000-0000-0000-0000000000c1?project=app"
        );
        assert_eq!(w.name, "runner-1");
        assert_eq!(w.platform.as_deref(), Some("linux/x86_64"));
        assert_eq!(w.liveness.label, "Online");
        assert_eq!(w.active_runs, 2);
        assert_eq!(
            w.cpu.as_deref(),
            Some("40%"),
            "the host CPU, not Maestro's 80%"
        );
        assert_eq!(w.readings_at_ms, Some(1_767_225_660_000));
    }

    #[test]
    fn a_runner_without_a_cpu_reading_has_none_rather_than_zero() {
        let mut o = overview();
        o.workers[0].latest.as_mut().unwrap().readings[0].available = false;
        o.workers[0].latest.as_mut().unwrap().readings[0].value = None;
        let m = OverviewModel::build(&Board {
            projects: vec![ProjectOverview {
                project: project(),
                overview: o,
            }],
            failed: Vec::new(),
        });
        assert_eq!(m.workers[0].cpu, None);
    }

    #[test]
    fn the_runner_counts_name_only_the_non_zero_ones() {
        let counts = RunnerCounts {
            online: 0,
            stale: 0,
            disconnected: 1,
            completed: 3,
        };
        assert_eq!(
            counts.words(),
            "1 disconnected",
            "completed runners are not counted here"
        );
        assert_eq!(
            RunnerCounts {
                online: 1,
                stale: 2,
                ..RunnerCounts::default()
            }
            .words(),
            "1 online · 2 stale"
        );
        assert_eq!(RunnerCounts::default().words(), "");
    }

    #[test]
    fn the_failed_step_is_its_label_or_a_one_based_number() {
        let steps: Vec<StepView> = serde_json::from_value(json!([
            {"index": 0, "label": "Setup", "status": "completed", "source": "maestro_log"},
            {"index": 1, "label": null, "status": "failed", "source": "maestro_log"}
        ]))
        .unwrap();
        assert_eq!(failed_step(Some(0), &steps).as_deref(), Some("Setup"));
        assert_eq!(
            failed_step(Some(1), &steps).as_deref(),
            Some("Step 2"),
            "unlabelled: 1-based"
        );
        assert_eq!(
            failed_step(None, &steps),
            None,
            "the flow did not say which step failed"
        );
    }

    #[test]
    fn the_feed_follows_every_socket_and_waits_for_the_first_load() {
        assert_eq!(feed(&[Connection::Live], false), Connection::Connecting);
        assert_eq!(
            feed(&[Connection::Live, Connection::Live], true),
            Connection::Live
        );
        assert_eq!(
            feed(&[Connection::Live, Connection::Reconnecting], true),
            Connection::Reconnecting
        );
        assert_eq!(
            feed(&[Connection::Live, Connection::Connecting], true),
            Connection::Connecting
        );
        assert_eq!(
            feed(&[], true),
            Connection::Live,
            "no project: nothing to follow"
        );
    }

    #[test]
    fn a_finished_pipeline_says_what_its_flows_did_in_non_zero_words() {
        let f = FlowCounts {
            passed: 4,
            failed: 1,
            pending: 2,
            ..FlowCounts::default()
        };
        assert_eq!(composition(&f), "4 passed · 1 failed · 2 not started");
        assert_eq!(composition(&FlowCounts::default()), "");
    }

    #[test]
    fn readings_turn_stale_after_the_freshness_threshold() {
        let at = Some(1_000_000);
        assert_eq!(freshness(at, 1_000_000 + 24_999), Provenance::Observed);
        assert_eq!(freshness(at, 1_000_000 + 25_000), Provenance::Stale);
        assert_eq!(freshness(None, 1_000_000), Provenance::Missing);
    }
}
