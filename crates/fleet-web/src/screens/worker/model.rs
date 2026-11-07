// SPDX-License-Identifier: AGPL-3.0-only
//! The worker page's view-model: what `GET …/workers/{id}` says, as text and facts.
//!
//! A value the agent doesn't report is a [`Fact`] with no value ("Not reported"), and a host
//! sample that is too old to trust turns stale instead of passing for current (invariant I4).

use fleet_protocol::rest::{WorkerRunPage, WorkerRunView, WorkerSummaryView, WorkerView};
use fleet_ui::{
    charts::format::{Unit, format_duration},
    mark::Mark,
    provenance::{Provenance, STALE_AFTER_MS},
};
use uuid::Uuid;

use crate::{
    clock::unix_ms,
    format::time_of_day,
    paths::pipeline_href,
    screens::{
        common::{Ctx, Fact},
        device::model::DeviceSessionRow,
        metrics::{Latest, MetricsPanel},
    },
    status::{Pill, liveness_pill},
};

const MEM_TOTAL: &str = "host_mem_total_bytes";
const CPU_CORES: &str = "host_cpu_cores";
const HOST_CPU: &str = "host_cpu_pct";
const HOST_MEM: &str = "host_mem_used_bytes";
const MAESTRO_CPU: &str = "maestro_cpu_pct";
const AGENT_CPU: &str = "agent_cpu_pct";

/// The metrics the Runner page charts, in the order its design shows them.
pub const CHARTED: [&str; 5] = [HOST_CPU, HOST_MEM, MAESTRO_CPU, AGENT_CPU, "emu_cpu_pct"];

/// The newest host sample's value for `metric`, when the agent reported one.
fn latest_value(w: &WorkerSummaryView, metric: &str) -> Option<f64> {
    w.latest
        .as_ref()?
        .readings
        .iter()
        .find(|r| r.metric == metric)
        .filter(|r| r.available)
        .and_then(|r| r.value)
}

/// The machine at a glance: its name, how it is doing, when it was last sampled and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerHeader {
    /// The runner's name, or its id when it has none.
    pub title: String,
    /// How it is doing now.
    pub liveness: Pill,
    /// `sampled 07:31:39, 4 s ago`, or `None` when no sample was taken in the last hour.
    pub sampled: Option<String>,
    /// How fresh the host sample is: observed, or stale once it is old; missing without a sample.
    pub provenance: Provenance,
    /// `macos · aarch64 · 8 cores · 16 GiB`; a fact the agent did not report reads "Not reported".
    pub meta: String,
}

impl WorkerHeader {
    /// The header of `view`, with times in the viewer's zone.
    #[must_use]
    pub fn build(view: &WorkerView, ctx: &Ctx) -> Self {
        let w = &view.worker;
        let title = w.runner_name.clone().unwrap_or_else(|| {
            let id = w.id.simple().to_string();
            format!("Worker {}", id.chars().take(8).collect::<String>())
        });
        let memory = latest_value(w, MEM_TOTAL).map(|bytes| Unit::Bytes.format(bytes));
        let cores = latest_value(w, CPU_CORES).map(|n| format!("{} cores", Unit::Number.format(n)));
        let facts = [
            Fact::maybe("Operating system", w.os.clone()),
            Fact::maybe("Architecture", w.arch.clone()),
            Fact::maybe("CPU cores", cores),
            Fact::maybe("Memory", memory),
        ];
        let at = w.latest.as_ref().map(|l| unix_ms(l.at));
        Self {
            title,
            liveness: liveness_pill(w.liveness),
            sampled: at.map(|at| {
                format!(
                    "sampled {}, {}",
                    time_of_day(at, ctx.utc_offset_minutes),
                    ctx.ago(at)
                )
            }),
            provenance: at.map_or(Provenance::Missing, |at| {
                Provenance::Observed.displayed(ctx.age_ms(at), STALE_AFTER_MS)
            }),
            meta: facts
                .iter()
                .map(|f| f.shown().to_owned())
                .collect::<Vec<_>>()
                .join(" · "),
        }
    }
}

/// A health cell's value: a number with its unit, or the word Missing with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthValue {
    /// The value, in its unit, such as `25%`.
    Value(String),
    /// No value, and why, when the agent or the range says.
    Missing(Option<String>),
}

/// One cell of Health now: a metric, its value, and what it is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCell {
    /// The server's stable metric id.
    pub id: &'static str,
    /// What it is, with its unit in the label.
    pub label: &'static str,
    /// The value, or why it is missing.
    pub value: HealthValue,
    /// What the value is about, such as `of 16 GiB`.
    pub note: Option<String>,
}

/// The host metric `id` of the newest sample, as a health value.
fn host_value(view: &WorkerView, id: &str, unit: Unit) -> HealthValue {
    let Some(latest) = &view.worker.latest else {
        return HealthValue::Missing(Some("no host sample in the last hour".to_owned()));
    };
    match latest.readings.iter().find(|r| r.metric == id) {
        Some(r) if r.available => match r.value {
            Some(v) => HealthValue::Value(unit.format(v)),
            None => HealthValue::Missing(r.reason.clone()),
        },
        Some(r) => HealthValue::Missing(r.reason.clone()),
        None => HealthValue::Missing(Some("the agent does not report this metric".to_owned())),
    }
}

/// Health now: the host's CPU and memory from the newest sample, and while a run is active the
/// Maestro and agent CPU, the newest point of each process chart in the range. Metrics the agent
/// never reports (such as temperature) are not cells at all.
#[must_use]
pub fn health_cells(view: &WorkerView, processes: Option<&MetricsPanel>) -> Vec<HealthCell> {
    let mut cells = vec![
        HealthCell {
            id: HOST_CPU,
            label: "Host CPU (%)",
            value: host_value(view, HOST_CPU, Unit::Percent),
            note: None,
        },
        HealthCell {
            id: HOST_MEM,
            label: "Memory used",
            value: host_value(view, HOST_MEM, Unit::Bytes),
            note: latest_value(&view.worker, MEM_TOTAL)
                .map(|total| format!("of {}", Unit::Bytes.format(total))),
        },
    ];
    if view.worker.active_runs > 0 {
        let process = |id: &str| match processes.and_then(|p| p.newest(id)) {
            Some(Latest::Value(v)) => HealthValue::Value(v.clone()),
            Some(Latest::Missing(reason)) => HealthValue::Missing(reason.clone()),
            None => HealthValue::Missing(Some("no samples in this range".to_owned())),
        };
        cells.push(HealthCell {
            id: MAESTRO_CPU,
            label: "Maestro CPU (% of one core)",
            value: process(MAESTRO_CPU),
            note: Some("while a run is active".to_owned()),
        });
        cells.push(HealthCell {
            id: AGENT_CPU,
            label: "Agent CPU (% of one core)",
            value: process(AGENT_CPU),
            note: Some("Fleet agent".to_owned()),
        });
    }
    cells
}

/// How many runs `WorkerView::recent_runs` carries at most (see its documentation).
const RECENT_RUNS: usize = 10;

/// One agent run on the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    /// The agent run.
    pub id: Uuid,
    /// Its CI job's key.
    pub job_key: String,
    /// The run's verdict, from its exit status: Failed, No failures, Interrupted or Running.
    pub verdict: Pill,
    /// Whether the run is still open, so its verdict may still change.
    pub open: bool,
    /// When it started, in the viewer's zone.
    pub started: String,
    /// How long it ran, or how long it has run so far.
    pub duration: String,
    /// `Ended · exit 1`, or `exit —, still running`, with any recorded gaps.
    pub outcome: String,
    /// The pipeline page.
    pub href: String,
    started_ms: i64,
}

impl RunRow {
    fn build(r: &WorkerRunView, ctx: &Ctx) -> Self {
        let started_ms = unix_ms(r.started_at);
        let exit = r.run.maestro_exit_code;
        let (verdict, open, duration, mut outcome) = match r.ended_at {
            None => (
                Pill {
                    label: "Running",
                    mark: Mark::Running,
                },
                true,
                format!(
                    "Running for {}",
                    format_duration(ctx.now_ms.saturating_sub(started_ms).max(0))
                ),
                "exit —, still running".to_owned(),
            ),
            Some(end) => {
                let took = format_duration(unix_ms(end).saturating_sub(started_ms));
                let (verdict, exit_text) = match exit {
                    Some(0) => (
                        Pill {
                            label: "No failures",
                            mark: Mark::End,
                        },
                        "0".to_owned(),
                    ),
                    Some(code) => (
                        Pill {
                            label: "Failed",
                            mark: Mark::Failed,
                        },
                        code.to_string(),
                    ),
                    None => (
                        Pill {
                            label: "Interrupted",
                            mark: Mark::Interrupted,
                        },
                        "—".to_owned(),
                    ),
                };
                (verdict, false, took, format!("Ended · exit {exit_text}"))
            }
        };
        match r.run.gaps {
            ..=0 => {}
            1 => outcome.push_str(" · 1 recorded gap"),
            n => outcome = format!("{outcome} · {n} recorded gaps"),
        }
        Self {
            id: r.run.id,
            job_key: r.job_key.clone(),
            verdict,
            open,
            started: ctx.time(started_ms),
            duration,
            outcome,
            href: pipeline_href(&ctx.project, r.pipeline_attempt_id),
            started_ms,
        }
    }
}

/// The request that would load more runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextPage {
    /// Every run is shown.
    Done,
    /// Ask for the first page of `/runs` (the view's list has no cursor).
    First,
    /// Ask for the page after this cursor.
    After(String),
}

/// The worker's runs, newest first, growing page by page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunsList {
    rows: Vec<RunRow>,
    next: NextPage,
}

impl RunsList {
    /// The runs in the worker view. A full list may have older runs behind it.
    #[must_use]
    pub fn from_view(view: &WorkerView, ctx: &Ctx) -> Self {
        let mut list = Self {
            rows: Vec::new(),
            next: if view.recent_runs.len() >= RECENT_RUNS {
                NextPage::First
            } else {
                NextPage::Done
            },
        };
        list.merge(&view.recent_runs, ctx);
        list
    }

    /// Adds a page of `/runs`: runs already shown are kept once, the order stays newest first
    /// and the cursor moves on.
    pub fn apply_page(&mut self, page: &WorkerRunPage, ctx: &Ctx) {
        self.merge(&page.items, ctx);
        self.next = page
            .next_cursor
            .clone()
            .map_or(NextPage::Done, NextPage::After);
    }

    fn merge(&mut self, runs: &[WorkerRunView], ctx: &Ctx) {
        // A run seen again takes its newer data.
        self.rows
            .retain(|row| !runs.iter().any(|r| r.run.id == row.id));
        for r in runs {
            if !self.rows.iter().any(|row| row.id == r.run.id) {
                self.rows.push(RunRow::build(r, ctx));
            }
        }
        self.rows
            .sort_by_key(|row| std::cmp::Reverse(row.started_ms));
    }

    /// The runs, newest first.
    #[must_use]
    pub fn rows(&self) -> &[RunRow] {
        &self.rows
    }

    /// What "load more" should ask for.
    #[must_use]
    pub fn next_page(&self) -> NextPage {
        self.next.clone()
    }
}

/// The devices the worker hosted, newest first.
#[must_use]
pub fn device_rows(view: &WorkerView, ctx: &Ctx) -> Vec<DeviceSessionRow> {
    view.devices
        .iter()
        .map(|d| DeviceSessionRow::build(d, ctx))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKER: &str = r#"{
      "worker": {
        "id": "00000000-0000-0000-0000-0000000000a1",
        "runner_name": "mac-mini-3", "os": "macos", "arch": "aarch64",
        "first_seen_at": "2026-01-01T08:00:00Z", "last_seen_at": "2026-01-01T10:59:55Z",
        "liveness": "online", "active_runs": 1,
        "latest": {
          "at": "2026-01-01T10:59:55Z",
          "readings": [
            {"metric": "host_cpu_pct", "label": "Host CPU", "unit": "percent", "scope": "host",
             "value": 37.5, "available": true},
            {"metric": "host_mem_used_bytes", "label": "Host memory used", "unit": "bytes",
             "scope": "host", "value": 6442450944.0, "available": true},
            {"metric": "host_mem_total_bytes", "label": "Host memory total", "unit": "bytes",
             "scope": "host", "value": 17179869184.0, "available": true},
            {"metric": "host_cpu_cores", "label": "Host CPU cores", "unit": "number",
             "scope": "host", "value": 8.0, "available": true},
            {"metric": "host_load_1m", "label": "Host load (1 min)", "unit": "number",
             "scope": "host", "value": 1.5, "available": true},
            {"metric": "host_swap_used_bytes", "label": "Host swap used", "unit": "bytes",
             "scope": "host", "value": 1073741824.0, "available": true},
            {"metric": "host_psi_cpu_pct", "label": "Host CPU pressure", "unit": "percent",
             "scope": "host", "value": 0.25, "available": true},
            {"metric": "host_thermal_c", "label": "Host temperature", "unit": "celsius",
             "scope": "host", "value": null, "available": false,
             "reason": "the agent does not report this metric yet"}
          ]
        }
      },
      "recent_runs": [], "devices": []
    }"#;

    fn ctx(now: &str) -> Ctx {
        let now = time::OffsetDateTime::parse(now, &time::format_description::well_known::Rfc3339)
            .unwrap();
        Ctx {
            project: "shop".to_owned(),
            now_ms: crate::clock::unix_ms(now),
            utc_offset_minutes: 0,
        }
    }

    fn view(json: &str) -> WorkerView {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn the_header_names_the_machine_and_says_when_it_was_sampled() {
        let h = WorkerHeader::build(&view(WORKER), &ctx("2026-01-01T11:00:00Z"));
        assert_eq!(h.title, "mac-mini-3");
        assert_eq!(
            (h.liveness.label, h.liveness.mark),
            ("Online", Mark::Passed)
        );
        assert_eq!(h.sampled.as_deref(), Some("sampled 10:59:55, 5 s ago"));
        assert_eq!(h.provenance, Provenance::Observed);
        assert_eq!(h.meta, "macos · aarch64 · 8 cores · 16 GiB");
    }

    #[test]
    fn an_unnamed_worker_is_identified_by_its_id_and_missing_facts_are_not_reported() {
        let mut v = view(WORKER);
        v.worker.runner_name = None;
        v.worker.os = None;
        v.worker.arch = None;
        v.worker.liveness = None;
        v.worker.latest = None;
        let h = WorkerHeader::build(&v, &ctx("2026-01-01T11:00:00Z"));
        assert_eq!(h.title, "Worker 00000000");
        assert_eq!(h.liveness.label, "Unknown");
        assert_eq!(h.sampled, None, "no sample, no sampled line");
        assert_eq!(h.provenance, Provenance::Missing);
        assert_eq!(
            h.meta,
            "Not reported · Not reported · Not reported · Not reported"
        );
    }

    #[test]
    fn health_now_is_the_host_cpu_and_memory_and_thermal_is_not_a_cell() {
        let mut v = view(WORKER);
        v.worker.active_runs = 0;
        let cells: Vec<_> = health_cells(&v, None)
            .into_iter()
            .map(|c| (c.id, c.label, c.value, c.note))
            .collect();
        assert_eq!(
            cells,
            [
                (
                    "host_cpu_pct",
                    "Host CPU (%)",
                    HealthValue::Value("37.5%".to_owned()),
                    None
                ),
                (
                    "host_mem_used_bytes",
                    "Memory used",
                    HealthValue::Value("6 GiB".to_owned()),
                    Some("of 16 GiB".to_owned())
                ),
            ],
            "no run is active, so no process cells; the thermal sensor the agent never reports is absent"
        );
    }

    #[test]
    fn process_cells_show_only_while_a_run_is_active_and_take_the_newest_point() {
        let panel = MetricsPanel::build(
            &serde_json::from_str(
                r#"{"subject": {"kind": "worker", "id": "00000000-0000-0000-0000-0000000000a1"},
                "from": "2026-01-01T10:00:00Z", "to": "2026-01-01T11:00:00Z",
                "resolution": "raw", "truncated": false,
                "series": [{"metric": "maestro_cpu_pct", "label": "Maestro CPU", "unit": "percent",
                  "scope": "maestro", "available": true,
                  "points": [{"at_ms": 1767261600000, "value": 12.0, "available": true},
                             {"at_ms": 1767261660000, "value": 180.0, "available": true}]}]}"#,
            )
            .unwrap(),
        );
        let cells = health_cells(&view(WORKER), Some(&panel));
        let maestro = cells.iter().find(|c| c.id == "maestro_cpu_pct").unwrap();
        assert_eq!(
            maestro.value,
            HealthValue::Value(Unit::PercentOfCore.format(180.0))
        );
        assert_eq!(maestro.label, "Maestro CPU (% of one core)");
        // The agent has no series in this range: missing, with the reason, not a guess.
        let agent = cells.iter().find(|c| c.id == "agent_cpu_pct").unwrap();
        assert_eq!(
            agent.value,
            HealthValue::Missing(Some("no samples in this range".to_owned()))
        );
    }

    #[test]
    fn a_host_reading_the_agent_did_not_send_is_missing_with_its_reason() {
        let mut v = view(WORKER);
        v.worker.latest.as_mut().unwrap().readings[0].available = false;
        v.worker.latest.as_mut().unwrap().readings[0].value = None;
        v.worker.latest.as_mut().unwrap().readings[0].reason = Some("sensor busy".to_owned());
        let cpu = health_cells(&v, None)
            .into_iter()
            .find(|c| c.id == "host_cpu_pct")
            .unwrap();
        assert_eq!(
            cpu.value,
            HealthValue::Missing(Some("sensor busy".to_owned()))
        );
        // No sample at all says so, rather than showing a healthy machine.
        v.worker.latest = None;
        let cpu = health_cells(&v, None)
            .into_iter()
            .find(|c| c.id == "host_cpu_pct")
            .unwrap();
        assert_eq!(
            cpu.value,
            HealthValue::Missing(Some("no host sample in the last hour".to_owned()))
        );
    }

    #[test]
    fn an_old_host_sample_is_stale_not_current() {
        // 25 s without an update is stale (fleet_ui::provenance::STALE_AFTER_MS).
        let h = WorkerHeader::build(&view(WORKER), &ctx("2026-01-01T11:00:20Z"));
        assert_eq!(h.provenance, Provenance::Stale);
        assert_eq!(h.sampled.as_deref(), Some("sampled 10:59:55, 25 s ago"));
    }

    fn run(id: u8, started: &str, ended: Option<&str>) -> serde_json::Value {
        serde_json::json!({
          "run": {
            "id": format!("00000000-0000-0000-0000-0000000001{id:02}"),
            "job_id": "00000000-0000-0000-0000-0000000000b1",
            "worker_id": "00000000-0000-0000-0000-0000000000a1",
            "runner_name": "mac-mini-3", "state": if ended.is_some() { "ended" } else { "active" },
            "trust_level": "trusted", "committed_seq": 40, "gaps": if id == 2 { 2 } else { 0 },
            "last_frame_at": "2026-01-01T10:59:55Z", "late_resume": false,
            "plan_id": null, "plan_shard_id": null,
            "maestro_exit_code": if ended.is_some() { serde_json::json!(1) } else { serde_json::Value::Null },
            "rev": 3, "liveness": if ended.is_some() { "completed" } else { "online" }
          },
          "pipeline_attempt_id": "00000000-0000-0000-0000-0000000000c1",
          "job_key": format!("e2e-shard-{id}"),
          "started_at": started,
          "ended_at": ended
        })
    }

    fn worker_with_runs(runs: Vec<serde_json::Value>) -> WorkerView {
        let mut v: serde_json::Value = serde_json::from_str(WORKER).unwrap();
        v["recent_runs"] = runs.into();
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_run_row_takes_its_verdict_from_the_exit_status_and_links_to_its_pipeline() {
        let v = worker_with_runs(vec![
            run(2, "2026-01-01T09:00:00Z", Some("2026-01-01T09:04:05Z")),
            run(1, "2026-01-01T10:30:00Z", None),
        ]);
        let list = RunsList::from_view(&v, &ctx("2026-01-01T11:00:00Z"));
        let rows = list.rows();
        // Newest first, whatever order the response came in.
        assert_eq!(rows[0].job_key, "e2e-shard-1");
        assert_eq!(rows[0].started, "2026-01-01 10:30:00");
        assert_eq!((rows[0].verdict.label, rows[0].open), ("Running", true));
        assert!(rows[0].duration.starts_with("Running for"));
        assert_eq!(rows[0].outcome, "exit —, still running");
        assert_eq!(rows[1].duration, "4 min 5 s");
        assert_eq!((rows[1].verdict.label, rows[1].open), ("Failed", false));
        assert_eq!(rows[1].outcome, "Ended · exit 1 · 2 recorded gaps");
        assert_eq!(
            rows[1].href,
            "/p/shop/pipelines/00000000-0000-0000-0000-0000000000c1"
        );
    }

    #[test]
    fn a_run_that_ended_clean_is_no_failures_and_one_without_an_exit_is_interrupted() {
        let mut clean = run(3, "2026-01-01T09:00:00Z", Some("2026-01-01T09:04:05Z"));
        clean["run"]["maestro_exit_code"] = serde_json::json!(0);
        let mut cut_short = run(4, "2026-01-01T10:00:00Z", Some("2026-01-01T10:01:00Z"));
        cut_short["run"]["maestro_exit_code"] = serde_json::Value::Null;
        let v = worker_with_runs(vec![clean, cut_short]);
        let list = RunsList::from_view(&v, &ctx("2026-01-01T11:00:00Z"));
        let rows = list.rows();
        assert_eq!(
            (rows[0].verdict.label, rows[0].verdict.mark),
            ("Interrupted", Mark::Interrupted)
        );
        assert_eq!(rows[0].outcome, "Ended · exit —");
        assert_eq!(
            (rows[1].verdict.label, rows[1].verdict.mark),
            ("No failures", Mark::End)
        );
        assert_eq!(rows[1].outcome, "Ended · exit 0");
    }

    #[test]
    fn a_short_list_is_complete_but_a_full_first_page_may_have_more() {
        let ctx = ctx("2026-01-01T11:00:00Z");
        let few = worker_with_runs(vec![run(1, "2026-01-01T10:30:00Z", None)]);
        assert_eq!(RunsList::from_view(&few, &ctx).next_page(), NextPage::Done);
        // The view carries the 10 latest runs and no cursor: "more" means page 1 of `/runs`.
        let ten = worker_with_runs(
            (0..10)
                .map(|i| run(i, &format!("2026-01-01T0{}:00:00Z", i % 9), None))
                .collect(),
        );
        assert_eq!(RunsList::from_view(&ten, &ctx).next_page(), NextPage::First);
    }

    #[test]
    fn paging_merges_without_duplicates_and_follows_the_cursor_to_the_end() {
        let ctx = ctx("2026-01-01T11:00:00Z");
        let first: Vec<_> = (0..10)
            .map(|i| run(i, &format!("2026-01-01T{:02}:00:00Z", 10 - i), None))
            .collect();
        let mut list = RunsList::from_view(&worker_with_runs(first.clone()), &ctx);
        // Page 1 of `/runs` repeats the 10 and adds 2 older, and says more follow.
        let mut page = first;
        page.push(run(20, "2026-01-01T00:00:00Z", None));
        page.push(run(21, "2025-12-31T23:00:00Z", None));
        let page = serde_json::json!({ "items": page, "next_cursor": "c-2" });
        list.apply_page(&serde_json::from_value(page).unwrap(), &ctx);
        assert_eq!(list.rows().len(), 12);
        assert_eq!(list.rows()[11].job_key, "e2e-shard-21");
        assert_eq!(list.next_page(), NextPage::After("c-2".to_owned()));
        // The last page has no cursor.
        let final_page = serde_json::json!({ "items": [run(30, "2025-12-31T22:00:00Z", None)], "next_cursor": null });
        list.apply_page(&serde_json::from_value(final_page).unwrap(), &ctx);
        assert_eq!(list.rows().len(), 13);
        assert_eq!(list.next_page(), NextPage::Done);
    }

    #[test]
    fn the_devices_a_worker_hosted_link_to_their_pages_and_say_what_they_are() {
        let mut v: serde_json::Value = serde_json::from_str(WORKER).unwrap();
        v["devices"] = serde_json::json!([
          {"device": {"id": "00000000-0000-0000-0000-0000000000d1",
                      "agent_run_id": "00000000-0000-0000-0000-000000000101",
                      "serial": "emulator-5554", "state": "ready", "rev": 4},
           "worker_id": "00000000-0000-0000-0000-0000000000a1", "runner_name": "mac-mini-3",
           "pipeline_attempt_id": "00000000-0000-0000-0000-0000000000c1",
           "job_id": "00000000-0000-0000-0000-0000000000b1", "job_key": "e2e-shard-1",
           "first_seen_at": "2026-01-01T10:00:00Z", "last_seen_at": "2026-01-01T10:59:55Z",
           "profile": {"platform": "android", "kind": "emulator", "model": "sdk_gphone64_arm64",
                       "avd_or_runtime": "Pixel_7_API_34", "os_version": "14", "api_level": 34,
                       "abi": "arm64-v8a"}},
          {"device": {"id": "00000000-0000-0000-0000-0000000000d2",
                      "agent_run_id": "00000000-0000-0000-0000-000000000101",
                      "serial": "emulator-5556", "state": "booting", "rev": 1},
           "worker_id": "00000000-0000-0000-0000-0000000000a1", "runner_name": "mac-mini-3",
           "pipeline_attempt_id": "00000000-0000-0000-0000-0000000000c1",
           "job_id": "00000000-0000-0000-0000-0000000000b1", "job_key": "e2e-shard-1",
           "first_seen_at": "2026-01-01T10:00:00Z", "last_seen_at": "2026-01-01T10:59:55Z",
           "profile": null}
        ]);
        let v: WorkerView = serde_json::from_value(v).unwrap();
        let rows = device_rows(&v, &ctx("2026-01-01T11:00:00Z"));
        assert_eq!(rows[0].serial, "emulator-5554");
        assert_eq!(
            rows[0].profile.as_deref(),
            Some("Pixel_7_API_34 · Android 14 (API 34) · arm64-v8a")
        );
        assert_eq!(rows[0].state.label, "Ready");
        assert_eq!(rows[0].last_seen, "5 s ago");
        assert_eq!(
            rows[0].href,
            "/devices/00000000-0000-0000-0000-0000000000d1?project=shop"
        );
        // No profile reported yet: no profile text, not a guess.
        assert_eq!(rows[1].profile, None);
        assert_eq!(rows[1].state.label, "Booting");
    }
}
