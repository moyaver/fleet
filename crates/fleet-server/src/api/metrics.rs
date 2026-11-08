// SPDX-License-Identifier: AGPL-3.0-only
//! Screen C's metric series for workers and devices (acceptance criteria 4 and 5).
//!
//! Every metric has a fixed label, unit and scope. Nothing is invented: a metric the agent never
//! reports (thermal state) is an unavailable series with a reason, a sample without a reading is
//! an unavailable point, and a range without samples says so. Long series are cut to the
//! requested number of points with Largest-Triangle-Three-Buckets, which keeps the first and last
//! reading and the shape in between; each run of missing readings becomes one unavailable point
//! where it begins.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use fleet_core::downsample::lttb;
use fleet_protocol::{
    frame::{DeviceUnavailable, Unavailable},
    rest::{
        DeviceLatestView, DeviceReadingView, LatestReadings, MetricAggregate, MetricPoint,
        MetricReading, MetricResolution, MetricScope, MetricSubject, MetricSubjectKind, MetricUnit,
        MetricsView, SeriesView,
    },
};
use fleet_store::screens::{DeviceLatest, DeviceRow, HostRow, ProcessRow};
use serde::Deserialize;
use std::collections::HashMap;
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use super::{ApiError, error, internal, read_project};
use crate::{session::Caller, state::AppState};

const REASON_NOT_REPORTED: &str = "the agent does not report this metric yet";
const REASON_NO_SAMPLES: &str = "no samples in this range";
const REASON_NO_VALUE: &str = "the agent reported no value";
const REASON_ALL_MISSING: &str = "the agent reported no value in this range";
const REASON_NOT_AGGREGATED: &str = "not kept in aggregates; use a range of 24 hours or less";
const REASON_NOT_SAMPLED: &str = "not read in this sample";
const REASON_NOT_AN_EMULATOR: &str = "not an emulator, so there is no emulator process to read";
const REASON_NO_EMULATOR_PROCESS: &str = "no emulator process maps to this device";
const REASON_NO_AVD_NAME: &str = "the emulator's command line names no AVD";
const REASON_PROBE_FAILED: &str = "the probe failed: adb did not answer it";
const REASON_THROTTLED: &str = "throttled: the host is under pressure";
const REASON_NO_BASELINE: &str = "the first probe only sets the CPU baseline";
const REASON_NO_ADB_LIST: &str = "adb has not listed devices yet";
const REASON_NO_SYSTEM_INFO: &str = "the platform's system information could not be read";
const REASON_NO_PSI: &str = "the kernel has no pressure information (PSI), as on macOS";
const REASON_NO_PROCESS: &str = "the process is not known to this run yet, or it has exited";
const REASON_UNKNOWN_AGENT_REASON: &str = "the agent gave a reason this server does not know";

/// Raw samples cover ranges up to this long.
const RAW_UP_TO: Duration = Duration::hours(24);
/// One-minute aggregates cover ranges up to this long; hourly ones beyond.
const MINUTES_UP_TO: Duration = Duration::days(30);
/// Fewest and most points a response may be asked to carry.
const POINTS: std::ops::RangeInclusive<usize> = 4..=5000;
const DEFAULT_POINTS: usize = 300;

/// One metric of the catalog. `col` is `None` for a metric the agent doesn't report yet;
/// `rolled` is how the aggregate tables keep it (`None`: they don't).
struct Def<C> {
    id: &'static str,
    label: &'static str,
    unit: MetricUnit,
    scope: MetricScope,
    col: Option<C>,
    rolled: Option<MetricAggregate>,
}

#[derive(Clone, Copy)]
enum HostCol {
    CpuPct,
    MemUsed,
    MemTotal,
    Load1m,
    CpuCores,
    SwapUsed,
    SwapTotal,
    PsiCpu,
    PsiMemory,
    PsiIo,
}

#[derive(Clone, Copy)]
enum ProcessCol {
    MaestroCpu,
    MaestroRss,
    AgentCpu,
    AgentRss,
}

#[derive(Clone, Copy)]
enum DeviceCol {
    EmuCpu,
    EmuRss,
    AdbState,
    BootCompleted,
    GuestCpu,
    GuestMemAvailable,
}

const fn def<C>(
    id: &'static str,
    label: &'static str,
    unit: MetricUnit,
    scope: MetricScope,
    col: Option<C>,
    rolled: Option<MetricAggregate>,
) -> Def<C> {
    Def {
        id,
        label,
        unit,
        scope,
        col,
        rolled,
    }
}

use MetricAggregate::{Avg, Max};
use MetricScope::{Agent, Device, Emulator, Guest, Host, Maestro};
use MetricUnit::{Bytes, Celsius, Number, Percent};

const HOST: [Def<HostCol>; 11] = [
    def(
        "host_cpu_pct",
        "Host CPU",
        Percent,
        Host,
        Some(HostCol::CpuPct),
        Some(Avg),
    ),
    def(
        "host_mem_used_bytes",
        "Host memory used",
        Bytes,
        Host,
        Some(HostCol::MemUsed),
        Some(Avg),
    ),
    def(
        "host_mem_total_bytes",
        "Host memory total",
        Bytes,
        Host,
        Some(HostCol::MemTotal),
        None,
    ),
    def(
        "host_load_1m",
        "Host load (1 min)",
        Number,
        Host,
        Some(HostCol::Load1m),
        Some(Avg),
    ),
    def(
        "host_cpu_cores",
        "Host CPU cores",
        Number,
        Host,
        Some(HostCol::CpuCores),
        None,
    ),
    def(
        "host_swap_used_bytes",
        "Host swap used",
        Bytes,
        Host,
        Some(HostCol::SwapUsed),
        Some(Avg),
    ),
    def(
        "host_swap_total_bytes",
        "Host swap total",
        Bytes,
        Host,
        Some(HostCol::SwapTotal),
        None,
    ),
    def(
        "host_psi_cpu_pct",
        "Host CPU pressure",
        Percent,
        Host,
        Some(HostCol::PsiCpu),
        Some(Avg),
    ),
    def(
        "host_psi_memory_pct",
        "Host memory pressure",
        Percent,
        Host,
        Some(HostCol::PsiMemory),
        Some(Avg),
    ),
    def(
        "host_psi_io_pct",
        "Host I/O pressure",
        Percent,
        Host,
        Some(HostCol::PsiIo),
        Some(Avg),
    ),
    def(
        "host_thermal_c",
        "Host temperature",
        Celsius,
        Host,
        None,
        None,
    ),
];

/// The processes of each agent run, so a worker that ran several runs at once has one series per
/// run for each of them: Maestro's, and the agent's own.
const PROCESS: [Def<ProcessCol>; 4] = [
    def(
        "maestro_cpu_pct",
        "Maestro CPU",
        Percent,
        Maestro,
        Some(ProcessCol::MaestroCpu),
        Some(Avg),
    ),
    def(
        "maestro_rss_bytes",
        "Maestro memory",
        Bytes,
        Maestro,
        Some(ProcessCol::MaestroRss),
        Some(Max),
    ),
    def(
        "agent_cpu_pct",
        "Agent CPU",
        Percent,
        Agent,
        Some(ProcessCol::AgentCpu),
        None,
    ),
    def(
        "agent_rss_bytes",
        "Agent memory",
        Bytes,
        Agent,
        Some(ProcessCol::AgentRss),
        None,
    ),
];

const DEVICE: [Def<DeviceCol>; 7] = [
    def(
        "emu_cpu_pct",
        "Emulator CPU",
        Percent,
        Emulator,
        Some(DeviceCol::EmuCpu),
        Some(Avg),
    ),
    def(
        "emu_rss_bytes",
        "Emulator memory",
        Bytes,
        Emulator,
        Some(DeviceCol::EmuRss),
        Some(Max),
    ),
    def(
        "adb_state",
        "ADB state (0 device, 1 offline, 2 unauthorized, 3 absent, 4 other)",
        Number,
        Device,
        Some(DeviceCol::AdbState),
        None,
    ),
    def(
        "boot_completed",
        "Boot completed (1 = yes, 0 = not yet)",
        Number,
        Device,
        Some(DeviceCol::BootCompleted),
        None,
    ),
    def(
        "guest_cpu_pct",
        "Guest CPU",
        Percent,
        Guest,
        Some(DeviceCol::GuestCpu),
        None,
    ),
    def(
        "guest_mem_available_bytes",
        "Guest memory available",
        Bytes,
        Guest,
        Some(DeviceCol::GuestMemAvailable),
        None,
    ),
    def(
        "device_thermal_c",
        "Device temperature",
        Celsius,
        Device,
        None,
        None,
    ),
];

fn host_value(c: HostCol, r: &HostRow) -> Option<f64> {
    match c {
        HostCol::CpuPct => r.host_cpu_pct,
        HostCol::MemUsed => r.host_mem_used_bytes,
        HostCol::MemTotal => r.host_mem_total_bytes,
        HostCol::Load1m => r.host_load_1m,
        HostCol::CpuCores => r.host_cpu_cores,
        HostCol::SwapUsed => r.host_swap_used_bytes,
        HostCol::SwapTotal => r.host_swap_total_bytes,
        HostCol::PsiCpu => r.host_psi_cpu_pct,
        HostCol::PsiMemory => r.host_psi_memory_pct,
        HostCol::PsiIo => r.host_psi_io_pct,
    }
}

fn process_value(c: ProcessCol, r: &ProcessRow) -> Option<f64> {
    match c {
        ProcessCol::MaestroCpu => r.maestro_cpu_pct,
        ProcessCol::MaestroRss => r.maestro_rss_bytes,
        ProcessCol::AgentCpu => r.agent_cpu_pct,
        ProcessCol::AgentRss => r.agent_rss_bytes,
    }
}

fn device_value(c: DeviceCol, r: &DeviceRow) -> Option<f64> {
    match c {
        DeviceCol::EmuCpu => r.emu_cpu_pct,
        DeviceCol::EmuRss => r.emu_rss_bytes,
        DeviceCol::AdbState => r.adb_state,
        DeviceCol::BootCompleted => r.boot_completed,
        DeviceCol::GuestCpu => r.guest_cpu_pct,
        DeviceCol::GuestMemAvailable => r.guest_mem_available_bytes,
    }
}

/// What a request asks for, validated.
struct Request {
    from: OffsetDateTime,
    to: OffsetDateTime,
    resolution: MetricResolution,
    points: usize,
    metrics: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct MetricsQuery {
    from: Option<String>,
    to: Option<String>,
    metrics: Option<String>,
    points: Option<usize>,
}

fn time_param(name: &str, value: &str) -> Result<OffsetDateTime, String> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| format!("{name} must be an RFC 3339 time, such as 2026-01-31T12:00:00Z"))
}

/// Checks the query against the metrics `known` for the subject. Defaults: the last hour, 300
/// points, every metric.
fn parse(q: &MetricsQuery, known: &[&'static str]) -> Result<Request, String> {
    let to = match &q.to {
        Some(t) => time_param("to", t)?,
        None => OffsetDateTime::now_utc(),
    };
    let from = match &q.from {
        Some(t) => time_param("from", t)?,
        None => to - Duration::hours(1),
    };
    if from >= to {
        return Err("from must be before to".into());
    }
    let points = q.points.unwrap_or(DEFAULT_POINTS);
    if !POINTS.contains(&points) {
        return Err(format!(
            "points must be between {} and {}",
            POINTS.start(),
            POINTS.end()
        ));
    }
    let mut metrics: Vec<String> = Vec::new();
    for name in q.metrics.as_deref().unwrap_or_default().split(',') {
        let name = name.trim();
        if name.is_empty() || metrics.iter().any(|m| m == name) {
            continue;
        }
        if !known.contains(&name) {
            return Err(format!(
                "unknown metric {name}; known metrics: {}",
                known.join(", ")
            ));
        }
        metrics.push(name.to_owned());
    }
    if metrics.is_empty() {
        metrics = known.iter().map(|m| (*m).to_owned()).collect();
    }
    let range = to - from;
    let resolution = if range <= RAW_UP_TO {
        MetricResolution::Raw
    } else if range <= MINUTES_UP_TO {
        MetricResolution::OneMinute
    } else {
        MetricResolution::OneHour
    };
    Ok(Request {
        from,
        to,
        resolution,
        points,
        metrics,
    })
}

/// The machine's metrics of one sample: its value, or why there is none. Process metrics are
/// per run and are not part of a worker's latest reading.
pub(super) fn latest_readings(row: &HostRow) -> LatestReadings {
    LatestReadings {
        at: row.at,
        readings: HOST
            .iter()
            .filter(|d| d.scope == Host)
            .map(|d| {
                let value = d.col.and_then(|c| host_value(c, row));
                MetricReading {
                    metric: d.id.to_owned(),
                    label: d.label.to_owned(),
                    unit: d.unit,
                    scope: d.scope,
                    value,
                    available: value.is_some(),
                    reason: match (value, d.col) {
                        (Some(_), _) => None,
                        (None, Some(_)) => Some(REASON_NO_VALUE.to_owned()),
                        (None, None) => Some(REASON_NOT_REPORTED.to_owned()),
                    },
                }
            })
            .collect(),
    }
}

/// Why a device reading is unknown, in words (PLAN §6.5).
fn unavailable_text(reason: Unavailable) -> &'static str {
    match reason {
        Unavailable::NotAnEmulator => REASON_NOT_AN_EMULATOR,
        Unavailable::NoEmulatorProcess => REASON_NO_EMULATOR_PROCESS,
        Unavailable::NoAvdName => REASON_NO_AVD_NAME,
        Unavailable::ProbeFailed => REASON_PROBE_FAILED,
        Unavailable::Throttled => REASON_THROTTLED,
        Unavailable::NoBaseline => REASON_NO_BASELINE,
        Unavailable::NoAdbList => REASON_NO_ADB_LIST,
        Unavailable::NotSampled => REASON_NOT_SAMPLED,
        Unavailable::NoSystemInfo => REASON_NO_SYSTEM_INFO,
        Unavailable::NoPsi => REASON_NO_PSI,
        Unavailable::NoProcess => REASON_NO_PROCESS,
        Unavailable::Other => REASON_UNKNOWN_AGENT_REASON,
    }
}

/// The name of an adb state code (`device_samples.adb_state`).
fn adb_state_text(code: i16) -> &'static str {
    match code {
        0 => "device",
        1 => "offline",
        2 => "unauthorized",
        3 => "absent",
        _ => "other",
    }
}

/// A device's newest sample as the device page shows it: every reading, with its scope, unit and
/// label, and a reason for each one the agent did not report.
pub(super) fn device_latest_view(row: &DeviceLatest) -> DeviceLatestView {
    let why: DeviceUnavailable = row.unavailable.as_ref().map(|j| j.0).unwrap_or_default();
    // A reading with no value and no reason (an agent that predates reasons) says so.
    let reason =
        |r: Option<Unavailable>| Some(r.map_or(REASON_NO_VALUE, unavailable_text).to_owned());
    let number =
        |metric: &str, label: &str, unit, scope, value: Option<f64>, r| DeviceReadingView {
            metric: metric.to_owned(),
            label: label.to_owned(),
            unit: Some(unit),
            scope,
            value,
            text: None,
            available: value.is_some(),
            reason: if value.is_some() { None } else { reason(r) },
        };
    let text = |metric: &str, label: &str, scope, value: Option<String>, r| DeviceReadingView {
        metric: metric.to_owned(),
        label: label.to_owned(),
        unit: None,
        scope,
        value: None,
        available: value.is_some(),
        reason: if value.is_some() { None } else { reason(r) },
        text: value,
    };
    DeviceLatestView {
        at: row.at,
        shard: row.shard.and_then(|s| u32::try_from(s).ok()),
        readings: vec![
            text(
                "adb_state",
                "ADB state",
                Device,
                row.adb_state.map(|c| adb_state_text(c).to_owned()),
                why.adb_state,
            ),
            number(
                "boot_completed",
                "Boot completed (1 = yes, 0 = not yet)",
                Number,
                Device,
                row.boot_completed.map(|b| if b { 1.0 } else { 0.0 }),
                why.boot_completed,
            ),
            number(
                "guest_cpu_pct",
                "Guest CPU",
                Percent,
                Guest,
                row.guest_cpu_pct,
                why.guest_cpu_pct,
            ),
            number(
                "guest_mem_available_bytes",
                "Guest memory available",
                Bytes,
                Guest,
                row.guest_mem_available_bytes,
                why.guest_mem_available_bytes,
            ),
            number(
                "emu_cpu_pct",
                "Emulator CPU",
                Percent,
                Emulator,
                row.emu_cpu_pct,
                why.emu_cpu_pct,
            ),
            number(
                "emu_rss_bytes",
                "Emulator memory",
                Bytes,
                Emulator,
                row.emu_rss_bytes,
                why.emu_rss_bytes,
            ),
            text(
                "avd_name",
                "AVD name",
                Device,
                row.avd_name.clone(),
                why.avd_name,
            ),
            number(
                "api_level",
                "API level",
                Number,
                Device,
                row.api_level.map(f64::from),
                why.api_level,
            ),
            text(
                "foreground_activity",
                "Foreground activity",
                Device,
                row.foreground_activity.clone(),
                why.foreground_activity,
            ),
        ],
    }
}

fn unix_ms(t: OffsetDateTime) -> i64 {
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)
}

const WORKER_PARAMS: &str = "Metrics to return, comma-separated (default: all)";

/// Screen C: a worker's host and process metrics over a range.
#[utoipa::path(get, path = "/api/v1/projects/{project}/workers/{worker}/metrics",
    params(("project" = String, Path, description = "Project slug"),
           ("worker" = Uuid, Path, description = "Worker ID"),
           ("from" = Option<String>, Query, description = "Start, RFC 3339 (default: an hour before `to`)"),
           ("to" = Option<String>, Query, description = "End, RFC 3339 (default: now)"),
           ("metrics" = Option<String>, Query, description = WORKER_PARAMS),
           ("points" = Option<usize>, Query, description = "At most this many points per series (4–5000, default 300)")),
    responses((status = 200, body = MetricsView), (status = 400, body = ApiError),
              (status = 404, body = ApiError)))]
pub(super) async fn worker_metrics(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path((project, worker)): Path<(String, Uuid)>,
    Query(q): Query<MetricsQuery>,
) -> Response {
    let project = match read_project(&state, &principal, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let known: Vec<&'static str> = HOST
        .iter()
        .map(|d| d.id)
        .chain(PROCESS.iter().map(|d| d.id))
        .collect();
    let req = match parse(&q, &known) {
        Ok(r) => r,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let host = match state
        .store
        .host_samples(project, worker, (req.from, req.to), req.resolution)
        .await
    {
        Ok(Some(s)) => s,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such worker"),
        Err(e) => return internal(&e),
    };
    let processes = match state
        .store
        .process_samples(project, worker, (req.from, req.to), req.resolution)
        .await
    {
        Ok(Some(s)) => s,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such worker"),
        Err(e) => return internal(&e),
    };
    let mut series_views = Vec::new();
    for id in &req.metrics {
        if let Some(d) = HOST.iter().find(|d| d.id == id) {
            let readings: Vec<(i64, Option<f64>)> = host
                .rows
                .iter()
                .map(|r| (unix_ms(r.at), d.col.and_then(|c| host_value(c, r))))
                .collect();
            series_views.push(series(d, &readings, &req, None));
        } else if let Some(d) = PROCESS.iter().find(|d| d.id == id) {
            let runs = runs_of(&processes.rows);
            if runs.is_empty() {
                series_views.push(series(d, &[], &req, None));
            }
            for (run, rows) in runs {
                let readings: Vec<(i64, Option<f64>)> = rows
                    .iter()
                    .map(|r| (unix_ms(r.at), d.col.and_then(|c| process_value(c, r))))
                    .collect();
                series_views.push(series(d, &readings, &req, Some(run)));
            }
        }
    }
    Json(MetricsView {
        subject: MetricSubject {
            kind: MetricSubjectKind::Worker,
            id: worker,
        },
        from: req.from,
        to: req.to,
        resolution: req.resolution,
        truncated: host.truncated || processes.truncated,
        series: series_views,
    })
    .into_response()
}

/// Each run's rows, runs in the order they first appear (rows keep their order).
fn runs_of(rows: &[ProcessRow]) -> Vec<(Uuid, Vec<&ProcessRow>)> {
    let mut runs: Vec<(Uuid, Vec<&ProcessRow>)> = Vec::new();
    let mut at: HashMap<Uuid, usize> = HashMap::new();
    for r in rows {
        let i = *at.entry(r.run_id).or_insert(runs.len());
        if i == runs.len() {
            runs.push((r.run_id, Vec::new()));
        }
        if let Some((_, own)) = runs.get_mut(i) {
            own.push(r);
        }
    }
    runs
}

/// Screen C: a device session's emulator and guest metrics over a range.
#[utoipa::path(get, path = "/api/v1/projects/{project}/devices/{device}/metrics",
    params(("project" = String, Path, description = "Project slug"),
           ("device" = Uuid, Path, description = "Device session ID"),
           ("from" = Option<String>, Query, description = "Start, RFC 3339 (default: an hour before `to`)"),
           ("to" = Option<String>, Query, description = "End, RFC 3339 (default: now)"),
           ("metrics" = Option<String>, Query, description = WORKER_PARAMS),
           ("points" = Option<usize>, Query, description = "At most this many points per series (4–5000, default 300)")),
    responses((status = 200, body = MetricsView), (status = 400, body = ApiError),
              (status = 404, body = ApiError)))]
pub(super) async fn device_metrics(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path((project, device)): Path<(String, Uuid)>,
    Query(q): Query<MetricsQuery>,
) -> Response {
    let project = match read_project(&state, &principal, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let known: Vec<&'static str> = DEVICE.iter().map(|d| d.id).collect();
    let req = match parse(&q, &known) {
        Ok(r) => r,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let samples = match state
        .store
        .device_samples(project, device, (req.from, req.to), req.resolution)
        .await
    {
        Ok(Some(s)) => s,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such device"),
        Err(e) => return internal(&e),
    };
    let series = req
        .metrics
        .iter()
        .filter_map(|id| DEVICE.iter().find(|d| d.id == id))
        .map(|d| {
            let readings: Vec<(i64, Option<f64>)> = samples
                .rows
                .iter()
                .map(|r| (unix_ms(r.at), d.col.and_then(|c| device_value(c, r))))
                .collect();
            series(d, &readings, &req, None)
        })
        .collect();
    Json(MetricsView {
        subject: MetricSubject {
            kind: MetricSubjectKind::Device,
            id: device,
        },
        from: req.from,
        to: req.to,
        resolution: req.resolution,
        truncated: samples.truncated,
        series,
    })
    .into_response()
}

/// One metric's series from its readings (`None` where a sample had no value). `run` is set for
/// a process metric, which is a series per run.
fn series<C>(
    d: &Def<C>,
    readings: &[(i64, Option<f64>)],
    req: &Request,
    run: Option<Uuid>,
) -> SeriesView {
    let raw = req.resolution == MetricResolution::Raw;
    let mut view = SeriesView {
        metric: d.id.to_owned(),
        label: d.label.to_owned(),
        unit: d.unit,
        scope: d.scope,
        run_id: run,
        aggregate: if raw { None } else { d.rolled },
        available: false,
        reason: None,
        points: Vec::new(),
    };
    if d.col.is_none() {
        view.reason = Some(REASON_NOT_REPORTED.into());
    } else if !raw && d.rolled.is_none() {
        view.reason = Some(REASON_NOT_AGGREGATED.into());
    } else if readings.is_empty() {
        view.reason = Some(REASON_NO_SAMPLES.into());
    } else {
        view.available = readings.iter().any(|(_, v)| v.is_some());
        if !view.available {
            view.reason = Some(REASON_ALL_MISSING.into());
        }
        view.points = downsample(readings, req.points);
    }
    view
}

fn point(&(at_ms, value): &(i64, Option<f64>)) -> MetricPoint {
    MetricPoint {
        at_ms,
        value,
        available: value.is_some(),
        reason: value.is_none().then(|| REASON_NO_VALUE.to_owned()),
    }
}

/// At most `budget` points: the readings themselves when they fit, else a selection of them.
/// Values are only ever picked, never averaged or filled in.
fn downsample(readings: &[(i64, Option<f64>)], budget: usize) -> Vec<MetricPoint> {
    if readings.len() <= budget {
        return readings.iter().map(point).collect();
    }
    let gap_starts: Vec<usize> = readings
        .iter()
        .enumerate()
        .filter(|(i, (_, v))| {
            v.is_none()
                && i.checked_sub(1)
                    .and_then(|p| readings.get(p))
                    .is_none_or(|(_, prev)| prev.is_some())
        })
        .map(|(i, _)| i)
        .collect();
    let have: Vec<usize> = readings
        .iter()
        .enumerate()
        .filter(|(_, (_, v))| v.is_some())
        .map(|(i, _)| i)
        .collect();
    // Never more markers than half the budget, spread evenly when there are more gaps.
    let gap_budget = gap_starts.len().min(budget / 2);
    let mut kept: Vec<usize> = (0..gap_budget)
        .filter_map(|k| gap_starts.get(k * gap_starts.len() / gap_budget).copied())
        .collect();
    let xy: Vec<(f64, f64)> = have
        .iter()
        .filter_map(|i| readings.get(*i))
        .map(|(at, v)| (at_as_f64(*at), v.unwrap_or_default()))
        .collect();
    kept.extend(
        lttb(&xy, budget - gap_budget)
            .into_iter()
            .filter_map(|i| have.get(i).copied()),
    );
    kept.sort_unstable();
    kept.dedup();
    kept.iter()
        .filter_map(|i| readings.get(*i))
        .map(point)
        .collect()
}

#[expect(
    clippy::cast_precision_loss,
    reason = "unix milliseconds are far below 2^52"
)]
fn at_as_f64(at_ms: i64) -> f64 {
    at_ms as f64
}
