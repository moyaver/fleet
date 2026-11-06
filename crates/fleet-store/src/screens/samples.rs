// SPDX-License-Identifier: AGPL-3.0-only
//! Screen C's metric samples: raw rows and the one-minute and one-hour aggregates.

use fleet_protocol::rest::MetricResolution;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Result, Store};

/// Samples one response carries at most; beyond that the answer says it is cut short.
pub const SAMPLE_LIMIT: i64 = 200_000;

/// One host sample, or one aggregate bucket. `None`: no reading.
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
    /// Maestro's CPU, percent of one core.
    pub maestro_cpu_pct: Option<f64>,
    /// Maestro's resident memory, bytes (the bucket's maximum when aggregated).
    pub maestro_rss_bytes: Option<f64>,
    /// The agent's CPU, percent of one core (raw samples only).
    pub agent_cpu_pct: Option<f64>,
    /// The agent's resident memory, bytes (raw samples only).
    pub agent_rss_bytes: Option<f64>,
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
    /// The guest's CPU, percent (raw samples only).
    pub guest_cpu_pct: Option<f64>,
    /// The guest's available memory, bytes (raw samples only).
    pub guest_mem_available_bytes: Option<f64>,
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
    /// A worker's host samples in `[from, to)`, from the raw table or an aggregate. `None` when
    /// the worker doesn't exist in the project.
    #[expect(
        clippy::too_many_lines,
        reason = "one SQL query per resolution, each listing the same host columns"
    )]
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
                            maestro_cpu_pct::float8 AS maestro_cpu_pct,
                            maestro_rss_bytes::float8 AS maestro_rss_bytes,
                            agent_cpu_pct::float8 AS agent_cpu_pct,
                            agent_rss_bytes::float8 AS agent_rss_bytes,
                            host_load_1m::float8 AS host_load_1m,
                            host_cpu_cores::float8 AS host_cpu_cores,
                            host_swap_used_bytes::float8 AS host_swap_used_bytes,
                            host_swap_total_bytes::float8 AS host_swap_total_bytes,
                            host_psi_cpu_pct::float8 AS host_psi_cpu_pct,
                            host_psi_memory_pct::float8 AS host_psi_memory_pct,
                            host_psi_io_pct::float8 AS host_psi_io_pct
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
                            avg(maestro_cpu_avg)::float8 AS maestro_cpu_pct,
                            max(maestro_rss_max)::float8 AS maestro_rss_bytes,
                            NULL::float8 AS agent_cpu_pct, NULL::float8 AS agent_rss_bytes,
                            avg(host_load_1m_avg)::float8 AS host_load_1m,
                            NULL::float8 AS host_cpu_cores,
                            avg(host_swap_used_avg)::float8 AS host_swap_used_bytes,
                            NULL::float8 AS host_swap_total_bytes,
                            avg(host_psi_cpu_pct_avg)::float8 AS host_psi_cpu_pct,
                            avg(host_psi_memory_pct_avg)::float8 AS host_psi_memory_pct,
                            avg(host_psi_io_pct_avg)::float8 AS host_psi_io_pct
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
                            avg(maestro_cpu_avg)::float8 AS maestro_cpu_pct,
                            max(maestro_rss_max)::float8 AS maestro_rss_bytes,
                            NULL::float8 AS agent_cpu_pct, NULL::float8 AS agent_rss_bytes,
                            avg(host_load_1m_avg)::float8 AS host_load_1m,
                            NULL::float8 AS host_cpu_cores,
                            avg(host_swap_used_avg)::float8 AS host_swap_used_bytes,
                            NULL::float8 AS host_swap_total_bytes,
                            avg(host_psi_cpu_pct_avg)::float8 AS host_psi_cpu_pct,
                            avg(host_psi_memory_pct_avg)::float8 AS host_psi_memory_pct,
                            avg(host_psi_io_pct_avg)::float8 AS host_psi_io_pct
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
