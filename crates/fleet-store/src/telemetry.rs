// SPDX-License-Identifier: AGPL-3.0-only
//! Metric samples (research 05 §3.2, §10.2).
//!
//! Telemetry is a separate stream with gaps allowed. The server buffers batches and flushes them
//! here every 10–30 s in one transaction, then sends `telemetry_ack`. Replays are harmless (the
//! primary key is `(agent_run_id, ts)`), and every loss is recorded: samples the agent dropped
//! (`dropped_since_last`) and samples whose clock is too far off to store.

use fleet_protocol::{event::AdbState, frame::Telemetry};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Result, Store};

/// One buffered telemetry frame and whose it is.
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryBatch {
    /// The agent run.
    pub agent_run_id: Uuid,
    /// Its project.
    pub project_id: Uuid,
    /// Its worker.
    pub worker_id: Uuid,
    /// The frame.
    pub batch: Telemetry,
}

/// Samples more than this far from the server clock are not stored (no DEFAULT partition).
const MAX_SKEW: time::Duration = time::Duration::hours(24);

impl Store {
    /// Stores buffered telemetry. Returns, per agent run, the highest `tseq` now stored.
    pub async fn store_telemetry(&self, batches: &[TelemetryBatch]) -> Result<Vec<(Uuid, u64)>> {
        let now = OffsetDateTime::now_utc();
        let mut tx = self.pool().begin().await?;
        let mut cols = Columns::default();
        let mut devices = DeviceColumns::default();
        let mut through: Vec<(Uuid, u64)> = Vec::new();
        for b in batches {
            let t = &b.batch;
            if t.dropped_since_last > 0 {
                let to = t.tseq_from.saturating_sub(1);
                let from = t.tseq_from.saturating_sub(u64::from(t.dropped_since_last));
                gap(&mut tx, b.agent_run_id, from, to, "agent_dropped").await?;
            }
            let mut skewed: Option<(u64, u64)> = None;
            for (i, s) in t.samples.iter().enumerate() {
                let tseq = t
                    .tseq_from
                    .saturating_add(u64::try_from(i).unwrap_or(u64::MAX));
                if (s.at.wall - now).abs() > MAX_SKEW {
                    skewed = Some(skewed.map_or((tseq, tseq), |(a, _)| (a, tseq)));
                    continue;
                }
                cols.push(b, tseq, s);
            }
            if let Some((from, to)) = skewed {
                gap(&mut tx, b.agent_run_id, from, to, "clock_skew").await?;
            }
            for d in &t.devices {
                // Device rows have no tseq of their own, so a skewed one is dropped, not recorded
                // as a gap (the gap ranges count the host samples of the frame).
                if (d.at.wall - now).abs() <= MAX_SKEW {
                    devices.push(b, d);
                }
            }
            match through.iter_mut().find(|(r, _)| *r == b.agent_run_id) {
                Some((_, t0)) => *t0 = (*t0).max(t.tseq_to),
                None => through.push((b.agent_run_id, t.tseq_to)),
            }
        }
        cols.insert(&mut tx).await?;
        devices.insert(&mut tx).await?;
        for (run, t) in &through {
            sqlx::query!(
                "UPDATE agent_runs SET telemetry_committed = greatest(telemetry_committed, $2)
                  WHERE id = $1",
                run,
                i64::try_from(*t).unwrap_or(i64::MAX)
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(through)
    }
}

async fn gap(
    tx: &mut sqlx::PgConnection,
    run: Uuid,
    from: u64,
    to: u64,
    reason: &str,
) -> Result<(), sqlx::Error> {
    if from == 0 || from > to {
        return Ok(());
    }
    sqlx::query!(
        "INSERT INTO ingest_gaps (agent_run_id, stream, from_seq, to_seq, reason)
         VALUES ($1, 'telemetry', $2, $3, $4) ON CONFLICT DO NOTHING",
        run,
        i64::try_from(from).unwrap_or(i64::MAX),
        i64::try_from(to).unwrap_or(i64::MAX),
        reason
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}

#[derive(Default)]
struct Columns {
    ts: Vec<OffsetDateTime>,
    run: Vec<Uuid>,
    worker: Vec<Uuid>,
    project: Vec<Uuid>,
    tseq: Vec<i64>,
    host_cpu: Vec<Option<f32>>,
    host_used: Vec<Option<i64>>,
    host_total: Vec<Option<i64>>,
    maestro_cpu: Vec<Option<f32>>,
    maestro_rss: Vec<Option<i64>>,
    agent_cpu: Vec<Option<f32>>,
    agent_rss: Vec<Option<i64>>,
    load_1m: Vec<Option<f32>>,
    cpu_cores: Vec<Option<i32>>,
    swap_used: Vec<Option<i64>>,
    swap_total: Vec<Option<i64>>,
    psi_cpu: Vec<Option<f32>>,
    psi_memory: Vec<Option<f32>>,
    psi_io: Vec<Option<f32>>,
}

/// adb's state as the `device_samples.adb_state` code: 0 device, 1 offline, 2 unauthorized,
/// 3 absent, 4 another state. NULL means adb did not report one.
fn adb_code(state: AdbState) -> i16 {
    match state {
        AdbState::Device => 0,
        AdbState::Offline => 1,
        AdbState::Unauthorized => 2,
        AdbState::Absent => 3,
        AdbState::Other => 4,
    }
}

fn opt_i64(v: Option<u64>) -> Option<i64> {
    v.and_then(|v| i64::try_from(v).ok())
}

fn opt_i32(v: Option<u32>) -> Option<i32> {
    v.and_then(|v| i32::try_from(v).ok())
}

/// One device sample per row of `device_samples`, for an insert with `unnest`.
#[derive(Default)]
struct DeviceColumns {
    ts: Vec<OffsetDateTime>,
    session: Vec<Uuid>,
    run: Vec<Uuid>,
    project: Vec<Uuid>,
    adb_state: Vec<Option<i16>>,
    boot_completed: Vec<Option<bool>>,
    guest_cpu: Vec<Option<f32>>,
    guest_mem: Vec<Option<i64>>,
}

impl DeviceColumns {
    fn push(&mut self, b: &TelemetryBatch, d: &fleet_protocol::frame::DeviceSample) {
        self.ts.push(d.at.wall);
        self.session.push(d.device_session_id);
        self.run.push(b.agent_run_id);
        self.project.push(b.project_id);
        self.adb_state.push(d.adb_state.map(adb_code));
        self.boot_completed.push(d.boot_completed);
        self.guest_cpu.push(d.guest_cpu_pct);
        self.guest_mem.push(opt_i64(d.guest_mem_available_bytes));
    }

    async fn insert(&self, tx: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
        if self.ts.is_empty() {
            return Ok(());
        }
        sqlx::query!(
            "INSERT INTO device_samples (ts, device_session_id, agent_run_id, project_id,
                                         adb_state, boot_completed, guest_cpu_pct,
                                         guest_mem_available_bytes)
             SELECT * FROM unnest($1::timestamptz[], $2::uuid[], $3::uuid[], $4::uuid[],
                                  $5::smallint[], $6::boolean[], $7::real[], $8::bigint[])
             ON CONFLICT DO NOTHING",
            &self.ts,
            &self.session,
            &self.run,
            &self.project,
            &self.adb_state as &[Option<i16>],
            &self.boot_completed as &[Option<bool>],
            &self.guest_cpu as &[Option<f32>],
            &self.guest_mem as &[Option<i64>]
        )
        .execute(&mut *tx)
        .await?;
        Ok(())
    }
}

impl Columns {
    fn push(&mut self, b: &TelemetryBatch, tseq: u64, s: &fleet_protocol::frame::Sample) {
        self.ts.push(s.at.wall);
        self.run.push(b.agent_run_id);
        self.worker.push(b.worker_id);
        self.project.push(b.project_id);
        self.tseq.push(i64::try_from(tseq).unwrap_or(i64::MAX));
        self.host_cpu.push(s.host_cpu_pct);
        self.host_used.push(opt_i64(s.host_mem_used_bytes));
        self.host_total.push(opt_i64(s.host_mem_total_bytes));
        self.maestro_cpu.push(s.maestro_cpu_pct);
        self.maestro_rss.push(opt_i64(s.maestro_rss_bytes));
        self.agent_cpu.push(s.agent_cpu_pct);
        self.agent_rss.push(opt_i64(s.agent_rss_bytes));
        self.load_1m.push(s.host_load_1m);
        self.cpu_cores.push(opt_i32(s.host_cpu_cores));
        self.swap_used.push(opt_i64(s.host_swap_used_bytes));
        self.swap_total.push(opt_i64(s.host_swap_total_bytes));
        self.psi_cpu.push(s.host_psi_cpu_pct);
        self.psi_memory.push(s.host_psi_memory_pct);
        self.psi_io.push(s.host_psi_io_pct);
    }

    async fn insert(&self, tx: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
        if self.ts.is_empty() {
            return Ok(());
        }
        sqlx::query!(
            "INSERT INTO host_samples (ts, agent_run_id, worker_id, project_id, tseq, host_cpu_pct,
                                       host_mem_used_bytes, host_mem_total_bytes, maestro_cpu_pct,
                                       maestro_rss_bytes, agent_cpu_pct, agent_rss_bytes,
                                       host_load_1m, host_cpu_cores, host_swap_used_bytes,
                                       host_swap_total_bytes, host_psi_cpu_pct,
                                       host_psi_memory_pct, host_psi_io_pct)
             SELECT * FROM unnest($1::timestamptz[], $2::uuid[], $3::uuid[], $4::uuid[],
                                  $5::bigint[], $6::real[], $7::bigint[], $8::bigint[],
                                  $9::real[], $10::bigint[], $11::real[], $12::bigint[],
                                  $13::real[], $14::integer[], $15::bigint[], $16::bigint[],
                                  $17::real[], $18::real[], $19::real[])
             ON CONFLICT DO NOTHING",
            &self.ts,
            &self.run,
            &self.worker,
            &self.project,
            &self.tseq,
            &self.host_cpu as &[Option<f32>],
            &self.host_used as &[Option<i64>],
            &self.host_total as &[Option<i64>],
            &self.maestro_cpu as &[Option<f32>],
            &self.maestro_rss as &[Option<i64>],
            &self.agent_cpu as &[Option<f32>],
            &self.agent_rss as &[Option<i64>],
            &self.load_1m as &[Option<f32>],
            &self.cpu_cores as &[Option<i32>],
            &self.swap_used as &[Option<i64>],
            &self.swap_total as &[Option<i64>],
            &self.psi_cpu as &[Option<f32>],
            &self.psi_memory as &[Option<f32>],
            &self.psi_io as &[Option<f32>]
        )
        .execute(&mut *tx)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use fleet_protocol::{frame::DeviceSample, journal::ClockSample};
    use sqlx::PgPool;

    use super::*;

    fn device(wall: OffsetDateTime, session: Uuid) -> DeviceSample {
        DeviceSample {
            at: ClockSample { wall, mono_ms: 0 },
            device_session_id: session,
            adb_state: None,
            boot_completed: None,
            guest_cpu_pct: None,
            guest_mem_available_bytes: None,
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn device_samples_keep_adbs_state_and_leave_unknown_readings_null(pool: PgPool) {
        let store = Store {
            pool,
            origin: Uuid::now_v7(),
        };
        let project = store.upsert_project("app", "App").await.unwrap();
        let worker = sqlx::query_scalar!(
            "INSERT INTO workers (project_id, identity_key) VALUES ($1, 'runner-a') RETURNING id",
            project
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        let (run, session) = (Uuid::now_v7(), Uuid::now_v7());
        let now = OffsetDateTime::now_utc();
        let mut answered = device(now - time::Duration::seconds(60), session);
        answered.adb_state = Some(AdbState::Device);
        answered.boot_completed = Some(true);
        answered.guest_cpu_pct = Some(12.5);
        answered.guest_mem_available_bytes = Some(2 << 30);
        let mut unreachable = device(now - time::Duration::seconds(30), session);
        unreachable.adb_state = Some(AdbState::Offline);
        // A clock two days off is not stored: there is no partition for it.
        let skewed = device(now - time::Duration::days(2), session);
        let batch = TelemetryBatch {
            agent_run_id: run,
            project_id: project,
            worker_id: worker,
            batch: Telemetry {
                tseq_from: 1,
                tseq_to: 1,
                dropped_since_last: 0,
                samples: Vec::new(),
                devices: vec![answered, unreachable, skewed],
            },
        };

        store.store_telemetry(&[batch]).await.unwrap();

        let rows = sqlx::query!(
            "SELECT adb_state, boot_completed, guest_cpu_pct, guest_mem_available_bytes
               FROM device_samples WHERE device_session_id = $1 ORDER BY ts",
            session
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        let rows: Vec<_> = rows
            .into_iter()
            .map(|r| {
                (
                    r.adb_state,
                    r.boot_completed,
                    r.guest_cpu_pct,
                    r.guest_mem_available_bytes,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                (Some(0), Some(true), Some(12.5), Some(2_i64 << 30)),
                (Some(1), None, None, None),
            ],
            "device, offline; the unknowns are NULL, never zero"
        );
    }
}
