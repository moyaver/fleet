// SPDX-License-Identifier: AGPL-3.0-only
//! The reactive adapter over `fleet_ui::live_store::LiveStore` (PLAN §5.4 pitfalls).
//!
//! A [`LiveModel`] follows one attempt. Each entity has its own signal, so a flow changing state
//! re-renders its row only, not the list; the lists are keyed `(epoch, id)` so a re-snapshot
//! rebuilds them. All frames of a batch are applied to the store first and the signals are set
//! once afterwards, which is the coalescing: one write per entity per animation frame.

use std::collections::HashMap;

use fleet_protocol::live::{
    AgentRunView, AttemptView, DeviceView, FlowView, JobView, LiveFrame, SuiteView,
};
use fleet_ui::live_store::{AttemptState, LiveStore};
use leptos::prelude::*;
use uuid::Uuid;

use crate::{
    clock::now_ms,
    live::keys::{Change, Key, Keys},
    progress::Progress,
    screens::pipeline::{
        activity::ActivityLog,
        model::{FailureRow, Header, Tree, failures, header},
        plans,
    },
};

/// The entities of one kind: their keys, and a signal per entity.
#[derive(Debug)]
pub struct EntityTable<T: Send + Sync + 'static> {
    shape: StoredValue<Keys>,
    cells: StoredValue<HashMap<Uuid, ArcRwSignal<T>>>,
    /// The keys, in id order. Only set when entities come, go or the epoch changes.
    pub keys: RwSignal<Vec<Key>>,
}

impl<T: Send + Sync + 'static> Clone for EntityTable<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Send + Sync + 'static> Copy for EntityTable<T> {}

impl<T: Clone + PartialEq + Send + Sync + 'static> EntityTable<T> {
    fn new() -> Self {
        Self {
            shape: StoredValue::new(Keys::default()),
            cells: StoredValue::new(HashMap::new()),
            keys: RwSignal::new(Vec::new()),
        }
    }

    /// The signal of one entity.
    #[must_use]
    pub fn cell(&self, id: Uuid) -> Option<ArcRwSignal<T>> {
        self.cells.with_value(|c| c.get(&id).cloned())
    }

    fn sync(&self, epoch: u64, entities: Vec<T>, id_of: impl Fn(&T) -> Uuid) {
        let change = self
            .shape
            .try_update_value(|k| k.update(epoch, entities.iter().map(&id_of).collect()))
            .flatten();
        self.cells.update_value(|cells| {
            if change == Some(Change::Rebuilt) {
                cells.clear();
            }
            let present: Vec<Uuid> = entities.iter().map(&id_of).collect();
            cells.retain(|id, _| present.contains(id));
            for entity in entities {
                match cells.get(&id_of(&entity)) {
                    Some(cell) => {
                        if cell.read_untracked().ne(&entity) {
                            cell.set(entity);
                        }
                    }
                    None => {
                        cells.insert(id_of(&entity), ArcRwSignal::new(entity));
                    }
                }
            }
        });
        // Cells exist before the keys that name them are published.
        if change.is_some() {
            self.keys.set(self.shape.with_value(Keys::keys));
        }
    }
}

/// One attempt, live.
#[derive(Debug, Clone, Copy)]
pub struct LiveModel {
    attempt_id: Uuid,
    store: StoredValue<LiveStore>,
    /// The attempt, once its snapshot arrived.
    pub attempt: RwSignal<Option<AttemptView>>,
    /// The header, once its snapshot arrived.
    pub header: RwSignal<Option<Header>>,
    /// Flow progress over the attempt, once its snapshot arrived.
    pub progress: RwSignal<Option<Progress>>,
    /// Failed and interrupted flows.
    pub failures: RwSignal<Vec<FailureRow>>,
    /// Jobs, suites and flows grouped.
    pub tree: RwSignal<Tree>,
    /// What happened since the page opened.
    pub activity: RwSignal<ActivityLog>,
    /// The attempt's version at the last frame applied.
    pub version: RwSignal<u64>,
    /// The snapshot generation: part of every list key.
    pub epoch: RwSignal<u64>,
    /// Counts the batches that may have changed the attempt's plans; the page refetches them.
    pub plan_refreshes: RwSignal<u64>,
    /// Waiting for a new snapshot: what is shown is the last known state.
    pub awaiting_snapshot: RwSignal<bool>,
    /// The server's last refusal.
    pub error: RwSignal<Option<String>>,
    /// Jobs.
    pub jobs: EntityTable<JobView>,
    /// Agent runs.
    pub runs: EntityTable<AgentRunView>,
    /// Suites.
    pub suites: EntityTable<SuiteView>,
    /// Devices.
    pub devices: EntityTable<DeviceView>,
    /// Flow executions.
    pub flows: EntityTable<FlowView>,
}

impl LiveModel {
    /// A model with no data yet, for `attempt_id`.
    #[must_use]
    pub fn new(attempt_id: Uuid) -> Self {
        Self {
            attempt_id,
            store: StoredValue::new(LiveStore::new()),
            attempt: RwSignal::new(None),
            header: RwSignal::new(None),
            progress: RwSignal::new(None),
            failures: RwSignal::new(Vec::new()),
            tree: RwSignal::new(Tree::default()),
            activity: RwSignal::new(ActivityLog::new()),
            version: RwSignal::new(0),
            epoch: RwSignal::new(0),
            plan_refreshes: RwSignal::new(0),
            awaiting_snapshot: RwSignal::new(false),
            error: RwSignal::new(None),
            jobs: EntityTable::new(),
            runs: EntityTable::new(),
            suites: EntityTable::new(),
            devices: EntityTable::new(),
            flows: EntityTable::new(),
        }
    }

    /// Applies a batch of frames and updates the signals once.
    pub fn apply(&self, project: &str, frames: Vec<LiveFrame>) {
        let before = self.state();
        let plans_may_change = plans::batch_changes_plans(&frames, |run| {
            before
                .as_ref()
                .is_some_and(|s| s.agent_runs().any(|r| r.id == run && r.plan_id.is_some()))
        });
        self.store.update_value(|s| {
            for frame in frames {
                // A resync is acted on by the socket (it resubscribes); the store marks itself.
                let _ = s.apply(frame);
            }
        });
        self.error
            .set(self.store.with_value(|s| s.last_error().map(str::to_owned)));
        self.publish(project, before.as_ref());
        // The attempt ending moves its plans' actual times one last time: refetch once, then.
        let ended_now = self.state().is_some_and(|s| plans::attempt_ended(s.jobs()));
        let ended_before = before
            .as_ref()
            .is_some_and(|s| plans::attempt_ended(s.jobs()));
        if plans_may_change || (ended_now && !ended_before) {
            self.plan_refreshes.update(|n| *n += 1);
        }
    }

    /// The socket closed: the state shown may be behind until a new snapshot arrives.
    pub fn connection_lost(&self) {
        self.store.update_value(LiveStore::connection_lost);
        self.awaiting_snapshot
            .set(self.state().is_some_and(|s| s.awaiting_snapshot()));
    }

    fn state(&self) -> Option<AttemptState> {
        self.store
            .with_value(|s| s.attempt(self.attempt_id).cloned())
    }

    fn publish(&self, project: &str, before: Option<&AttemptState>) {
        let Some(state) = self.state() else {
            return;
        };
        if before == Some(&state) {
            return;
        }
        if let Some(before) = before {
            self.activity
                .update(|log| log.observe(now_ms(), before, &state));
        }
        let epoch = state.epoch();
        self.jobs
            .sync(epoch, state.jobs().cloned().collect(), |e| e.id);
        self.runs
            .sync(epoch, state.agent_runs().cloned().collect(), |e| e.id);
        self.suites
            .sync(epoch, state.suites().cloned().collect(), |e| e.id);
        self.devices
            .sync(epoch, state.devices().cloned().collect(), |e| e.id);
        self.flows
            .sync(epoch, state.flows().cloned().collect(), |e| e.id);
        set_if_changed(&self.tree, Tree::build(&state));
        set_if_changed(&self.header, Some(header(&state)));
        set_if_changed(
            &self.progress,
            Some(Progress::from_flows(
                state.flows(),
                state.attempt().planned_total,
            )),
        );
        set_if_changed(&self.failures, failures(&state, project));
        set_if_changed(&self.attempt, Some(state.attempt().clone()));
        self.awaiting_snapshot.set(state.awaiting_snapshot());
        self.epoch.set(state.epoch());
        self.version.set(state.version());
    }
}

fn set_if_changed<T: PartialEq + Send + Sync + 'static>(signal: &RwSignal<T>, value: T) {
    if signal.read_untracked().ne(&value) {
        signal.set(value);
    }
}
