// SPDX-License-Identifier: AGPL-3.0-only
//! What screen A shows, computed from the projects' [`OverviewView`]s. No Leptos, no clock: the
//! view passes "now" in where elapsed time is needed.

use fleet_protocol::{
    live::{AttemptView, Liveness, ProjectView},
    rest::{
        ActiveAttemptView, MetricReading, MetricScope, MetricUnit, OverviewView, RecentAttemptView,
        WorkerSummaryView,
    },
};
use fleet_ui::{
    charts::format::Unit,
    provenance::{Provenance, STALE_AFTER_MS},
};
use uuid::Uuid;

use crate::{
    clock::unix_ms,
    format::{platform, ref_name, short_commit},
    paths::{self, ci_run_url},
    progress::Progress,
    status::{Pill, attempt_pill, job_pill, liveness_pill, source_provenance},
};

/// A project and its overview.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectOverview {
    /// The project.
    pub project: ProjectView,
    /// Its overview.
    pub overview: OverviewView,
}

/// The headline numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// Pipeline attempts with an open job.
    pub active_attempts: u64,
    /// Jobs running right now.
    pub running_jobs: u64,
    /// Flows running right now, over the active attempts.
    pub running_flows: u64,
    /// Flows that passed, over the active attempts.
    pub passed_flows: u64,
    /// Flows that failed, over the active attempts: what needs attention.
    pub failed_flows: u64,
    /// Runners seen in the last day, by liveness.
    pub runners: RunnerCounts,
    /// Mean host CPU over the online workers that reported it; `None` when none did.
    pub average_cpu_pct: Option<f64>,
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

/// An attempt that is running.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveRow {
    /// The project's slug.
    pub project: String,
    /// The attempt.
    pub attempt_id: Uuid,
    /// Where its page is.
    pub href: String,
    /// What to call it: the repository, else the run key.
    pub title: String,
    /// Git ref.
    pub git_ref: Option<String>,
    /// Commit, shortened to 7 characters.
    pub commit: Option<String>,
    /// Re-run attempt, from 1.
    pub attempt_number: i32,
    /// Its overall state.
    pub status: Pill,
    /// Flow progress, or uncertain.
    pub progress: Progress,
    /// First seen, unix ms.
    pub started_ms: i64,
    /// The CI provider's page for the run, when known.
    pub ci_url: Option<String>,
}

/// An attempt that finished.
#[derive(Debug, Clone, PartialEq)]
pub struct RecentRow {
    /// The project's slug.
    pub project: String,
    /// The attempt.
    pub attempt_id: Uuid,
    /// Where its page is.
    pub href: String,
    /// What to call it.
    pub title: String,
    /// Git ref.
    pub git_ref: Option<String>,
    /// Commit, shortened.
    pub commit: Option<String>,
    /// Re-run attempt.
    pub attempt_number: i32,
    /// Each job's name and outcome pill, with the evidence source its outcome rests on.
    pub jobs: Vec<RecentJob>,
    /// Flow progress at the end.
    pub progress: Progress,
    /// First seen, unix ms.
    pub started_ms: i64,
    /// When the last job closed, unix ms.
    pub finished_ms: i64,
}

/// A finished job in a [`RecentRow`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecentJob {
    /// Its name.
    pub name: String,
    /// Outcome.
    pub pill: Pill,
    /// How settled the outcome is.
    pub provenance: Provenance,
}

/// One runner's health.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerRow {
    /// The project's slug.
    pub project: String,
    /// The worker.
    pub worker_id: Uuid,
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
    /// The newest host readings: CPU, memory, thermal.
    pub readings: Vec<Reading>,
    /// When the readings were taken, unix ms.
    pub readings_at_ms: Option<i64>,
}

/// One metric of a worker, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// What it is, as the server labels it.
    pub label: String,
    /// The value with its unit, or "Unavailable".
    pub text: String,
    /// Whether the agent reported a value.
    pub available: bool,
    /// Why there is none.
    pub reason: Option<String>,
}

/// Screen A as a whole.
#[derive(Debug, Clone, PartialEq)]
pub struct OverviewModel {
    /// Headline numbers.
    pub summary: Summary,
    /// Running attempts, every project, newest first.
    pub active: Vec<ActiveRow>,
    /// Finished attempts, every project, newest first.
    pub recent: Vec<RecentRow>,
    /// Runners, every project.
    pub workers: Vec<WorkerRow>,
}

impl OverviewModel {
    /// Builds screen A from every project's overview.
    #[must_use]
    pub fn build(projects: &[ProjectOverview]) -> Self {
        let mut summary = Summary {
            active_attempts: 0,
            running_jobs: 0,
            running_flows: 0,
            passed_flows: 0,
            failed_flows: 0,
            runners: RunnerCounts::default(),
            average_cpu_pct: None,
        };
        let mut active = Vec::new();
        let mut recent = Vec::new();
        let mut workers = Vec::new();
        let mut cpu = Vec::new();
        for p in projects {
            let slug = &p.project.slug;
            let o = &p.overview;
            summary.active_attempts += count(o.active_total);
            summary.runners.online += count(o.worker_counts.online);
            summary.runners.stale += count(o.worker_counts.stale);
            summary.runners.disconnected += count(o.worker_counts.disconnected);
            summary.runners.completed += count(o.worker_counts.completed);
            for a in &o.active {
                summary.running_jobs += count(a.jobs.running);
                summary.running_flows += count(a.flows.running);
                summary.passed_flows += count(a.flows.passed);
                summary.failed_flows += count(a.flows.failed);
                active.push(active_row(slug, a));
            }
            recent.extend(o.recent.iter().map(|r| recent_row(slug, r)));
            for w in &o.workers {
                if w.liveness == Some(Liveness::Online) {
                    cpu.extend(
                        w.latest
                            .iter()
                            .flat_map(|l| &l.readings)
                            .filter(|r| r.metric == CPU_METRIC)
                            .filter_map(|r| r.value),
                    );
                }
                workers.push(worker_row(slug, w));
            }
        }
        if !cpu.is_empty() {
            #[expect(
                clippy::cast_precision_loss,
                reason = "a runner count is far below 2^52, so the conversion is exact"
            )]
            let n = cpu.len() as f64;
            summary.average_cpu_pct = Some(cpu.iter().sum::<f64>() / n);
        }
        active.sort_by_key(|r| std::cmp::Reverse(r.started_ms));
        recent.sort_by_key(|r| std::cmp::Reverse(r.finished_ms));
        Self {
            summary,
            active,
            recent,
            workers,
        }
    }
}

fn active_row(slug: &str, a: &ActiveAttemptView) -> ActiveRow {
    let at = &a.attempt;
    ActiveRow {
        project: slug.to_owned(),
        attempt_id: at.id,
        href: paths::pipeline_href(slug, at.id),
        title: title(at),
        git_ref: at.git_ref.as_deref().map(|r| ref_name(r).to_owned()),
        commit: at.commit_sha.as_deref().map(short_commit),
        attempt_number: at.attempt,
        status: attempt_pill(&a.jobs),
        progress: Progress::from_counts(&a.flows, at.planned_total),
        started_ms: unix_ms(at.first_seen_at),
        ci_url: ci_run_url(&at.provider, at.repository.as_deref(), &at.run_key),
    }
}

fn recent_row(slug: &str, r: &RecentAttemptView) -> RecentRow {
    let at = &r.attempt;
    RecentRow {
        project: slug.to_owned(),
        attempt_id: at.id,
        href: paths::pipeline_href(slug, at.id),
        title: title(at),
        git_ref: at.git_ref.as_deref().map(|r| ref_name(r).to_owned()),
        commit: at.commit_sha.as_deref().map(short_commit),
        attempt_number: at.attempt,
        jobs: r
            .jobs
            .iter()
            .map(|j| RecentJob {
                name: j.name.clone().unwrap_or_else(|| j.job_key.clone()),
                pill: job_pill(&j.lifecycle, j.outcome.as_deref()),
                provenance: source_provenance(j.outcome_source.as_deref()),
            })
            .collect(),
        progress: Progress::from_counts(&r.flows, at.planned_total),
        started_ms: unix_ms(at.first_seen_at),
        finished_ms: unix_ms(r.finished_at),
    }
}

fn worker_row(slug: &str, w: &WorkerSummaryView) -> WorkerRow {
    WorkerRow {
        project: slug.to_owned(),
        worker_id: w.id,
        href: paths::worker_href(slug, w.id),
        name: w
            .runner_name
            .clone()
            .unwrap_or_else(|| short_commit(&w.id.to_string())),
        platform: platform(w.os.as_deref(), w.arch.as_deref()),
        liveness: liveness_pill(w.liveness),
        active_runs: w.active_runs,
        readings: w
            .latest
            .as_ref()
            .map(|l| host_readings(&l.readings))
            .unwrap_or_default(),
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

/// The readings shown on a runner's row: host scope, in the server's order, so a process or
/// guest figure is never mistaken for the machine's.
fn host_readings(readings: &[MetricReading]) -> Vec<Reading> {
    readings
        .iter()
        .filter(|r| r.scope == MetricScope::Host)
        .filter(|r| {
            matches!(
                r.metric.as_str(),
                CPU_METRIC | "host_mem_used_bytes" | "host_thermal_c"
            )
        })
        .map(|r| Reading {
            label: r.label.clone(),
            text: match (r.available, r.value) {
                (true, Some(v)) => unit(r.unit).format(v),
                _ => "Unavailable".to_owned(),
            },
            available: r.available && r.value.is_some(),
            reason: r.reason.clone(),
        })
        .collect()
}

fn unit(u: MetricUnit) -> Unit {
    match u {
        MetricUnit::Percent => Unit::Percent,
        MetricUnit::Bytes => Unit::Bytes,
        MetricUnit::Celsius => Unit::Celsius,
        MetricUnit::Number => Unit::Number,
    }
}

fn title(a: &AttemptView) -> String {
    a.repository.clone().unwrap_or_else(|| a.run_key.clone())
}

fn count(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use fleet_ui::tone::Tone;
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
                reading("host_thermal_c", "Host temperature", "celsius", "host", &Value::Null),
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
                    "outcome": "failed", "outcome_source": "junit_final",
                    "reopened_count": 0, "rev": 4
                }],
                "flows": {"pending": 0, "running": 0, "passed": 1, "failed": 1, "skipped": 0,
                          "interrupted": 0, "unknown": 0}
            }],
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

    fn model() -> OverviewModel {
        OverviewModel::build(&[ProjectOverview {
            project: project(),
            overview: overview(),
        }])
    }

    #[test]
    fn the_summary_adds_up_the_active_attempts_and_the_online_runners() {
        let s = model().summary;
        assert_eq!(s.active_attempts, 2);
        assert_eq!(s.running_jobs, 3);
        assert_eq!(s.running_flows, 3);
        assert_eq!(s.passed_flows, 3);
        assert_eq!(s.failed_flows, 1);
        assert_eq!(
            s.runners,
            RunnerCounts {
                online: 1,
                stale: 1,
                disconnected: 0,
                completed: 0
            },
            "the server's counts, which cover every runner, not only the 50 listed"
        );
        assert_eq!(
            s.average_cpu_pct,
            Some(40.0),
            "only the online runner counts; a stale runner's last value is not today's load"
        );
    }

    #[test]
    fn average_cpu_is_unavailable_when_no_online_runner_reported_it() {
        let mut o = overview();
        o.workers.retain(|w| w.liveness == Some(Liveness::Stale));
        let m = OverviewModel::build(&[ProjectOverview {
            project: project(),
            overview: o,
        }]);
        assert_eq!(m.summary.average_cpu_pct, None);
    }

    #[test]
    fn an_active_row_links_to_its_page_and_marks_unplanned_progress_uncertain() {
        let m = model();
        let planned = &m.active[0];
        assert_eq!(
            planned.href,
            "/p/app/pipelines/00000000-0000-0000-0000-000000000001"
        );
        assert_eq!(planned.title, "acme/app");
        assert_eq!(planned.commit.as_deref(), Some("0123456"));
        assert_eq!(planned.status.label, "Running");
        assert_eq!(planned.progress.text(), "4 of 6 flows finished · 1 running");
        assert_eq!(
            planned.ci_url.as_deref(),
            Some("https://github.com/acme/app/actions/runs/9001")
        );
        assert_eq!(planned.started_ms, 1_767_225_610_000);

        let unplanned = &m.active[1];
        assert!(
            unplanned.progress.is_uncertain(),
            "no planned_total: never guessed"
        );
        assert_eq!(unplanned.ci_url, None);
    }

    #[test]
    fn a_recent_row_carries_each_jobs_outcome_with_its_provenance() {
        let r = &model().recent[0];
        assert_eq!(r.finished_ms, 1_767_225_650_000);
        assert_eq!(r.jobs.len(), 1);
        assert_eq!(r.jobs[0].name, "E2E");
        assert_eq!(
            (r.jobs[0].pill.label, r.jobs[0].pill.tone),
            ("Failed", Tone::Danger)
        );
        assert_eq!(r.jobs[0].provenance, Provenance::Final);
        assert_eq!(r.progress.text(), "2 of 2 flows finished");
    }

    #[test]
    fn a_runner_shows_host_readings_with_units_and_unavailable_ones_say_so() {
        let w = &model().workers[0];
        assert_eq!(
            w.href,
            "/workers/00000000-0000-0000-0000-0000000000c1?project=app"
        );
        assert_eq!(w.name, "runner-1");
        assert_eq!(w.platform.as_deref(), Some("linux/x86_64"));
        assert_eq!(w.liveness.label, "Online");
        assert_eq!(w.active_runs, 2);
        let texts: Vec<(&str, &str, bool)> = w
            .readings
            .iter()
            .map(|r| (r.label.as_str(), r.text.as_str(), r.available))
            .collect();
        assert_eq!(
            texts,
            [
                ("Host CPU", "40%", true),
                ("Host memory used", "4 GiB", true),
                ("Host temperature", "Unavailable", false),
            ],
            "host scope only; thermal is unavailable, never a made-up number (AC5)"
        );
        assert_eq!(
            w.readings[2].reason.as_deref(),
            Some("the agent did not report it")
        );
        assert_eq!(w.readings_at_ms, Some(1_767_225_660_000));
    }

    #[test]
    fn readings_turn_stale_after_the_freshness_threshold() {
        let at = Some(1_000_000);
        assert_eq!(freshness(at, 1_000_000 + 24_999), Provenance::Observed);
        assert_eq!(freshness(at, 1_000_000 + 25_000), Provenance::Stale);
        assert_eq!(freshness(None, 1_000_000), Provenance::Missing);
    }
}
