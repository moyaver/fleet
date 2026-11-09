// SPDX-License-Identifier: AGPL-3.0-only
//! The dashboard's mirror of the live socket (PLAN §7.3; research 03 §5).
//!
//! Feed it every [`LiveFrame`] in arrival order. A snapshot replaces an attempt's state; deltas
//! apply only when they carry exactly `version + 1`. A duplicate is ignored, a gap or a
//! `resync` marks the attempt as awaiting a new snapshot and tells the caller to re-subscribe
//! ([`Applied::Resync`]). Until that snapshot arrives the old state stays readable but no more
//! deltas apply, so the screen never shows a state that skipped a change.
//!
//! Each snapshot bumps the attempt's [`AttemptState::epoch`]. Keyed lists in the UI include it
//! in their keys, so a re-snapshot rebuilds rows instead of diffing across an unknown gap.
//!
//! No I/O, no reactive types: the Leptos layer wraps this in a signal.

use std::collections::BTreeMap;

use fleet_protocol::live::{
    AgentRunView, AttemptView, DeviceView, EntityView, FlowView, JobView, LiveFrame, PlanView,
    Snapshot, SuiteView,
};
use uuid::Uuid;

/// What a frame did to the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a Resync result means the caller must fetch a snapshot again"]
pub enum Applied {
    /// State changed.
    Updated,
    /// Nothing to do: a duplicate, a delta for an attempt without a snapshot, or a frame that
    /// carries no state.
    Ignored,
    /// The store can no longer follow this socket: subscribe again to get snapshots.
    Resync,
}

/// One attempt as the browser knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptState {
    attempt: AttemptView,
    version: u64,
    epoch: u64,
    awaiting_snapshot: bool,
    jobs: BTreeMap<Uuid, JobView>,
    agent_runs: BTreeMap<Uuid, AgentRunView>,
    suites: BTreeMap<Uuid, SuiteView>,
    devices: BTreeMap<Uuid, DeviceView>,
    flows: BTreeMap<Uuid, FlowView>,
    plans: Vec<PlanView>,
}

impl AttemptState {
    /// The attempt itself; its `version` is kept current by deltas.
    #[must_use]
    pub fn attempt(&self) -> &AttemptView {
        &self.attempt
    }

    /// The version of the last frame applied.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// How many snapshots this attempt has been rebuilt from, from 1.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The state skipped a change and waits for a snapshot: it is stale, not wrong.
    #[must_use]
    pub fn awaiting_snapshot(&self) -> bool {
        self.awaiting_snapshot
    }

    /// Jobs, by id.
    pub fn jobs(&self) -> impl Iterator<Item = &JobView> {
        self.jobs.values()
    }

    /// Agent runs, by id.
    pub fn agent_runs(&self) -> impl Iterator<Item = &AgentRunView> {
        self.agent_runs.values()
    }

    /// Suites, by id.
    pub fn suites(&self) -> impl Iterator<Item = &SuiteView> {
        self.suites.values()
    }

    /// Devices, by id.
    pub fn devices(&self) -> impl Iterator<Item = &DeviceView> {
        self.devices.values()
    }

    /// Flow executions, by id.
    pub fn flows(&self) -> impl Iterator<Item = &FlowView> {
        self.flows.values()
    }

    /// Linked Fleet plans.
    #[must_use]
    pub fn plans(&self) -> &[PlanView] {
        &self.plans
    }
}

/// Every attempt the socket has told this browser about. See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveStore {
    attempts: BTreeMap<Uuid, AttemptState>,
    last_error: Option<String>,
}

impl LiveStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One attempt's state, if a snapshot arrived.
    #[must_use]
    pub fn attempt(&self, id: Uuid) -> Option<&AttemptState> {
        self.attempts.get(&id)
    }

    /// The server's last refusal, for display.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// The socket closed: versions may have been missed, so every attempt awaits a snapshot.
    pub fn connection_lost(&mut self) {
        self.await_snapshots();
    }

    fn await_snapshots(&mut self) {
        for a in self.attempts.values_mut() {
            a.awaiting_snapshot = true;
        }
    }

    /// Applies the next frame from the socket.
    pub fn apply(&mut self, frame: LiveFrame) -> Applied {
        match frame {
            LiveFrame::Snapshot { snapshot } => self.snapshot(*snapshot),
            LiveFrame::Delta {
                attempt_id,
                version,
                entities,
                ..
            } => self.delta(attempt_id, version, entities),
            LiveFrame::Liveness {
                attempt_id,
                agent_run_id,
                liveness,
            } => {
                let run = self
                    .attempts
                    .get_mut(&attempt_id)
                    .and_then(|a| a.agent_runs.get_mut(&agent_run_id));
                match run {
                    Some(run) => {
                        run.liveness = Some(liveness);
                        Applied::Updated
                    }
                    None => Applied::Ignored,
                }
            }
            LiveFrame::Resync { .. } => {
                self.await_snapshots();
                Applied::Resync
            }
            LiveFrame::Error { message } => {
                self.last_error = Some(message);
                Applied::Ignored
            }
            LiveFrame::Subscribed { .. } | LiveFrame::Unsubscribed | LiveFrame::Samples { .. } => {
                Applied::Ignored
            }
        }
    }

    fn snapshot(&mut self, s: Snapshot) -> Applied {
        let epoch = self.attempts.get(&s.attempt.id).map_or(0, |a| a.epoch) + 1;
        let version = u64::try_from(s.attempt.version).unwrap_or(0);
        let id = s.attempt.id;
        self.attempts.insert(
            id,
            AttemptState {
                version,
                epoch,
                awaiting_snapshot: false,
                jobs: s.jobs.into_iter().map(|e| (e.id, e)).collect(),
                agent_runs: s.agent_runs.into_iter().map(|e| (e.id, e)).collect(),
                suites: s.suites.into_iter().map(|e| (e.id, e)).collect(),
                devices: s.devices.into_iter().map(|e| (e.id, e)).collect(),
                flows: s.flows.into_iter().map(|e| (e.id, e)).collect(),
                plans: s.plans,
                attempt: s.attempt,
            },
        );
        Applied::Updated
    }

    fn delta(&mut self, attempt: Uuid, version: u64, entities: Vec<EntityView>) -> Applied {
        let Some(state) = self.attempts.get_mut(&attempt) else {
            return Applied::Ignored;
        };
        if state.awaiting_snapshot || version <= state.version {
            return Applied::Ignored;
        }
        if version != state.version + 1 {
            state.awaiting_snapshot = true;
            return Applied::Resync;
        }
        state.version = version;
        state.attempt.version = i64::try_from(version).unwrap_or(i64::MAX);
        for e in entities {
            match e {
                EntityView::Job(v) => _ = state.jobs.insert(v.id, v),
                EntityView::AgentRun(v) => _ = state.agent_runs.insert(v.id, v),
                EntityView::Suite(v) => _ = state.suites.insert(v.id, v),
                EntityView::Flow(v) => _ = state.flows.insert(v.id, v),
                EntityView::Device(v) => _ = state.devices.insert(v.id, v),
            }
        }
        Applied::Updated
    }
}

#[cfg(test)]
mod tests {
    use fleet_protocol::{
        frame::RunOrigin,
        live::{Liveness, ResyncReason},
    };
    use time::OffsetDateTime;

    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    const ATTEMPT: u128 = 1;

    fn attempt(version: i64) -> AttemptView {
        AttemptView {
            id: id(ATTEMPT),
            project_id: id(100),
            provider: "github_actions".into(),
            run_key: "acme/app:42".into(),
            repository: None,
            commit_sha: None,
            git_ref: None,
            attempt: 1,
            version,
            planned_total: None,
            jobs: 0,
            open_jobs: 0,
            first_seen_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn flow(n: u128, name: &str, state: &str, rev: i64) -> FlowView {
        FlowView {
            origin: RunOrigin::Live,
            id: id(n),
            job_id: id(10),
            suite_execution_id: id(11),
            agent_run_id: id(12),
            device_session_id: None,
            shard_number: None,
            ordinal: None,
            name: name.into(),
            path: None,
            state: state.into(),
            outcome_source: None,
            finality: "provisional".into(),
            has_conflict: false,
            failure_message: None,
            duration_ms: None,
            steps: 0,
            rev,
        }
    }

    fn run(liveness: Option<Liveness>) -> AgentRunView {
        AgentRunView {
            origin: RunOrigin::Live,
            id: id(12),
            job_id: id(10),
            worker_id: None,
            runner_name: None,
            state: "active".into(),
            trust_level: "trusted".into(),
            committed_seq: 0,
            gaps: 0,
            last_frame_at: OffsetDateTime::UNIX_EPOCH,
            late_resume: false,
            plan_id: None,
            plan_shard_id: None,
            runner_exit_code: None,
            runner_version: None,
            runner_profile_supported: None,
            file_warnings: Vec::new(),
            rev: 1,
            liveness,
        }
    }

    fn snapshot(version: i64, flows: Vec<FlowView>) -> LiveFrame {
        LiveFrame::Snapshot {
            snapshot: Box::new(Snapshot {
                attempt: attempt(version),
                jobs: vec![],
                agent_runs: vec![run(Some(Liveness::Online))],
                suites: vec![],
                devices: vec![],
                flows,
                plans: vec![],
            }),
        }
    }

    fn delta(version: u64, entities: Vec<EntityView>) -> LiveFrame {
        LiveFrame::Delta {
            project_id: id(100),
            attempt_id: id(ATTEMPT),
            version,
            entities,
        }
    }

    fn flow_states(store: &LiveStore) -> Vec<(String, String)> {
        store
            .attempt(id(ATTEMPT))
            .unwrap()
            .flows()
            .map(|f| (f.name.clone(), f.state.clone()))
            .collect()
    }

    #[test]
    fn a_snapshot_is_the_attempts_state_at_its_version() {
        let mut store = LiveStore::new();
        let r = store.apply(snapshot(5, vec![flow(20, "login", "running", 1)]));
        assert_eq!(r, Applied::Updated);
        let a = store.attempt(id(ATTEMPT)).unwrap();
        assert_eq!(a.version(), 5);
        assert_eq!(a.epoch(), 1);
        assert!(!a.awaiting_snapshot());
        assert_eq!(flow_states(&store), [("login".into(), "running".into())]);
    }

    #[test]
    fn the_next_version_updates_entities_in_place_and_adds_new_ones() {
        let mut store = LiveStore::new();
        let _ = store.apply(snapshot(5, vec![flow(20, "login", "running", 1)]));
        let r = store.apply(delta(
            6,
            vec![
                EntityView::Flow(flow(20, "login", "passed", 2)),
                EntityView::Flow(flow(21, "checkout", "running", 1)),
            ],
        ));
        assert_eq!(r, Applied::Updated);
        assert_eq!(
            flow_states(&store),
            [
                ("login".into(), "passed".into()),
                ("checkout".into(), "running".into())
            ]
        );
        let a = store.attempt(id(ATTEMPT)).unwrap();
        assert_eq!((a.version(), a.attempt().version), (6, 6));
    }

    #[test]
    fn a_delta_at_or_below_the_current_version_is_ignored() {
        let mut store = LiveStore::new();
        let _ = store.apply(snapshot(5, vec![flow(20, "login", "running", 1)]));
        for v in [4, 5] {
            let r = store.apply(delta(
                v,
                vec![EntityView::Flow(flow(20, "login", "failed", 9))],
            ));
            assert_eq!(r, Applied::Ignored, "version {v}");
        }
        assert_eq!(flow_states(&store), [("login".into(), "running".into())]);
    }

    #[test]
    fn a_version_gap_asks_for_a_resync_and_stops_applying_deltas() {
        let mut store = LiveStore::new();
        let _ = store.apply(snapshot(5, vec![flow(20, "login", "running", 1)]));
        let gap = store.apply(delta(
            7,
            vec![EntityView::Flow(flow(20, "login", "passed", 3))],
        ));
        assert_eq!(gap, Applied::Resync);
        assert!(store.attempt(id(ATTEMPT)).unwrap().awaiting_snapshot());
        // Even the "missing" version 6 must not apply on top of a state that skipped nothing
        // visible: the store waits for the snapshot.
        let late = store.apply(delta(
            6,
            vec![EntityView::Flow(flow(20, "login", "passed", 2))],
        ));
        assert_eq!(late, Applied::Ignored);
        assert_eq!(flow_states(&store), [("login".into(), "running".into())]);
    }

    #[test]
    fn a_new_snapshot_after_a_gap_replaces_the_state_and_bumps_the_epoch() {
        let mut store = LiveStore::new();
        let _ = store.apply(snapshot(5, vec![flow(20, "login", "running", 1)]));
        let _ = store.apply(delta(9, vec![]));
        let _ = store.apply(snapshot(9, vec![flow(21, "checkout", "passed", 4)]));
        let a = store.attempt(id(ATTEMPT)).unwrap();
        assert_eq!(
            (a.version(), a.epoch(), a.awaiting_snapshot()),
            (9, 2, false)
        );
        assert_eq!(flow_states(&store), [("checkout".into(), "passed".into())]);
        let r = store.apply(delta(
            10,
            vec![EntityView::Flow(flow(21, "checkout", "failed", 5))],
        ));
        assert_eq!(r, Applied::Updated, "deltas continue from the new snapshot");
    }

    #[test]
    fn a_resync_frame_marks_every_attempt_and_asks_to_resubscribe() {
        let mut store = LiveStore::new();
        let _ = store.apply(snapshot(5, vec![]));
        let r = store.apply(LiveFrame::Resync {
            reason: ResyncReason::Lagged,
        });
        assert_eq!(r, Applied::Resync);
        assert!(store.attempt(id(ATTEMPT)).unwrap().awaiting_snapshot());
    }

    #[test]
    fn losing_the_connection_marks_every_attempt_stale_until_resnapshotted() {
        let mut store = LiveStore::new();
        let _ = store.apply(snapshot(5, vec![]));
        store.connection_lost();
        assert!(store.attempt(id(ATTEMPT)).unwrap().awaiting_snapshot());
        let r = store.apply(delta(6, vec![]));
        assert_eq!(r, Applied::Ignored);
    }

    #[test]
    fn a_delta_for_an_attempt_without_a_snapshot_is_ignored() {
        let mut store = LiveStore::new();
        assert_eq!(store.apply(delta(3, vec![])), Applied::Ignored);
        assert!(store.attempt(id(ATTEMPT)).is_none());
    }

    #[test]
    fn liveness_frames_update_the_run_without_touching_the_version() {
        let mut store = LiveStore::new();
        let _ = store.apply(snapshot(5, vec![]));
        let r = store.apply(LiveFrame::Liveness {
            attempt_id: id(ATTEMPT),
            agent_run_id: id(12),
            liveness: Liveness::Stale,
        });
        assert_eq!(r, Applied::Updated);
        let a = store.attempt(id(ATTEMPT)).unwrap();
        assert_eq!(a.version(), 5);
        assert_eq!(
            a.agent_runs().map(|r| r.liveness).collect::<Vec<_>>(),
            [Some(Liveness::Stale)]
        );
    }

    #[test]
    fn a_refusal_is_kept_for_display() {
        let mut store = LiveStore::new();
        let r = store.apply(LiveFrame::Error {
            message: "no such attempt".into(),
        });
        assert_eq!(r, Applied::Ignored);
        assert_eq!(store.last_error(), Some("no such attempt"));
    }
}
