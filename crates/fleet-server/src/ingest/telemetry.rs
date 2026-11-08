// SPDX-License-Identifier: AGPL-3.0-only
//! Telemetry checkpoints (research 05 §3.2, §10.2): samples are buffered in memory and written
//! every `ingest.telemetry_flush_ms` (10–30 s) in one statement; each connection then gets
//! `telemetry_ack{through}`. Heartbeats never touch the database.

use std::{sync::Mutex, time::Duration};

use fleet_protocol::frame::Telemetry;
use fleet_store::{Store, ingest::RunHandle, telemetry::TelemetryBatch};
use tokio::sync::mpsc;

use crate::{live::Hub, state::AppState};

/// Buffered samples beyond this trigger an early flush.
const EARLY_FLUSH_SAMPLES: usize = 50_000;

/// Telemetry waiting for the next checkpoint, with where each ack goes.
#[derive(Debug, Default)]
pub(crate) struct TelemetryBuffer {
    pending: Mutex<Vec<(TelemetryBatch, mpsc::Sender<u64>)>>,
    /// One flush at a time, so acks never go backwards.
    flushing: tokio::sync::Mutex<()>,
    wake: tokio::sync::Notify,
}

impl TelemetryBuffer {
    /// Queues a frame; its ack goes to `acks` after the flush commits.
    pub(crate) fn push(&self, run: &RunHandle, batch: Telemetry, acks: mpsc::Sender<u64>) {
        let samples = {
            let Ok(mut p) = self.pending.lock() else {
                return;
            };
            p.push((
                TelemetryBatch {
                    agent_run_id: run.agent_run_id,
                    project_id: run.project_id,
                    worker_id: run.worker_id,
                    batch,
                },
                acks,
            ));
            p.iter().map(|(b, _)| b.batch.samples.len()).sum::<usize>()
        };
        if samples > EARLY_FLUSH_SAMPLES {
            self.wake.notify_one();
        }
    }

    /// Writes everything buffered and sends the acks. On failure the samples stay queued for
    /// the next checkpoint (telemetry may lag, but is never acked before it is stored).
    pub(crate) async fn flush(&self, store: &Store, hub: &Hub) {
        let _one = self.flushing.lock().await;
        let taken = match self.pending.lock() {
            Ok(mut p) => std::mem::take(&mut *p),
            Err(_) => return,
        };
        if taken.is_empty() {
            return;
        }
        let batches: Vec<TelemetryBatch> = taken.iter().map(|(b, _)| b.clone()).collect();
        match store.store_telemetry(&batches).await {
            Ok(through) => {
                for project in batches
                    .iter()
                    .filter(|b| b.batch.has_samples())
                    .map(|b| b.project_id)
                {
                    hub.publish_samples(project);
                }
                for (b, acks) in &taken {
                    if let Some((_, t)) = through.iter().find(|(r, _)| *r == b.agent_run_id) {
                        // A closed connection just doesn't get its ack; the agent resends.
                        let _ = acks.try_send(*t);
                    }
                }
                tracing::debug!(batches = batches.len(), "telemetry checkpoint written");
            }
            Err(e) => {
                tracing::warn!(error = %e, "telemetry checkpoint failed; retrying next time");
                if let Ok(mut p) = self.pending.lock() {
                    let mut back = taken;
                    back.append(&mut p);
                    *p = back;
                }
            }
        }
    }
}

/// The checkpoint loop; does a final flush on shutdown.
pub(crate) async fn flusher(state: AppState) {
    let period = Duration::from_millis(state.config.ingest.telemetry_flush_ms);
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = state.telemetry.wake.notified() => {}
            () = state.shutdown.cancelled() => break,
        }
        state.telemetry.flush(&state.store, &state.hub).await;
    }
    state.telemetry.flush(&state.store, &state.hub).await;
}
