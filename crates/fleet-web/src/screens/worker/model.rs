// SPDX-License-Identifier: AGPL-3.0-only
//! The worker page's view-model: what `GET …/workers/{id}` says, as text and facts.
//!
//! A value the agent doesn't report is a [`Fact`] with no value ("Not reported"), and a host
//! sample that is too old to trust turns stale instead of passing for current (invariant I4).

use fleet_protocol::rest::{MetricReading, WorkerRunPage, WorkerRunView, WorkerView};
use fleet_ui::{
    charts::{
        format::{Unit, format_duration},
        series::Reading,
    },
    provenance::{Provenance, STALE_AFTER_MS},
};
use uuid::Uuid;

use crate::{
    clock::unix_ms,
    paths::pipeline_href,
    screens::{
        common::{Ctx, Fact},
        device::model::DeviceSessionRow,
        metrics::unit_of,
    },
    status::{Pill, liveness_pill},
};

const MEM_TOTAL: &str = "host_mem_total_bytes";

/// The machine at a glance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerHeader {
    /// The runner's name, or its id when it has none.
    pub title: String,
    /// How it is doing now.
    pub liveness: Pill,
    /// What it is.
    pub facts: Vec<Fact>,
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
        let memory = w
            .latest
            .as_ref()
            .and_then(|l| l.readings.iter().find(|r| r.metric == MEM_TOTAL))
            .filter(|r| r.available)
            .and_then(|r| r.value)
            .map(|bytes| Unit::Bytes.format(bytes));
        Self {
            title,
            liveness: liveness_pill(w.liveness),
            facts: vec![
                Fact::maybe("Operating system", w.os.clone()),
                Fact::maybe("Architecture", w.arch.clone()),
                Fact::maybe("Memory", memory),
                // The agent doesn't report a core count yet (no field in `WorkerSummaryView`).
                Fact::maybe("CPU cores", None),
                Fact::known("First seen", ctx.time(unix_ms(w.first_seen_at))),
                Fact::known("Last seen", ctx.ago(unix_ms(w.last_seen_at))),
            ],
        }
    }
}

/// One metric's latest value: a number with its unit, or why there is none.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadingTile {
    /// The server's stable metric id.
    pub id: String,
    /// The server's label.
    pub label: String,
    /// What it is measured in.
    pub unit: Unit,
    /// The reading.
    pub reading: Reading,
}

impl ReadingTile {
    fn from_reading(r: &MetricReading) -> Self {
        let unit = unit_of(r.unit, r.scope);
        let reading = match (r.available, r.value) {
            (true, Some(v)) => Reading::Value(v),
            _ => Reading::Unavailable {
                reason: r.reason.clone(),
            },
        };
        Self {
            id: r.metric.clone(),
            label: r.label.clone(),
            unit,
            reading,
        }
    }

    /// `37.5%`, `6 GiB`, or `unavailable (reason)`: an unavailable reading shows no number.
    #[must_use]
    pub fn text(&self) -> String {
        self.reading.label(self.unit)
    }
}

/// The newest host sample.
#[derive(Debug, Clone, PartialEq)]
pub struct HostSample {
    /// When it was taken and how long ago.
    pub taken: String,
    /// Observed, or stale once it is older than the freshness threshold.
    pub provenance: Provenance,
    /// Every host metric.
    pub tiles: Vec<ReadingTile>,
}

/// The worker's health now.
#[derive(Debug, Clone, PartialEq)]
pub enum HostHealth {
    /// The server has no host sample from the last hour: that says nothing about the machine.
    NoRecentSample,
    /// The newest sample.
    Sample(HostSample),
}

impl HostHealth {
    /// The health in `view`.
    #[must_use]
    pub fn build(view: &WorkerView, ctx: &Ctx) -> Self {
        let Some(latest) = &view.worker.latest else {
            return Self::NoRecentSample;
        };
        let at = unix_ms(latest.at);
        Self::Sample(HostSample {
            taken: format!("{} ({})", ctx.time(at), ctx.ago(at)),
            provenance: Provenance::Observed.displayed(ctx.age_ms(at), STALE_AFTER_MS),
            tiles: latest
                .readings
                .iter()
                .map(ReadingTile::from_reading)
                .collect(),
        })
    }
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
    /// What state the run is in.
    pub state: String,
    /// Display liveness.
    pub liveness: Pill,
    /// When it started, in the viewer's zone.
    pub started: String,
    /// How long it ran; `None` until it ends.
    pub duration: Option<String>,
    /// Maestro's exit code; `None` until Maestro exits.
    pub exit_code: Option<i32>,
    /// Explicitly recorded gaps in its data (invariant I5), when there are any.
    pub gaps: Option<String>,
    /// The pipeline page.
    pub href: String,
    started_ms: i64,
}

impl RunRow {
    fn build(r: &WorkerRunView, ctx: &Ctx) -> Self {
        let started_ms = unix_ms(r.started_at);
        let state = match r.run.state.as_str() {
            "active" => "Active",
            "detached" => "Detached",
            "finalizing" => "Finalizing",
            "ended" => "Ended",
            "abandoned" => "Abandoned",
            other => other,
        };
        let gaps = match r.run.gaps {
            ..=0 => None,
            1 => Some("1 recorded gap".to_owned()),
            n => Some(format!("{n} recorded gaps")),
        };
        Self {
            id: r.run.id,
            job_key: r.job_key.clone(),
            state: state.to_owned(),
            liveness: liveness_pill(r.run.liveness),
            started: ctx.time(started_ms),
            duration: r
                .ended_at
                .map(|end| format_duration(unix_ms(end).saturating_sub(started_ms))),
            exit_code: r.run.maestro_exit_code,
            gaps,
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
    use fleet_ui::tone::Tone;

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
    fn the_header_names_the_machine_and_says_how_it_is_doing() {
        let h = WorkerHeader::build(&view(WORKER), &ctx("2026-01-01T11:00:00Z"));
        assert_eq!(h.title, "mac-mini-3");
        assert_eq!(
            (h.liveness.label, h.liveness.tone),
            ("Online", Tone::Success)
        );
        let facts: Vec<_> = h.facts.iter().map(|f| (f.label, f.shown())).collect();
        assert_eq!(
            facts,
            [
                ("Operating system", "macos"),
                ("Architecture", "aarch64"),
                ("Memory", "16 GiB"),
                // The agent doesn't report a core count yet: say so rather than guess.
                ("CPU cores", "Not reported"),
                ("First seen", "2026-01-01 08:00:00"),
                ("Last seen", "5 s ago"),
            ]
        );
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
        let os = h
            .facts
            .iter()
            .find(|f| f.label == "Operating system")
            .unwrap();
        let mem = h.facts.iter().find(|f| f.label == "Memory").unwrap();
        assert_eq!((os.value.clone(), mem.value.clone()), (None, None));
        assert_eq!(mem.shown(), "Not reported");
    }

    #[test]
    fn host_health_shows_every_metric_with_its_unit_and_thermal_as_unavailable() {
        let health = HostHealth::build(&view(WORKER), &ctx("2026-01-01T11:00:00Z"));
        let HostHealth::Sample(s) = health else {
            panic!("a recent sample exists");
        };
        let tiles: Vec<_> = s
            .tiles
            .iter()
            .map(|t| (t.label.as_str(), t.text()))
            .collect();
        assert_eq!(
            tiles,
            [
                ("Host CPU", "37.5%".to_owned()),
                ("Host memory used", "6 GiB".to_owned()),
                ("Host memory total", "16 GiB".to_owned()),
                (
                    "Host temperature",
                    "unavailable (the agent does not report this metric yet)".to_owned()
                ),
            ]
        );
        assert_eq!(s.taken, "2026-01-01 10:59:55 (5 s ago)");
        assert_eq!(s.provenance, Provenance::Observed);
    }

    #[test]
    fn an_old_host_sample_is_stale_not_current() {
        // 25 s without an update is stale (fleet_ui::provenance::STALE_AFTER_MS).
        let HostHealth::Sample(s) = HostHealth::build(&view(WORKER), &ctx("2026-01-01T11:00:20Z"))
        else {
            panic!("a sample exists");
        };
        assert_eq!(s.provenance, Provenance::Stale);
        assert_eq!(s.taken, "2026-01-01 10:59:55 (25 s ago)");
    }

    #[test]
    fn no_recent_sample_is_stated_not_shown_as_a_healthy_machine() {
        let mut v = view(WORKER);
        v.worker.latest = None;
        assert_eq!(
            HostHealth::build(&v, &ctx("2026-01-01T11:00:00Z")),
            HostHealth::NoRecentSample
        );
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
    fn a_run_row_says_how_it_went_and_links_to_its_pipeline() {
        let v = worker_with_runs(vec![
            run(2, "2026-01-01T09:00:00Z", Some("2026-01-01T09:04:05Z")),
            run(1, "2026-01-01T10:30:00Z", None),
        ]);
        let list = RunsList::from_view(&v, &ctx("2026-01-01T11:00:00Z"));
        let rows = list.rows();
        // Newest first, whatever order the response came in.
        assert_eq!(rows[0].job_key, "e2e-shard-1");
        assert_eq!(rows[0].started, "2026-01-01 10:30:00");
        assert_eq!(
            rows[0].duration, None,
            "a run that has not ended has no duration"
        );
        assert_eq!(rows[0].exit_code, None);
        assert_eq!(rows[0].liveness.label, "Online");
        assert_eq!(rows[1].duration.as_deref(), Some("4 min 5 s"));
        assert_eq!(rows[1].exit_code, Some(1));
        assert_eq!(rows[1].gaps.as_deref(), Some("2 recorded gaps"));
        assert_eq!(rows[0].gaps, None);
        assert_eq!(
            rows[1].href,
            "/p/shop/pipelines/00000000-0000-0000-0000-0000000000c1"
        );
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
