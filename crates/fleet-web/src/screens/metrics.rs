// SPDX-License-Identifier: AGPL-3.0-only
//! Screen C's metric charts and range picker, shared by the worker and the device page.
//!
//! The server decides what a metric is called, what it is measured in and what it is about
//! (`fleet_protocol::rest::MetricsView`); this module only lays that out. A reading that is not
//! there stays "unavailable" with its reason from the response to the screen: it is never drawn
//! as a number, a zero or an interpolation (acceptance criteria 4 and 5).

use crate::clock::unix_ms;
use crate::format::{short_commit, time_of_day};
use fleet_protocol::rest::{
    MetricAggregate, MetricPoint, MetricResolution, MetricScope, MetricUnit, MetricsView,
    SeriesView,
};
use fleet_ui::{
    charts::{
        format::Unit,
        series::{Metric, Point, Reading, Series},
    },
    provenance::Provenance,
};

/// What one chart shows.
#[derive(Debug, Clone, PartialEq)]
pub enum ChartBody {
    /// Readings to draw; stretches without a value are gaps.
    Series(Series),
    /// No value in the whole range: say so, with the reason, instead of drawing anything.
    Unavailable {
        /// Why there is no value, when the server says.
        reason: Option<String>,
    },
}

/// The newest reading of a chart, shown in its header.
#[derive(Debug, Clone, PartialEq)]
pub enum Latest {
    /// The newest reading's value in the chart's unit, such as `38%`.
    Value(String),
    /// The newest point has no value; why, when the server says.
    Missing(Option<String>),
}

/// One metric's chart.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricChart {
    /// The server's stable metric id, such as `host_cpu_pct`.
    pub id: String,
    /// The server's label, such as `Host CPU`.
    pub title: String,
    /// What it is measured in, spelled out.
    pub unit_symbol: &'static str,
    /// What the figures are when they are not raw samples: `Average per bucket`.
    pub aggregate_note: Option<&'static str>,
    /// What is drawn.
    pub body: ChartBody,
    /// The newest point, for the chart's header; `None` when the range has no points at all.
    pub latest: Option<Latest>,
}

/// The charts about one kind of subject. Host, process and guest figures are never mixed.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricSection {
    /// What the figures are about.
    pub scope: MetricScope,
    /// Heading naming the scope.
    pub heading: &'static str,
    /// The charts, in the response's order.
    pub charts: Vec<MetricChart>,
}

/// Screen C's correlated charts for one worker or device.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricsPanel {
    /// Charts by scope, host first.
    pub sections: Vec<MetricSection>,
    /// The requested range (unix ms), the x-axis of every chart, so they line up and share a
    /// cursor with the timeline.
    pub domain_ms: (i64, i64),
    /// Where the points come from, in words: `Raw samples`, `1-minute averages` or `1-hour averages`.
    pub resolution_note: &'static str,
    /// How the values are known: observed when raw, derived from aggregates otherwise.
    pub provenance: Provenance,
    /// Set when more samples matched than one response carries, so the charts stop short.
    pub truncated_note: Option<&'static str>,
}

const SCOPES: [(MetricScope, &str); 6] = [
    (MetricScope::Host, "Host (the machine)"),
    (MetricScope::Runner, "Maestro process"),
    (MetricScope::Agent, "Fleet agent"),
    (
        MetricScope::Emulator,
        "Emulator process (as the host sees it)",
    ),
    (MetricScope::Guest, "Inside the device (guest)"),
    (MetricScope::Device, "Device (adb and sensors)"),
];

/// The kit's unit for a metric. CPU of a process is a percentage of one core, so it can exceed
/// 100; the host's is of the whole machine (`MetricUnit::Percent` in `fleet_protocol::rest`).
pub(crate) fn unit_of(unit: MetricUnit, scope: MetricScope) -> Unit {
    match (unit, scope) {
        (MetricUnit::Bytes, _) => Unit::Bytes,
        (MetricUnit::Celsius, _) => Unit::Celsius,
        (MetricUnit::Number, _) => Unit::Number,
        (MetricUnit::Percent, MetricScope::Runner | MetricScope::Agent | MetricScope::Emulator) => {
            Unit::PercentOfCore
        }
        (MetricUnit::Percent, MetricScope::Host | MetricScope::Guest | MetricScope::Device) => {
            Unit::Percent
        }
    }
}

const fn unit_symbol(unit: Unit) -> &'static str {
    match unit {
        Unit::Bytes => "bytes",
        Unit::Percent => "%",
        Unit::PercentOfCore => "% of one core",
        Unit::Celsius => "°C",
        Unit::Millis => "ms",
        Unit::Number => "",
    }
}

fn reading(p: &MetricPoint) -> Reading {
    match (p.available, p.value) {
        (true, Some(v)) => Reading::Value(v),
        _ => Reading::Unavailable {
            reason: p.reason.clone(),
        },
    }
}

fn latest_of(points: &[Point], unit: Unit) -> Option<Latest> {
    points.last().map(|p| match &p.reading {
        Reading::Value(v) => Latest::Value(unit.format(*v)),
        Reading::Unavailable { reason } => Latest::Missing(reason.clone()),
    })
}

fn chart(s: &SeriesView) -> MetricChart {
    let unit = unit_of(s.unit, s.scope);
    // A process series is one of several runs' on the worker: the run tells the lines apart.
    let label = match s.run_id {
        Some(run) => format!("{} · run {}", s.label, short_commit(&run.to_string())),
        None => s.label.clone(),
    };
    let points: Vec<Point> = s
        .points
        .iter()
        .map(|p| Point {
            at_ms: p.at_ms,
            reading: reading(p),
        })
        .collect();
    let has_value = points
        .iter()
        .any(|p| matches!(p.reading, Reading::Value(_)));
    let latest = latest_of(&points, unit);
    let body = if s.available && has_value {
        ChartBody::Series(Series::new(
            Metric {
                name: label.clone(),
                unit,
            },
            points,
        ))
    } else {
        let reason = s
            .reason
            .clone()
            .or_else(|| s.points.iter().find_map(|p| p.reason.clone()));
        ChartBody::Unavailable { reason }
    };
    MetricChart {
        id: s.metric.clone(),
        title: label,
        unit_symbol: unit_symbol(unit),
        aggregate_note: match s.aggregate {
            Some(MetricAggregate::Avg) => Some("Average per bucket"),
            Some(MetricAggregate::Max) => Some("Maximum per bucket"),
            None => None,
        },
        body,
        latest,
    }
}

impl MetricsPanel {
    /// Keeps only the charts named by `ids`, in their order; a section left without charts goes.
    /// A page shows the metrics its design names, so the others are not drawn at all.
    #[must_use]
    pub fn only(mut self, ids: &[&str]) -> Self {
        for section in &mut self.sections {
            section.charts.retain(|c| ids.contains(&c.id.as_str()));
        }
        self.sections.retain(|s| !s.charts.is_empty());
        self
    }

    /// The newest reading of the first chart named `id` that has one, such as `runner_cpu_pct`.
    #[must_use]
    pub fn newest(&self, id: &str) -> Option<&Latest> {
        self.sections
            .iter()
            .flat_map(|s| &s.charts)
            .filter(|c| c.id == id)
            .find_map(|c| c.latest.as_ref())
    }

    /// The window as the page names it, such as `06:31 to 07:31` in the viewer's zone.
    #[must_use]
    pub fn window_label(&self, utc_offset_minutes: i32) -> String {
        format!(
            "{} to {}",
            time_of_day(self.domain_ms.0, utc_offset_minutes),
            time_of_day(self.domain_ms.1, utc_offset_minutes)
        )
    }

    /// Lays out a metrics response.
    #[must_use]
    pub fn build(view: &MetricsView) -> Self {
        let sections = SCOPES
            .iter()
            .filter_map(|&(scope, heading)| {
                let charts: Vec<MetricChart> = view
                    .series
                    .iter()
                    .filter(|s| s.scope == scope)
                    .map(chart)
                    .collect();
                (!charts.is_empty()).then_some(MetricSection {
                    scope,
                    heading,
                    charts,
                })
            })
            .collect();
        Self {
            sections,
            domain_ms: (unix_ms(view.from), unix_ms(view.to)),
            resolution_note: match view.resolution {
                MetricResolution::Raw => "Raw samples",
                MetricResolution::OneMinute => "1-minute averages",
                MetricResolution::OneHour => "1-hour averages",
            },
            provenance: match view.resolution {
                MetricResolution::Raw => Provenance::Observed,
                MetricResolution::OneMinute | MetricResolution::OneHour => Provenance::Derived,
            },
            truncated_note: view.truncated.then_some(
                "More samples matched than one response carries: the charts end before the end of the range.",
            ),
        }
    }
}

/// The most points one response may carry, and the fewest it may be asked for.
const POINTS: std::ops::RangeInclusive<usize> = 4..=5000;

/// A range the picker offers, counted back from now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangePreset {
    /// The last 15 minutes.
    Last15Minutes,
    /// The last hour (the server's default).
    LastHour,
    /// The last 6 hours.
    Last6Hours,
    /// The last 24 hours: the longest range with raw samples.
    Last24Hours,
    /// The last 7 days.
    Last7Days,
}

impl RangePreset {
    /// Every preset, shortest first.
    pub const ALL: [Self; 5] = [
        Self::Last15Minutes,
        Self::LastHour,
        Self::Last6Hours,
        Self::Last24Hours,
        Self::Last7Days,
    ];

    /// The picker's button text.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Last15Minutes => "15 minutes",
            Self::LastHour => "1 hour",
            Self::Last6Hours => "6 hours",
            Self::Last24Hours => "24 hours",
            Self::Last7Days => "7 days",
        }
    }

    const fn length(self) -> time::Duration {
        match self {
            Self::Last15Minutes => time::Duration::minutes(15),
            Self::LastHour => time::Duration::hours(1),
            Self::Last6Hours => time::Duration::hours(6),
            Self::Last24Hours => time::Duration::hours(24),
            Self::Last7Days => time::Duration::days(7),
        }
    }
}

/// What the user chose in the range picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSelection {
    /// A stretch ending now.
    Preset(RangePreset),
    /// Exactly this range.
    Custom {
        /// Start, inclusive.
        from: time::OffsetDateTime,
        /// End, exclusive.
        to: time::OffsetDateTime,
    },
}

/// Why a range can't be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RangeError {
    /// The end is not after the start.
    #[error("the range must end after it starts")]
    NotForward,
}

/// The query of `GET …/metrics`. Whether the answer is raw samples or one-minute or one-hour
/// aggregates is the server's choice; the page only says from, to and how many points fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsQuery {
    /// Start, inclusive.
    pub from: time::OffsetDateTime,
    /// End, exclusive.
    pub to: time::OffsetDateTime,
    /// At most this many points per series.
    pub points: usize,
}

impl RangeSelection {
    /// The request for this selection at `now`, asking for at most `points` points (kept
    /// within what the server accepts).
    pub fn query(
        self,
        now: time::OffsetDateTime,
        points: usize,
    ) -> Result<MetricsQuery, RangeError> {
        let (from, to) = match self {
            Self::Preset(p) => (now - p.length(), now),
            Self::Custom { from, to } => (from, to),
        };
        if from >= to {
            return Err(RangeError::NotForward);
        }
        Ok(MetricsQuery {
            from,
            to,
            points: points.clamp(*POINTS.start(), *POINTS.end()),
        })
    }
}

impl MetricsQuery {
    /// `from=…&to=…&points=…`, times in UTC to the second.
    #[must_use]
    pub fn to_query_string(&self) -> String {
        format!(
            "from={}&to={}&points={}",
            rfc3339_utc(self.from),
            rfc3339_utc(self.to),
            self.points
        )
    }
}

fn rfc3339_utc(t: time::OffsetDateTime) -> String {
    let t = t
        .to_offset(time::UtcOffset::UTC)
        .replace_nanosecond(0)
        .unwrap_or(t);
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// How many points the charts ask for: about one per two pixels of a 640 px chart.
pub const CHART_POINTS: usize = 300;

/// The query string for `selection` at `now_ms` (unix ms), asking for [`CHART_POINTS`] points.
pub fn query_string_at(selection: RangeSelection, now_ms: i64) -> Result<String, RangeError> {
    let now = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(now_ms) * 1_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    selection
        .query(now, CHART_POINTS)
        .map(|q| q.to_query_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A worker's metrics as `GET …/workers/{id}/metrics` returns them: host CPU with a gap,
    /// Maestro's CPU, memory in bytes, and the thermal sensor the agent doesn't report.
    const WORKER_METRICS: &str = r#"{
      "subject": {"kind": "worker", "id": "00000000-0000-0000-0000-0000000000a1"},
      "from": "2026-01-01T10:00:00Z", "to": "2026-01-01T11:00:00Z",
      "resolution": "raw", "truncated": false,
      "series": [
        {"metric": "host_thermal_c", "label": "Host temperature", "unit": "celsius",
         "scope": "host", "available": false,
         "reason": "the agent does not report this metric yet",
         "points": [{"at_ms": 1767261600000, "value": null, "available": false,
                     "reason": "the agent does not report this metric yet"}]},
        {"metric": "runner_cpu_pct", "label": "Maestro CPU", "unit": "percent",
         "scope": "runner", "available": true,
         "points": [{"at_ms": 1767261600000, "value": 180.0, "available": true}]},
        {"metric": "host_cpu_pct", "label": "Host CPU", "unit": "percent",
         "scope": "host", "available": true,
         "points": [
           {"at_ms": 1767261600000, "value": 12.5, "available": true},
           {"at_ms": 1767261605000, "value": null, "available": false,
            "reason": "the agent reported no value"},
           {"at_ms": 1767261610000, "value": 40.0, "available": true}]},
        {"metric": "host_mem_used_bytes", "label": "Host memory used", "unit": "bytes",
         "scope": "host", "available": true,
         "points": [{"at_ms": 1767261600000, "value": 8589934592.0, "available": true}]}
      ]
    }"#;

    fn panel(json: &str) -> MetricsPanel {
        let view: MetricsView = serde_json::from_str(json).unwrap();
        MetricsPanel::build(&view)
    }

    fn chart<'a>(panel: &'a MetricsPanel, id: &str) -> &'a MetricChart {
        panel
            .sections
            .iter()
            .flat_map(|s| &s.charts)
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no chart {id}"))
    }

    #[test]
    fn charts_carry_the_servers_label_and_the_right_unit() {
        let p = panel(WORKER_METRICS);
        let ChartBody::Series(cpu) = &chart(&p, "host_cpu_pct").body else {
            panic!("host CPU has readings");
        };
        assert_eq!(cpu.metric.name, "Host CPU");
        assert_eq!(cpu.metric.unit, Unit::Percent);
        let ChartBody::Series(mem) = &chart(&p, "host_mem_used_bytes").body else {
            panic!("memory has readings");
        };
        assert_eq!(mem.metric.unit, Unit::Bytes);
        // 8 GiB reads as 8 GiB, not as 8589934592 of something.
        assert_eq!(mem.points()[0].reading.label(mem.metric.unit), "8 GiB");
    }

    #[test]
    fn a_process_cpu_is_a_percentage_of_one_core_but_the_host_is_of_the_whole_machine() {
        let p = panel(WORKER_METRICS);
        let unit = |id| match &chart(&p, id).body {
            ChartBody::Series(s) => s.metric.unit,
            ChartBody::Unavailable { .. } => panic!("{id} has readings"),
        };
        assert_eq!(unit("runner_cpu_pct"), Unit::PercentOfCore);
        assert_eq!(unit("host_cpu_pct"), Unit::Percent);
        assert_eq!(chart(&p, "runner_cpu_pct").unit_symbol, "% of one core");
        assert_eq!(chart(&p, "host_cpu_pct").unit_symbol, "%");
    }

    #[test]
    fn the_thermal_sensor_that_is_not_reported_is_unavailable_with_its_reason_and_no_number() {
        let p = panel(WORKER_METRICS);
        let thermal = chart(&p, "host_thermal_c");
        assert_eq!(thermal.title, "Host temperature");
        assert_eq!(thermal.unit_symbol, "°C");
        let ChartBody::Unavailable { reason } = &thermal.body else {
            panic!("an unreported sensor draws no series");
        };
        assert_eq!(
            reason.as_deref(),
            Some("the agent does not report this metric yet")
        );
    }

    #[test]
    fn a_missing_reading_in_the_middle_stays_unavailable_with_its_reason() {
        let p = panel(WORKER_METRICS);
        let ChartBody::Series(cpu) = &chart(&p, "host_cpu_pct").body else {
            panic!("host CPU has readings");
        };
        let readings: Vec<&Reading> = cpu.points().iter().map(|p: &Point| &p.reading).collect();
        assert_eq!(
            readings,
            [
                &Reading::Value(12.5),
                &Reading::Unavailable {
                    reason: Some("the agent reported no value".to_owned())
                },
                &Reading::Value(40.0),
            ]
        );
    }

    #[test]
    fn a_value_marked_unavailable_is_never_drawn() {
        let json = WORKER_METRICS.replace(
            r#""value": 12.5, "available": true"#,
            r#""value": 12.5, "available": false"#,
        );
        let p = panel(&json);
        let ChartBody::Series(cpu) = &chart(&p, "host_cpu_pct").body else {
            panic!("other readings exist");
        };
        assert_eq!(
            cpu.points()[0].reading,
            Reading::Unavailable { reason: None }
        );
    }

    #[test]
    fn every_chart_shares_the_requested_range_so_they_line_up() {
        let p = panel(WORKER_METRICS);
        // 2026-01-01T10:00:00Z .. 11:00:00Z
        assert_eq!(p.domain_ms, (1_767_261_600_000, 1_767_265_200_000));
    }

    #[test]
    fn charts_are_grouped_by_what_they_measure_and_host_comes_first() {
        let p = panel(WORKER_METRICS);
        let headings: Vec<_> = p.sections.iter().map(|s| s.heading).collect();
        assert_eq!(headings, ["Host (the machine)", "Maestro process"]);
        let host: Vec<_> = p.sections[0].charts.iter().map(|c| c.id.as_str()).collect();
        // The response's order within a group is kept.
        assert_eq!(
            host,
            ["host_thermal_c", "host_cpu_pct", "host_mem_used_bytes"]
        );
    }

    /// Two agent runs on one worker: Maestro's CPU is one series per run, each its own line.
    const TWO_RUNS: &str = r#"{
      "subject": {"kind": "worker", "id": "00000000-0000-0000-0000-0000000000a1"},
      "from": "2026-01-01T10:00:00Z", "to": "2026-01-01T11:00:00Z",
      "resolution": "raw", "truncated": false,
      "series": [
        {"metric": "runner_cpu_pct", "label": "Maestro CPU", "unit": "percent",
         "scope": "runner", "run_id": "3f2a9c1d-0000-4000-8000-000000000001", "available": true,
         "points": [{"at_ms": 1767261600000, "value": 100.0, "available": true}]},
        {"metric": "runner_cpu_pct", "label": "Maestro CPU", "unit": "percent",
         "scope": "runner", "run_id": "9b7e0d4c-0000-4000-8000-000000000002", "available": true,
         "points": [{"at_ms": 1767261600000, "value": 300.0, "available": true}]}
      ]
    }"#;

    #[test]
    fn each_run_on_a_worker_is_its_own_runner_line_labelled_by_its_run() {
        let p = panel(TWO_RUNS);
        assert_eq!(p.sections.len(), 1);
        let charts = &p.sections[0].charts;
        let titles: Vec<&str> = charts.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Maestro CPU · run 3f2a9c1", "Maestro CPU · run 9b7e0d4"]
        );
        let readings: Vec<Reading> = charts
            .iter()
            .map(|c| match &c.body {
                ChartBody::Series(s) => s.points()[0].reading.clone(),
                ChartBody::Unavailable { reason } => panic!("run has readings: {reason:?}"),
            })
            .collect();
        assert_eq!(readings, [Reading::Value(100.0), Reading::Value(300.0)]);
    }

    /// A 7-day response: one-minute aggregates, cut short, host CPU averaged, Maestro memory
    /// kept as the bucket maximum, and the agent's own CPU not kept in aggregates at all.
    const WEEK_METRICS: &str = r#"{
      "subject": {"kind": "worker", "id": "00000000-0000-0000-0000-0000000000a1"},
      "from": "2026-01-01T00:00:00Z", "to": "2026-01-08T00:00:00Z",
      "resolution": "1m", "truncated": true,
      "series": [
        {"metric": "host_cpu_pct", "label": "Host CPU", "unit": "percent", "scope": "host",
         "aggregate": "avg", "available": true,
         "points": [{"at_ms": 1767225600000, "value": 20.0, "available": true}]},
        {"metric": "runner_rss_bytes", "label": "Maestro memory", "unit": "bytes",
         "scope": "runner", "aggregate": "max", "available": true,
         "points": [{"at_ms": 1767225600000, "value": 1073741824.0, "available": true}]},
        {"metric": "agent_cpu_pct", "label": "Agent CPU", "unit": "percent", "scope": "agent",
         "available": false, "reason": "not kept in aggregates; use a range of 24 hours or less",
         "points": []}
      ]
    }"#;

    #[test]
    fn aggregated_figures_say_what_they_are_and_a_cut_response_says_it_stops_short() {
        let p = panel(WEEK_METRICS);
        assert_eq!(p.resolution_note, "1-minute averages");
        assert_eq!(p.provenance, Provenance::Derived);
        assert_eq!(
            chart(&p, "host_cpu_pct").aggregate_note,
            Some("Average per bucket")
        );
        assert_eq!(
            chart(&p, "runner_rss_bytes").aggregate_note,
            Some("Maximum per bucket")
        );
        assert_eq!(
            p.truncated_note,
            Some(
                "More samples matched than one response carries: the charts end before the end of the range."
            )
        );
        // An empty series with no value is explicit, with the server's reason.
        let ChartBody::Unavailable { reason } = &chart(&p, "agent_cpu_pct").body else {
            panic!("no points, nothing to draw");
        };
        assert_eq!(
            reason.as_deref(),
            Some("not kept in aggregates; use a range of 24 hours or less")
        );
    }

    #[test]
    fn raw_samples_have_no_aggregate_note_and_no_truncation_warning() {
        let p = panel(WORKER_METRICS);
        assert_eq!(p.resolution_note, "Raw samples");
        assert_eq!(p.provenance, Provenance::Observed);
        assert_eq!(p.truncated_note, None);
        assert_eq!(chart(&p, "host_cpu_pct").aggregate_note, None);
    }

    fn at(rfc3339: &str) -> time::OffsetDateTime {
        time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    #[test]
    fn a_preset_asks_for_the_last_stretch_up_to_now() {
        let now = at("2026-01-01T11:00:00Z");
        let q = RangeSelection::Preset(RangePreset::Last6Hours)
            .query(now, 300)
            .unwrap();
        assert_eq!(
            q.to_query_string(),
            "from=2026-01-01T05:00:00Z&to=2026-01-01T11:00:00Z&points=300"
        );
    }

    #[test]
    fn presets_are_labelled_for_the_picker_in_order() {
        let labels: Vec<_> = RangePreset::ALL.iter().map(|p| p.label()).collect();
        assert_eq!(
            labels,
            ["15 minutes", "1 hour", "6 hours", "24 hours", "7 days"]
        );
        let q = RangeSelection::Preset(RangePreset::Last7Days)
            .query(at("2026-01-08T00:00:00Z"), 300)
            .unwrap();
        assert_eq!(q.from, at("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn a_custom_range_is_sent_as_chosen_and_must_run_forward() {
        let now = at("2026-01-01T11:00:00Z");
        let custom = |from, to| RangeSelection::Custom {
            from: at(from),
            to: at(to),
        };
        let q = custom("2026-01-01T09:30:00+02:00", "2026-01-01T10:00:00Z")
            .query(now, 300)
            .unwrap();
        // Always UTC on the wire, whatever offset the user typed.
        assert_eq!(
            q.to_query_string(),
            "from=2026-01-01T07:30:00Z&to=2026-01-01T10:00:00Z&points=300"
        );
        assert_eq!(
            custom("2026-01-01T10:00:00Z", "2026-01-01T10:00:00Z").query(now, 300),
            Err(RangeError::NotForward)
        );
        assert_eq!(
            custom("2026-01-01T10:00:00Z", "2026-01-01T09:00:00Z").query(now, 300),
            Err(RangeError::NotForward)
        );
    }

    #[test]
    fn the_point_budget_stays_inside_what_the_server_accepts() {
        let now = at("2026-01-01T11:00:00Z");
        let points = |n| {
            RangeSelection::Preset(RangePreset::LastHour)
                .query(now, n)
                .unwrap()
                .points
        };
        assert_eq!(points(1), 4);
        assert_eq!(points(900), 900);
        assert_eq!(points(1_000_000), 5000);
    }

    #[test]
    fn a_selection_becomes_the_query_at_the_given_time() {
        // 2026-01-01T11:00:00Z
        let now_ms = 1_767_265_200_000;
        assert_eq!(
            query_string_at(RangeSelection::Preset(RangePreset::Last15Minutes), now_ms).as_deref(),
            Ok("from=2026-01-01T10:45:00Z&to=2026-01-01T11:00:00Z&points=300")
        );
    }
}
