// SPDX-License-Identifier: AGPL-3.0-only
//! Retention jobs (PLAN §7.7; issue #45), run by the leader.
//!
//! - **Rollups** every `retention.rollup_interval_seconds`: one-minute and one-hour aggregates.
//! - **Garbage collection** every `retention.gc_interval_minutes`: artifacts past their
//!   retention expire, blobs nobody refers to are deleted after a grace period, and the temp
//!   files crashed uploads left behind are removed.
//! - **Expiry of time-based data** hourly: old partitions are dropped, old rollups and audit
//!   events deleted.
//!
//! The functions take the time as a parameter (the schedule passes the wall clock, tests pass
//! what they like) and log instead of failing: a job that cannot run is retried at its next turn,
//! and never disturbs ingest. All of them are idempotent, so a brief overlap of two leaders, or a
//! job that outlives its leadership, does no harm.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use fleet_blob::{BlobKey, BlobStore, Sha256Digest};
use fleet_store::{Store, gc::GarbageBlob};
use time::OffsetDateTime;
use tokio::time::Instant;

use crate::{config::Retention, state::AppState};

/// Blobs claimed per round.
const GC_BATCH: i64 = 100;

/// Rolls up telemetry.
pub(crate) async fn roll_up(store: &Store, now: OffsetDateTime) {
    match store.roll_up(now).await {
        Ok(r) if r.minutes + r.hours > 0 => tracing::info!(
            minute_rows = r.minutes,
            hour_rows = r.hours,
            late_buckets = r.late_buckets,
            "telemetry rolled up"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "rolling up telemetry"),
    }
}

/// Drops partitions and rows that outlived their retention.
pub(crate) async fn drop_expired(store: &Store, config: &Retention, now: OffsetDateTime) {
    match store.drop_expired(now, &config.policy()).await {
        Ok(r) => tracing::info!(
            partitions_dropped = r.partitions,
            partitions_held_back = r.held_back,
            rollup_rows_deleted = r.rollup_rows,
            audit_events_deleted = r.audit_events,
            "expired data dropped"
        ),
        Err(e) => tracing::warn!(error = %e, "dropping expired data"),
    }
}

/// Expires artifacts, deletes the objects of blobs nobody refers to, and sweeps stale temp files.
pub(crate) async fn collect_garbage(
    store: &Store,
    blobs: &dyn BlobStore,
    config: &Retention,
    now: OffsetDateTime,
) {
    let tombstones = (config.expired_artifact_days > 0)
        .then(|| Duration::from_secs(u64::from(config.expired_artifact_days) * 86_400));
    match store.expire_artifacts(now, tombstones).await {
        Ok(e) => {
            if e.expired + e.abandoned + e.tombstones_deleted > 0 {
                tracing::info!(
                    expired = e.expired,
                    abandoned = e.abandoned,
                    unreferenced_blobs = e.unreferenced_blobs,
                    tombstones_deleted = e.tombstones_deleted,
                    "artifacts expired"
                );
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "expiring artifacts");
            return;
        }
    }
    let grace = Duration::from_secs(u64::from(config.blob_grace_hours) * 3600);
    let mut deleted = 0_u64;
    loop {
        let batch = match store.claim_garbage_blobs(now, grace, GC_BATCH).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "claiming blobs to delete");
                break;
            }
        };
        let claimed = batch.len();
        let mut failed = false;
        for blob in batch {
            match delete_blob(store, blobs, &blob).await {
                Ok(()) => deleted += 1,
                Err(error) => {
                    // Still `deleting` in the database: the next round retries it.
                    tracing::warn!(project_id = %blob.project_id, sha256 = blob.sha256, error,
                        "deleting a blob");
                    failed = true;
                }
            }
        }
        // A failed delete would be claimed again at once: stop and let the next round retry.
        if failed || i64::try_from(claimed).unwrap_or(i64::MAX) < GC_BATCH {
            break;
        }
    }
    if deleted > 0 {
        tracing::info!(deleted, "blobs deleted");
    }
    let stale = now - time::Duration::hours(i64::from(config.stale_upload_hours));
    match blobs.sweep_stale_uploads(stale).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(removed = n, "stale upload files removed"),
        Err(e) => tracing::warn!(error = %e, "sweeping stale upload files"),
    }
}

async fn delete_blob(
    store: &Store,
    blobs: &dyn BlobStore,
    blob: &GarbageBlob,
) -> Result<(), String> {
    let digest = Sha256Digest::from_hex(&blob.sha256).ok_or("the stored digest is not hex")?;
    blobs
        .delete(&BlobKey::new(blob.project_id, digest))
        .await
        .map_err(|e| e.to_string())?;
    store.forget_blob(blob).await.map_err(|e| e.to_string())
}

/// One recurring job: due at `next`, never two at once.
#[derive(Debug)]
struct Job {
    every: Duration,
    next: Instant,
    running: Arc<AtomicBool>,
}

impl Job {
    fn new(every: Duration) -> Self {
        Self {
            every,
            next: Instant::now(),
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A guard if the job is due and its last run is over.
    fn due(&mut self) -> Option<Running> {
        if Instant::now() < self.next || self.running.swap(true, Ordering::AcqRel) {
            return None;
        }
        self.next = Instant::now() + self.every;
        Some(Running(Arc::clone(&self.running)))
    }
}

/// Marks a job as finished when dropped, however it ends.
struct Running(Arc<AtomicBool>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The leader's retention schedule.
#[derive(Debug)]
pub(crate) struct Schedule {
    rollups: Job,
    garbage: Job,
    expiry: Job,
}

impl Schedule {
    pub(crate) fn new(config: &Retention) -> Self {
        Self {
            rollups: Job::new(Duration::from_secs(config.rollup_interval_seconds)),
            garbage: Job::new(Duration::from_secs(config.gc_interval_minutes * 60)),
            expiry: Job::new(Duration::from_secs(3600)),
        }
    }

    /// Starts every job that is due. Call it only while leading. Jobs run beside the liveness
    /// loop, on the server's task tracker, so a slow one never delays liveness.
    pub(crate) fn tick(&mut self, state: &AppState) {
        if let Some(guard) = self.rollups.due() {
            let store = state.store.clone();
            state.tracker.spawn(async move {
                let _guard = guard;
                roll_up(&store, OffsetDateTime::now_utc()).await;
            });
        }
        if let Some(guard) = self.garbage.due() {
            let state = state.clone();
            state.clone().tracker.spawn(async move {
                let _guard = guard;
                match state.blobs().await {
                    Ok(blobs) => {
                        collect_garbage(
                            &state.store,
                            blobs.as_ref(),
                            &state.config.retention,
                            OffsetDateTime::now_utc(),
                        )
                        .await;
                    }
                    Err(e) => tracing::warn!(error = format!("{e:#}"), "no blob store to collect"),
                }
            });
        }
        if let Some(guard) = self.expiry.due() {
            let state = state.clone();
            state.clone().tracker.spawn(async move {
                let _guard = guard;
                drop_expired(
                    &state.store,
                    &state.config.retention,
                    OffsetDateTime::now_utc(),
                )
                .await;
            });
        }
    }
}
