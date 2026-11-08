// SPDX-License-Identifier: AGPL-3.0-only
//! Screen C's metric samples: raw rows and the one-minute and one-hour aggregates.

use fleet_protocol::{
    frame::{DeviceUnavailable, SampleUnavailable},
    rest::MetricResolution,
};
use sqlx::types::Json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Result, Store};

/// Samples one response carries at most; beyond that the answer says it is cut short.
pub const SAMPLE_LIMIT: i64 = 200_000;

/// One host sample, or one aggregate bucket, of a worker's machine. Host figures are per worker:
/// the runs on it are averaged into one series. `None`: no reading.
#[derive(Debug, Clone, PartialEq)]
pub struct HostRow {
    /// When it was taken (the bucket's start).
    pub at: OffsetDateTime,
    /// Host CPU, percent.
    pub host_cpu_pct: Option<f64>,
    /// Host memory in use, bytes.
    pub host_mem_used_bytes: Option<f64>,
    /// Host memory installed, bytes (raw samples only).
    pub host_mem_total_bytes: Option<f64>,
    /// Host load average over one minute.
    pub host_load_1m: Option<f64>,
    /// Host physical CPU cores (raw samples only).
    pub host_cpu_cores: Option<f64>,
    /// Host swap in use, bytes.
    pub host_swap_used_bytes: Option<f64>,
    /// Host swap configured, bytes (raw samples only).
    pub host_swap_total_bytes: Option<f64>,
    /// Linux pressure stall: percent of the last 10 seconds with some task waiting for CPU.
    pub host_psi_cpu_pct: Option<f64>,
    /// Linux pressure stall: percent of the last 10 seconds with some task waiting for memory.
    pub host_psi_memory_pct: Option<f64>,
    /// Linux pressure stall: percent of the last 10 seconds with some task waiting for I/O.
    pub host_psi_io_pct: Option<f64>,
    /// Why each unknown reading is unknown. Raw samples only: aggregates keep no reasons, and a raw
    /// sample from an agent that predates them has none.
    pub unavailable: Option<Json<SampleUnavailable>>,
}

/// One sample, or one aggregate bucket, of an agent run's own processes: its Maestro process and
/// the agent itself. Each run has its own processes, so runs that share a worker never share these
/// readings. `None`: no reading.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessRow {
    /// When it was taken (the bucket's start).
    pub at: OffsetDateTime,
    /// The agent run whose processes these are.
    pub run_id: Uuid,
    /// Maestro's CPU, percent of one core (the bucket's average when aggregated).
    pub maestro_cpu_pct: Option<f64>,
    /// Maestro's resident memory, bytes (the bucket's maximum when aggregated).
    pub maestro_rss_bytes: Option<f64>,
    /// The agent's CPU, percent of one core (raw samples only).
    pub agent_cpu_pct: Option<f64>,
    /// The agent's resident memory, bytes (raw samples only).
    pub agent_rss_bytes: Option<f64>,
}

/// One device sample, or one aggregate bucket. `None`: no reading.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceRow {
    /// When it was taken (the bucket's start).
    pub at: OffsetDateTime,
    /// The emulator process's CPU, percent.
    pub emu_cpu_pct: Option<f64>,
    /// The emulator process's resident memory, bytes (the bucket's maximum when aggregated).
    pub emu_rss_bytes: Option<f64>,
    /// adb's state as its code (`device_samples.adb_state`: 0 device, 1 offline, 2 unauthorized,
    /// 3 absent, 4 another state; raw samples only).
    pub adb_state: Option<f64>,
    /// Whether `sys.boot_completed` was set, as 1 or 0 (raw samples only).
    pub boot_completed: Option<f64>,
    /// The guest's CPU, percent (raw samples only).
    pub guest_cpu_pct: Option<f64>,
    /// The guest's available memory, bytes (raw samples only).
    pub guest_mem_available_bytes: Option<f64>,
}

/// A device session's newest sample, every reading as the agent sent it (PLAN §6.5 Devices). A
/// reading the agent did not report is `None`, and `unavailable` says why. `adb_state` is the code
/// of `device_samples.adb_state` (see `telemetry`).
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceLatest {
    /// When it was taken.
    pub at: OffsetDateTime,
    /// The shard the device was assigned to; `None` from agents that predate it.
    pub shard: Option<i32>,
    /// adb's state code.
    pub adb_state: Option<i16>,
    /// Whether `sys.boot_completed` was set.
    pub boot_completed: Option<bool>,
    /// The guest's CPU, percent.
    pub guest_cpu_pct: Option<f64>,
    /// The guest's available memory, bytes.
    pub guest_mem_available_bytes: Option<f64>,
    /// The emulator process's CPU, percent of one core.
    pub emu_cpu_pct: Option<f64>,
    /// The emulator process's resident memory, bytes.
    pub emu_rss_bytes: Option<f64>,
    /// The AVD name.
    pub avd_name: Option<String>,
    /// The API level.
    pub api_level: Option<i16>,
    /// The resumed activity, on a sample taken after a failed flow.
    pub foreground_activity: Option<String>,
    /// Why each unknown reading is unknown; `None` from agents that predate it.
    pub unavailable: Option<Json<DeviceUnavailable>>,
}

/// Samples read for a range.
#[derive(Debug, Clone, PartialEq)]
pub struct Samples<T> {
    /// Ascending in time.
    pub rows: Vec<T>,
    /// More than [`SAMPLE_LIMIT`] matched: `rows` stops short of the end of the range.
    pub truncated: bool,
}

fn capped<T>(mut rows: Vec<T>) -> Samples<T> {
    let limit = usize::try_from(SAMPLE_LIMIT).unwrap_or(usize::MAX);
    let truncated = rows.len() > limit;
    rows.truncate(limit);
    Samples { rows, truncated }
}

impl Store {
    /// A device session's newest sample. `None` when it has none yet.
    pub async fn device_latest(
        &self,
        project_id: Uuid,
        device_id: Uuid,
    ) -> Result<Option<DeviceLatest>> {
        let row = sqlx::query_as!(
            DeviceLatest,
            "SELECT ts AS \"at!\", shard, adb_state, boot_completed,
                    guest_cpu_pct::float8 AS guest_cpu_pct,
                    guest_mem_available_bytes::float8 AS guest_mem_available_bytes,
                    emu_cpu_pct::float8 AS emu_cpu_pct, emu_rss_bytes::float8 AS emu_rss_bytes,
                    avd_name, api_level, foreground_activity,
                    unavailable AS \"unavailable: Json<DeviceUnavailable>\"
               FROM device_samples
              WHERE project_id = $1 AND device_session_id = $2
              ORDER BY ts DESC LIMIT 1",
            project_id,
            device_id
        )
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// A worker's host samples in `[from, to)`, from the raw table or an aggregate. `None` when
    /// the worker doesn't exist in the project.
    pub async fn host_samples(
        &self,
        project_id: Uuid,
        worker_id: Uuid,
        range: (OffsetDateTime, OffsetDateTime),
        resolution: MetricResolution,
    ) -> Result<Option<Samples<HostRow>>> {
        let exists = sqlx::query_scalar!(
            "SELECT true AS \"x!\" FROM workers WHERE id = $1 AND project_id = $2",
            worker_id,
            project_id
        )
        .fetch_optional(self.pool())
        .await?;
        if exists.is_none() {
            return Ok(None);
        }
        let (from, to) = range;
        let limit = SAMPLE_LIMIT + 1;
        let rows = match resolution {
            MetricResolution::Raw => {
                sqlx::query_as!(
                    HostRow,
                    "SELECT ts AS \"at!\", host_cpu_pct::float8 AS host_cpu_pct,
                            host_mem_used_bytes::float8 AS host_mem_used_bytes,
                            host_mem_total_bytes::float8 AS host_mem_total_bytes,
                            host_load_1m::float8 AS host_load_1m,
                            host_cpu_cores::float8 AS host_cpu_cores,
                            host_swap_used_bytes::float8 AS host_swap_used_bytes,
                            host_swap_total_bytes::float8 AS host_swap_total_bytes,
                            host_psi_cpu_pct::float8 AS host_psi_cpu_pct,
                            host_psi_memory_pct::float8 AS host_psi_memory_pct,
                            host_psi_io_pct::float8 AS host_psi_io_pct,
                            unavailable AS \"unavailable: Json<SampleUnavailable>\"
                       FROM host_samples
                      WHERE project_id = $1 AND worker_id = $2 AND ts >= $3 AND ts < $4
                      ORDER BY ts LIMIT $5",
                    project_id,
                    worker_id,
                    from,
                    to,
                    limit
                )
                .fetch_all(self.pool())
                .await?
            }
            MetricResolution::OneMinute => {
                sqlx::query_as!(
                    HostRow,
                    "SELECT bucket AS \"at!\", avg(host_cpu_pct_avg)::float8 AS host_cpu_pct,
                            avg(host_mem_used_avg)::float8 AS host_mem_used_bytes,
                            NULL::float8 AS host_mem_total_bytes,
                            avg(host_load_1m_avg)::float8 AS host_load_1m,
                            NULL::float8 AS host_cpu_cores,
                            avg(host_swap_used_avg)::float8 AS host_swap_used_bytes,
                            NULL::float8 AS host_swap_total_bytes,
                            avg(host_psi_cpu_pct_avg)::float8 AS host_psi_cpu_pct,
                            avg(host_psi_memory_pct_avg)::float8 AS host_psi_memory_pct,
                            avg(host_psi_io_pct_avg)::float8 AS host_psi_io_pct,
                            NULL::jsonb AS \"unavailable: Json<SampleUnavailable>\"
                       FROM host_rollup_1m
                      WHERE project_id = $1 AND worker_id = $2 AND bucket >= $3 AND bucket < $4
                      GROUP BY bucket ORDER BY bucket LIMIT $5",
                    project_id,
                    worker_id,
                    from,
                    to,
                    limit
                )
                .fetch_all(self.pool())
                .await?
            }
            MetricResolution::OneHour => {
                sqlx::query_as!(
                    HostRow,
                    "SELECT bucket AS \"at!\", avg(host_cpu_pct_avg)::float8 AS host_cpu_pct,
                            avg(host_mem_used_avg)::float8 AS host_mem_used_bytes,
                            NULL::float8 AS host_mem_total_bytes,
                            avg(host_load_1m_avg)::float8 AS host_load_1m,
                            NULL::float8 AS host_cpu_cores,
                            avg(host_swap_used_avg)::float8 AS host_swap_used_bytes,
                            NULL::float8 AS host_swap_total_bytes,
                            avg(host_psi_cpu_pct_avg)::float8 AS host_psi_cpu_pct,
                            avg(host_psi_memory_pct_avg)::float8 AS host_psi_memory_pct,
                            avg(host_psi_io_pct_avg)::float8 AS host_psi_io_pct,
                            NULL::jsonb AS \"unavailable: Json<SampleUnavailable>\"
                       FROM host_rollup_1h
                      WHERE project_id = $1 AND worker_id = $2 AND bucket >= $3 AND bucket < $4
                      GROUP BY bucket ORDER BY bucket LIMIT $5",
                    project_id,
                    worker_id,
                    from,
                    to,
                    limit
                )
                .fetch_all(self.pool())
                .await?
            }
        };
        Ok(Some(capped(rows)))
    }

    /// A worker's process samples in `[from, to)`, per run: the runs on one worker share its
    /// machine, not their processes. `None` when the worker doesn't exist in the project.
    pub async fn process_samples(
        &self,
        project_id: Uuid,
        worker_id: Uuid,
        range: (OffsetDateTime, OffsetDateTime),
        resolution: MetricResolution,
    ) -> Result<Option<Samples<ProcessRow>>> {
        let exists = sqlx::query_scalar!(
            "SELECT true AS \"x!\" FROM workers WHERE id = $1 AND project_id = $2",
            worker_id,
            project_id
        )
        .fetch_optional(self.pool())
        .await?;
        if exists.is_none() {
            return Ok(None);
        }
        let (from, to) = range;
        let limit = SAMPLE_LIMIT + 1;
        // Aggregates are kept per run already (one row per run and bucket), so nothing is
        // combined across runs here.
        let rows = match resolution {
            MetricResolution::Raw => {
                sqlx::query_as!(
                    ProcessRow,
                    "SELECT ts AS \"at!\", agent_run_id AS \"run_id!\",
                            maestro_cpu_pct::float8 AS maestro_cpu_pct,
                            maestro_rss_bytes::float8 AS maestro_rss_bytes,
                            agent_cpu_pct::float8 AS agent_cpu_pct,
                            agent_rss_bytes::float8 AS agent_rss_bytes
                       FROM host_samples
                      WHERE project_id = $1 AND worker_id = $2 AND ts >= $3 AND ts < $4
                      ORDER BY ts, agent_run_id LIMIT $5",
                    project_id,
                    worker_id,
                    from,
                    to,
                    limit
                )
                .fetch_all(self.pool())
                .await?
            }
            MetricResolution::OneMinute => {
                sqlx::query_as!(
                    ProcessRow,
                    "SELECT bucket AS \"at!\", agent_run_id AS \"run_id!\",
                            maestro_cpu_avg::float8 AS maestro_cpu_pct,
                            maestro_rss_max::float8 AS maestro_rss_bytes,
                            NULL::float8 AS agent_cpu_pct, NULL::float8 AS agent_rss_bytes
                       FROM host_rollup_1m
                      WHERE project_id = $1 AND worker_id = $2 AND bucket >= $3 AND bucket < $4
                      ORDER BY bucket, agent_run_id LIMIT $5",
                    project_id,
                    worker_id,
                    from,
                    to,
                    limit
                )
                .fetch_all(self.pool())
                .await?
            }
            MetricResolution::OneHour => {
                sqlx::query_as!(
                    ProcessRow,
                    "SELECT bucket AS \"at!\", agent_run_id AS \"run_id!\",
                            maestro_cpu_avg::float8 AS maestro_cpu_pct,
                            maestro_rss_max::float8 AS maestro_rss_bytes,
                            NULL::float8 AS agent_cpu_pct, NULL::float8 AS agent_rss_bytes
                       FROM host_rollup_1h
                      WHERE project_id = $1 AND worker_id = $2 AND bucket >= $3 AND bucket < $4
                      ORDER BY bucket, agent_run_id LIMIT $5",
                    project_id,
                    worker_id,
                    from,
                    to,
                    limit
                )
                .fetch_all(self.pool())
                .await?
            }
        };
        Ok(Some(capped(rows)))
    }

    /// A device session's samples in `[from, to)`. `None` when the device doesn't exist in the
    /// project.
    pub async fn device_samples(
        &self,
        project_id: Uuid,
        device_id: Uuid,
        range: (OffsetDateTime, OffsetDateTime),
        resolution: MetricResolution,
    ) -> Result<Option<Samples<DeviceRow>>> {
        let exists = sqlx::query_scalar!(
            "SELECT true AS \"x!\" FROM device_sessions WHERE id = $1 AND project_id = $2",
            device_id,
            project_id
        )
        .fetch_optional(self.pool())
        .await?;
        if exists.is_none() {
            return Ok(None);
        }
        let (from, to) = range;
        let limit = SAMPLE_LIMIT + 1;
        let rows = match resolution {
            MetricResolution::Raw => {
                sqlx::query_as!(
                    DeviceRow,
                    "SELECT ts AS \"at!\", emu_cpu_pct::float8 AS emu_cpu_pct,
                            emu_rss_bytes::float8 AS emu_rss_bytes,
                            adb_state::float8 AS adb_state,
                            boot_completed::int::float8 AS boot_completed,
                            guest_cpu_pct::float8 AS guest_cpu_pct,
                            guest_mem_available_bytes::float8 AS guest_mem_available_bytes
                       FROM device_samples
                      WHERE project_id = $1 AND device_session_id = $2 AND ts >= $3 AND ts < $4
                      ORDER BY ts LIMIT $5",
                    project_id,
                    device_id,
                    from,
                    to,
                    limit
                )
                .fetch_all(self.pool())
                .await?
            }
            MetricResolution::OneMinute => {
                sqlx::query_as!(
                    DeviceRow,
                    "SELECT bucket AS \"at!\", emu_cpu_pct_avg::float8 AS emu_cpu_pct,
                            emu_rss_max::float8 AS emu_rss_bytes,
                            NULL::float8 AS adb_state, NULL::float8 AS boot_completed,
                            NULL::float8 AS guest_cpu_pct,
                            NULL::float8 AS guest_mem_available_bytes
                       FROM device_rollup_1m
                      WHERE project_id = $1 AND device_session_id = $2
                        AND bucket >= $3 AND bucket < $4
                      ORDER BY bucket LIMIT $5",
                    project_id,
                    device_id,
                    from,
                    to,
                    limit
                )
                .fetch_all(self.pool())
                .await?
            }
            MetricResolution::OneHour => {
                sqlx::query_as!(
                    DeviceRow,
                    "SELECT bucket AS \"at!\", emu_cpu_pct_avg::float8 AS emu_cpu_pct,
                            emu_rss_max::float8 AS emu_rss_bytes,
                            NULL::float8 AS adb_state, NULL::float8 AS boot_completed,
                            NULL::float8 AS guest_cpu_pct,
                            NULL::float8 AS guest_mem_available_bytes
                       FROM device_rollup_1h
                      WHERE project_id = $1 AND device_session_id = $2
                        AND bucket >= $3 AND bucket < $4
                      ORDER BY bucket LIMIT $5",
                    project_id,
                    device_id,
                    from,
                    to,
                    limit
                )
                .fetch_all(self.pool())
                .await?
            }
        };
        Ok(Some(capped(rows)))
    }
}
