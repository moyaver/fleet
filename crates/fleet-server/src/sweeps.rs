// SPDX-License-Identifier: AGPL-3.0-only
//! Background work (PLAN §7.7).
//!
//! Every instance, once a second: recompute liveness (pushing crossings to browsers) and send
//! deltas that waited too long for a predecessor. Every `liveness.sweep_interval_ms`: persist
//! its connected runs' last contact; the leader (a Postgres advisory lock, so one instance at a
//! time) then detaches silent runs and abandons runs whose resume grace expired. Hourly, the
//! leader creates partitions ahead, drops expired credentials and expired time-based data; it
//! also starts the retention jobs (rollups, artifact expiry and blob collection) on their own
//! schedules, beside this loop ([`crate::retention`]).

use std::time::Duration;

use fleet_store::sweep::Leader;
use tokio::time::{Instant, MissedTickBehavior};

use crate::state::AppState;

pub(crate) async fn run(state: AppState) {
    let mut second = tokio::time::interval(Duration::from_secs(1));
    second.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let sweep_every = Duration::from_millis(state.config.liveness.sweep_interval_ms);
    let mut next_sweep = Instant::now() + sweep_every;
    let mut next_hourly = Instant::now();
    let mut leader: Option<Leader> = None;
    let mut retention = crate::retention::Schedule::new(&state.config.retention);
    loop {
        tokio::select! {
            _ = second.tick() => {}
            () = state.shutdown.cancelled() => return,
        }
        state.hub.tick();
        state.hub.flush_stale(&state.hub.attempt_projects());
        if Instant::now() < next_sweep {
            continue;
        }
        next_sweep = Instant::now() + sweep_every;
        crate::artifacts::purge_spool(&state).await;
        sweep(&state, &mut leader).await;
        if leader.is_some() {
            retention.tick(&state);
        }
        if leader.is_some() && Instant::now() >= next_hourly {
            next_hourly = Instant::now() + Duration::from_secs(3600);
            if let Err(e) = state.store.ensure_partitions(1, 7).await {
                tracing::warn!(error = %e, "creating partitions");
            }
            if let Err(e) = state.store.cleanup_credentials().await {
                tracing::warn!(error = %e, "removing expired credentials");
            }
        }
    }
}

async fn sweep(state: &AppState, leader: &mut Option<Leader>) {
    if let Err(e) = state.store.persist_last_seen(&state.hub.tick()).await {
        tracing::warn!(error = %e, "persisting last contact");
        return;
    }
    if let Some(l) = leader.as_mut()
        && !l.alive().await
    {
        tracing::warn!("lost the leader session");
        *leader = None;
    }
    if leader.is_none() {
        match state.store.try_lead().await {
            Ok(Some(l)) => {
                tracing::info!("this instance leads the background sweeps");
                *leader = Some(l);
            }
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(error = %e, "trying to lead");
                return;
            }
        }
    }
    let l = &state.config.liveness;
    let i = &state.config.ingest;
    // Silent: well past the idle timeout plus one persist cycle, so a live socket is never hit.
    let silent = Duration::from_millis(i.idle_timeout_ms + 2 * l.sweep_interval_ms + 30_000);
    match state
        .store
        .sweep(silent, Duration::from_millis(l.resume_grace_ms))
        .await
    {
        Ok(report) => {
            if report.detached + report.abandoned > 0 {
                tracing::info!(
                    detached = report.detached,
                    abandoned = report.abandoned,
                    "liveness sweep"
                );
            }
            for change in report.changes {
                state.publish(Some(change));
            }
        }
        Err(e) => tracing::warn!(error = %e, "liveness sweep"),
    }
}
