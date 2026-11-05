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
//! Liveness (`online` → `stale` → `disconnected` → `completed`) is in memory: heartbeats never
//! touch the database. Crossings are pushed as `liveness` frames.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use fleet_protocol::live::{EntityView, LiveFrame, Liveness, ResyncReason, Snapshot};
use fleet_store::{
    Store,
    ingest::{Change, RunHandle},
};
use time::OffsetDateTime;
use tokio::{sync::broadcast, time::Instant};
use uuid::Uuid;

/// How long a later version waits for an earlier one before it is sent anyway.
const REORDER_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct Order {
    published: u64,
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
    capacity: usize,
    #[cfg(test)]
    pub(crate) fail_entity_load: std::sync::atomic::AtomicBool,
}

fn json(frame: &LiveFrame) -> Arc<str> {
    serde_json::to_string(frame).unwrap_or_default().into()
}

impl Hub {
    pub(crate) fn new(stale_after: Duration, capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            #[cfg(test)]
            fail_entity_load: std::sync::atomic::AtomicBool::new(false),
            channels: Mutex::new(HashMap::new()),
            order: Mutex::new(HashMap::new()),
            runs: Mutex::new(HashMap::new()),
            stale_after,
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
                    pending: BTreeMap::new(),
                });
            if change.version <= o.published {
                return;
            }
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
        }
        for (project, frames) in out {
            self.send(project, frames);
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
    /// from the stored state (after a restart an active run shows `stale`, never `disconnected`).
    pub(crate) fn overlay(&self, entities: &mut [EntityView]) {
        let runs = self
            .runs
            .lock()
            .expect("hub lock should not be poisoned: no code panics while holding it");
        for e in entities {
            if let EntityView::AgentRun(r) = e {
                r.liveness = Some(self.run_liveness_in(&runs, r.id, &r.state));
            }
        }
    }

    fn run_liveness_in(&self, runs: &HashMap<Uuid, RunLive>, id: Uuid, state: &str) -> Liveness {
        match (state, runs.get(&id)) {
            ("ended", _) => Liveness::Completed,
            (_, Some(live)) => self.state_of(live),
            ("active" | "finalizing", None) => Liveness::Stale,
            _ => Liveness::Disconnected,
        }
    }

    /// One run's display liveness, as [`Self::overlay`] would set it.
    pub(crate) fn run_liveness(&self, id: Uuid, state: &str) -> Liveness {
        let runs = self
            .runs
            .lock()
            .expect("hub lock should not be poisoned: no code panics while holding it");
        self.run_liveness_in(&runs, id, state)
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
    fn versions_go_out_in_order() {
        let mut o = Order {
            published: 4,
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
