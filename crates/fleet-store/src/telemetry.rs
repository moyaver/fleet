// SPDX-License-Identifier: AGPL-3.0-only
//! Metric samples (research 05 §3.2, §10.2).
//!
//! Telemetry is a separate stream with gaps allowed. The server buffers batches and flushes them
//! here every 10–30 s in one transaction, then sends `telemetry_ack`. Replays are harmless (the
//! primary key is `(agent_run_id, ts)`), and every loss is recorded: samples the agent dropped
//! (`dropped_since_last`) and samples whose clock is too far off to store.

use fleet_protocol::frame::Telemetry;
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
            match through.iter_mut().find(|(r, _)| *r == b.agent_run_id) {
                Some((_, t0)) => *t0 = (*t0).max(t.tseq_to),
                None => through.push((b.agent_run_id, t.tseq_to)),
            }
        }
        cols.insert(&mut tx).await?;
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
}

fn opt_i64(v: Option<u64>) -> Option<i64> {
    v.and_then(|v| i64::try_from(v).ok())
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
    }

    async fn insert(&self, tx: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
        if self.ts.is_empty() {
            return Ok(());
        }
        sqlx::query!(
            "INSERT INTO host_samples (ts, agent_run_id, worker_id, project_id, tseq, host_cpu_pct,
                                       host_mem_used_bytes, host_mem_total_bytes, maestro_cpu_pct,
                                       maestro_rss_bytes, agent_cpu_pct, agent_rss_bytes)
             SELECT * FROM unnest($1::timestamptz[], $2::uuid[], $3::uuid[], $4::uuid[],
                                  $5::bigint[], $6::real[], $7::bigint[], $8::bigint[],
                                  $9::real[], $10::bigint[], $11::real[], $12::bigint[])
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
            &self.agent_rss as &[Option<i64>]
        )
        .execute(&mut *tx)
        .await?;
        Ok(())
    }
}
