// SPDX-License-Identifier: AGPL-3.0-only
//! The live hub (PLAN §7.3; research 03 §5).
//!
//! Every committed change bumps its pipeline attempt's `version` in Postgres, in the same
//! transaction. After the ack, the hub loads the changed entities and broadcasts a delta on the
//! project's channel, **in version order per attempt**: deltas committed by different sockets
//! can finish out of order, so the hub holds a later version until the earlier one is out (or,
//! after a short wait, sends it anyway and lets clients see the gap and resync).
//!
//! Snapshots are read from Postgres together with their version, so the hub holds no state that
//! a restart could lose: after a restart it reloads the open attempts' versions and every run's
//! liveness starts as `stale` (research 03 §5.4) until its agent reconnects.
//!
//! Liveness (`online` → `stale` → `disconnected` → `completed`) is in memory for the runs whose
//! socket this instance holds: heartbeats never touch the database. Crossings are pushed as
//! `liveness` frames. Every other run is judged from its persisted contact, so instances agree.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use fleet_protocol::live::{EntityView, LiveFrame, Liveness, ResyncReason, Snapshot};
use fleet_store::{
    Store,
    ingest::{Change, RunHandle},
    live::{LiveNotice, Notices},
};
use time::OffsetDateTime;
use tokio::{sync::broadcast, time::Instant};
use uuid::Uuid;

use crate::state::AppState;

/// How long a later version waits for an earlier one before it is sent anyway.
const REORDER_WAIT: Duration = Duration::from_secs(2);
/// Keep idle order state longer than the run liveness records.
const ORDER_RETENTION: Duration = Duration::from_secs(3600).saturating_add(REORDER_WAIT);

#[derive(Debug)]
struct Order {
    published: u64,
    last_publish: Instant,
    pending: BTreeMap<u64, (Instant, Arc<str>)>,
}

#[derive(Debug, Clone, Copy)]
struct RunLive {
    project: Uuid,
    attempt: Uuid,
    epoch: u32,
    last_seen: Instant,
    last_seen_wall: OffsetDateTime,
    connected: bool,
    ended: bool,
    shown: Liveness,
}

/// See the module docs.
#[derive(Debug)]
pub(crate) struct Hub {
    channels: Mutex<HashMap<Uuid, broadcast::Sender<Arc<str>>>>,
    order: Mutex<HashMap<Uuid, Order>>,
    runs: Mutex<HashMap<Uuid, RunLive>>,
    stale_after: Duration,
    /// [`Self::stale_after`] plus one sweep interval: the threshold for persisted contact.
    persisted_stale_after: time::Duration,
    /// When this instance started. Contact persisted before that does not make a run live.
    booted: OffsetDateTime,
    capacity: usize,
    #[cfg(test)]
    pub(crate) fail_entity_load: std::sync::atomic::AtomicBool,
}

fn json(frame: &LiveFrame) -> Arc<str> {
    serde_json::to_string(frame).unwrap_or_default().into()
}

impl Hub {
    /// `sweep_interval` is how often each instance persists its runs' contact.
    pub(crate) fn new(stale_after: Duration, sweep_interval: Duration, capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            #[cfg(test)]
            fail_entity_load: std::sync::atomic::AtomicBool::new(false),
            channels: Mutex::new(HashMap::new()),
            order: Mutex::new(HashMap::new()),
            runs: Mutex::new(HashMap::new()),
            stale_after,
            persisted_stale_after: time::Duration::try_from(
                stale_after.saturating_add(sweep_interval),
            )
            .unwrap_or(time::Duration::MAX),
            booted: OffsetDateTime::now_utc(),
        }
    }

    /// A project's channel. Subscribe before reading a snapshot, so nothing falls in between.
    pub(crate) fn subscribe(&self, project: Uuid) -> broadcast::Receiver<Arc<str>> {
        self.sender(project).subscribe()
    }

    fn sender(&self, project: Uuid) -> broadcast::Sender<Arc<str>> {
        let mut c = self
            .channels
            .lock()
            .expect("hub lock should not be poisoned: no code panics while holding it");
        c.entry(project)
            .or_insert_with(|| broadcast::channel(self.capacity).0)
            .clone()
    }

    /// Restores attempt versions after a restart, so the first delta continues the sequence.
    pub(crate) async fn rebuild(&self, store: &Store) -> fleet_store::Result<usize> {
        let open = store.open_attempt_versions().await?;
        let mut order = self
            .order
            .lock()
            .expect("hub lock should not be poisoned: no code panics while holding it");
        for (attempt, _project, version) in &open {
            order.insert(
                *attempt,
                Order {
                    published: *version,
                    last_publish: Instant::now(),
                    pending: BTreeMap::new(),
                },
            );
        }
        Ok(open.len())
    }

    /// Broadcasts a committed change, in version order per attempt.
    pub(crate) async fn publish(&self, store: &Store, change: Change) {
        let frame = if change.entities.is_empty() {
            json(&LiveFrame::Resync {
                reason: ResyncReason::Lagged,
            })
        } else {
            #[cfg(test)]
            let load = if self
                .fail_entity_load
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                Err(fleet_store::Error::Corrupt(
                    "injected entity load failure".into(),
                ))
            } else {
                store.entities(change.project_id, &change.entities).await
            };
            #[cfg(not(test))]
            let load = store.entities(change.project_id, &change.entities).await;
            match load {
                Ok(mut entities) => {
                    self.overlay(&mut entities);
                    json(&LiveFrame::Delta {
                        project_id: change.project_id,
                        attempt_id: change.pipeline_attempt_id,
                        version: change.version,
                        entities,
                    })
                }
                Err(err) => {
                    tracing::warn!(error = %err, "loading a delta; subscribers will resync");
                    json(&LiveFrame::Resync {
                        reason: ResyncReason::Lagged,
                    })
                }
            }
        };
        let ready = {
            let mut order = self
                .order
                .lock()
                .expect("hub lock should not be poisoned: no code panics while holding it");
            let o = order
                .entry(change.pipeline_attempt_id)
                .or_insert_with(|| Order {
                    published: change.version.saturating_sub(1),
                    last_publish: Instant::now(),
                    pending: BTreeMap::new(),
                });
            if change.version <= o.published {
                return;
            }
            o.last_publish = Instant::now();
            o.pending.insert(change.version, (Instant::now(), frame));
            drain(o, false)
        };
        self.send(change.project_id, ready);
    }

    fn send(&self, project: Uuid, frames: Vec<Arc<str>>) {
        if frames.is_empty() {
            return;
        }
        let tx = self.sender(project);
        for f in frames {
            // No subscribers is fine.
            let _ = tx.send(f);
        }
    }

    /// Sends versions that waited too long for a predecessor (the gap makes clients resync).
    pub(crate) fn flush_stale(&self, projects: &HashMap<Uuid, Uuid>) {
        let mut out: Vec<(Uuid, Vec<Arc<str>>)> = Vec::new();
        {
            let mut order = self
                .order
                .lock()
                .expect("hub lock should not be poisoned: no code panics while holding it");
            for (attempt, o) in order.iter_mut() {
                let late = o
                    .pending
                    .first_key_value()
                    .is_some_and(|(_, (at, _))| at.elapsed() > REORDER_WAIT);
                if late && let Some(project) = projects.get(attempt) {
                    out.push((*project, drain(o, true)));
                }
            }
            order
                .retain(|_, o| !o.pending.is_empty() || o.last_publish.elapsed() < ORDER_RETENTION);
        }
        for (project, frames) in out {
            self.send(project, frames);
        }
    }

    /// Asks every subscriber to re-snapshot: this instance may have missed changes made elsewhere.
    pub(crate) fn resync_all(&self) {
        let frame = json(&LiveFrame::Resync {
            reason: ResyncReason::Lagged,
        });
        let channels: Vec<_> = self
            .channels
            .lock()
            .expect("hub lock should not be poisoned: no code panics while holding it")
            .values()
            .cloned()
            .collect();
        for tx in channels {
            // No subscribers is fine.
            let _ = tx.send(Arc::clone(&frame));
        }
    }

    /// Known attempt → project pairs (for [`Self::flush_stale`]).
    pub(crate) fn attempt_projects(&self) -> HashMap<Uuid, Uuid> {
        self.runs
            .lock()
            .expect("hub lock should not be poisoned: no code panics while holding it")
            .values()
            .map(|r| (r.attempt, r.project))
            .collect()
    }

    /// Any inbound frame from a connected run.
    pub(crate) fn seen(&self, run: &RunHandle) {
        let mut runs = self
            .runs
            .lock()
            .expect("hub lock should not be poisoned: no code panics while holding it");
        let e = runs.entry(run.agent_run_id).or_insert(RunLive {
            project: run.project_id,
            attempt: run.pipeline_attempt_id,
            epoch: run.epoch,
            last_seen: Instant::now(),
            last_seen_wall: OffsetDateTime::now_utc(),
            connected: true,
            ended: false,
            shown: Liveness::Online,
        });
        if run.epoch < e.epoch {
            return;
        }
        if run.epoch > e.epoch {
            e.epoch = run.epoch;
            e.ended = false;
        }
        e.last_seen = Instant::now();
        e.last_seen_wall = OffsetDateTime::now_utc();
        e.connected = true;
    }

    /// The socket closed; `ended` after a `bye`.
    pub(crate) fn disconnected(&self, run: &RunHandle, ended: bool) {
        if let Some(r) = self
            .runs
            .lock()
            .expect("hub lock should not be poisoned: no code panics while holding it")
            .get_mut(&run.agent_run_id)
            && r.epoch == run.epoch
        {
            r.connected = false;
            r.ended |= ended;
        }
    }

    fn state_of(&self, r: &RunLive) -> Liveness {
        if r.ended {
            Liveness::Completed
        } else if !r.connected {
            Liveness::Disconnected
        } else if r.last_seen.elapsed() > self.stale_after {
            Liveness::Stale
        } else {
            Liveness::Online
        }
    }

    /// Recomputes liveness; pushes threshold crossings. Returns the connected runs' last
    /// contact, for [`Store::persist_last_seen`].
    pub(crate) fn tick(&self) -> Vec<(Uuid, OffsetDateTime)> {
        let mut crossed = Vec::new();
        let mut seen = Vec::new();
        {
            let mut runs = self
                .runs
                .lock()
                .expect("hub lock should not be poisoned: no code panics while holding it");
            for (id, r) in runs.iter_mut() {
                let now = self.state_of(r);
                if now != r.shown {
                    r.shown = now;
                    crossed.push((r.project, r.attempt, *id, now));
                }
                if r.connected {
                    seen.push((*id, r.last_seen_wall));
                }
            }
            runs.retain(|_, r| r.connected || r.last_seen.elapsed() < Duration::from_secs(3600));
        }
        for (project, attempt, run, liveness) in crossed {
            self.send(
                project,
                vec![json(&LiveFrame::Liveness {
                    attempt_id: attempt,
                    agent_run_id: run,
                    liveness,
                })],
            );
        }
        seen
    }

    /// Fills in each agent run's liveness: from live contact when this instance has it, else
    /// from the stored state and contact (see [`Self::run_liveness_in`]).
    pub(crate) fn overlay(&self, entities: &mut [EntityView]) {
        let runs = self
            .runs
            .lock()
            .expect("hub lock should not be poisoned: no code panics while holding it");
        for e in entities {
            if let EntityView::AgentRun(r) = e {
                r.liveness = Some(self.run_liveness_in(&runs, r.id, &r.state, r.last_frame_at));
            }
        }
    }

    /// A run's display liveness. A socket this instance holds is authoritative, and so is a
    /// closed one unless the run is `active`: closing a socket detaches its run in Postgres, so
    /// an `active` run whose socket closed here was taken over by another instance. Any other
    /// `active` or `finalizing` run is judged from its persisted contact. The instance holding
    /// its socket persists that contact every sweep, so it is at most one sweep interval old, and
    /// the threshold allows for that lag. Contact from before this instance booted does not
    /// count, so after a restart a run stays `stale` until its agent reconnects.
    fn run_liveness_in(
        &self,
        runs: &HashMap<Uuid, RunLive>,
        id: Uuid,
        state: &str,
        last_frame_at: OffsetDateTime,
    ) -> Liveness {
        match (state, runs.get(&id)) {
            ("ended", _) => Liveness::Completed,
            (_, Some(live)) if live.connected || live.ended || state != "active" => {
                self.state_of(live)
            }
            ("active" | "finalizing", _) => {
                let fresh = last_frame_at > self.booted
                    && OffsetDateTime::now_utc() - last_frame_at <= self.persisted_stale_after;
                if fresh {
                    Liveness::Online
                } else {
                    Liveness::Stale
                }
            }
            _ => Liveness::Disconnected,
        }
    }

    /// One run's display liveness, as [`Self::overlay`] would set it.
    pub(crate) fn run_liveness(
        &self,
        id: Uuid,
        state: &str,
        last_frame_at: OffsetDateTime,
    ) -> Liveness {
        let runs = self
            .runs
            .lock()
            .expect("hub lock should not be poisoned: no code panics while holding it");
        self.run_liveness_in(&runs, id, state, last_frame_at)
    }

    /// The same overlay for a snapshot.
    pub(crate) fn overlay_snapshot(&self, s: &mut Snapshot) {
        let mut views: Vec<EntityView> = s.agent_runs.drain(..).map(EntityView::AgentRun).collect();
        self.overlay(&mut views);
        s.agent_runs = views
            .into_iter()
            .filter_map(|v| match v {
                EntityView::AgentRun(r) => Some(r),
                _ => None,
            })
            .collect();
    }
}

/// Republishes other instances' committed changes into this instance's hub (research 03 §5.5),
/// as a local commit would be. Runs until shutdown. A lost listener is re-established with
/// backoff, and subscribers resync: notices sent in the gap are gone.
pub(crate) async fn relay(state: AppState, mut notices: Notices) {
    loop {
        let received = tokio::select! {
            () = state.shutdown.cancelled() => return,
            received = notices.recv() => received,
        };
        match received {
            Ok(LiveNotice::Change(change)) => state.publish(Some(change)),
            Ok(LiveNotice::Reconnected) => state.hub.resync_all(),
            Err(err) => {
                tracing::warn!(error = %err, "live listener lost; reconnecting");
                let Some(fresh) = reconnect(&state).await else {
                    return;
                };
                notices = fresh;
                state.hub.resync_all();
            }
        }
    }
}

/// Listens again after a failure, backing off from 100 ms to 5 s; `None` on shutdown.
async fn reconnect(state: &AppState) -> Option<Notices> {
    let mut delay = Duration::from_millis(100);
    loop {
        tokio::select! {
            () = state.shutdown.cancelled() => return None,
            () = tokio::time::sleep(delay) => {}
        }
        match state.store.notices().await {
            Ok(notices) => return Some(notices),
            Err(err) => {
                tracing::warn!(error = %err, "re-listening for live changes");
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }
    }
}

/// Takes the frames that can go out now: the contiguous run after `published`, or with `force`
/// everything pending.
fn drain(o: &mut Order, force: bool) -> Vec<Arc<str>> {
    let mut out = Vec::new();
    while let Some(entry) = o.pending.first_entry() {
        if !force && *entry.key() != o.published + 1 {
            break;
        }
        o.published = *entry.key();
        out.push(entry.remove().1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_attempt_orders_are_pruned_without_dropping_pending_frames() {
        let hub = Hub::new(Duration::from_secs(5), Duration::from_secs(1), 16);
        let pending = Uuid::now_v7();
        let recent = Uuid::now_v7();
        {
            let mut orders = hub.order.lock().unwrap();
            for _ in 0..100 {
                orders.insert(
                    Uuid::now_v7(),
                    Order {
                        published: 1,
                        last_publish: Instant::now() - ORDER_RETENTION,
                        pending: BTreeMap::new(),
                    },
                );
            }
            orders.insert(
                recent,
                Order {
                    published: 4,
                    last_publish: Instant::now(),
                    pending: BTreeMap::new(),
                },
            );
            orders.insert(
                pending,
                Order {
                    published: 1,
                    last_publish: Instant::now() - ORDER_RETENTION,
                    pending: BTreeMap::from([(3, (Instant::now(), "three".into()))]),
                },
            );
        }
        hub.flush_stale(&HashMap::new());
        let orders = hub.order.lock().unwrap();
        assert_eq!(orders.len(), 2);
        assert!(orders.contains_key(&recent));
        assert!(orders[&pending].pending.contains_key(&3));
    }

    #[test]
    fn versions_go_out_in_order() {
        let mut o = Order {
            published: 4,
            last_publish: Instant::now(),
            pending: BTreeMap::new(),
        };
        o.pending.insert(6, (Instant::now(), "six".into()));
        assert!(drain(&mut o, false).is_empty(), "6 waits for 5");
        o.pending.insert(5, (Instant::now(), "five".into()));
        let out: Vec<String> = drain(&mut o, false)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(out, ["five", "six"]);
        assert_eq!(o.published, 6);
        o.pending.insert(9, (Instant::now(), "nine".into()));
        assert_eq!(drain(&mut o, true).len(), 1, "forced after the wait");
        assert_eq!(o.published, 9);
    }
}
