// SPDX-License-Identifier: AGPL-3.0-only
//! The live activity stream (spec §8 screen B): the meaningful things that happened, newest
//! first, derived from what changed between two states of the live store. The server sends
//! entities, not events, so the stream is exactly the differences the browser saw: a flow
//! started, ended, a runner went quiet, a job closed. Nothing is replayed from before the page
//! was opened.

use std::collections::{HashMap, VecDeque};

use fleet_protocol::live::{AgentRunView, FlowView, JobView};
use fleet_ui::{live_store::AttemptState, tone::Tone};
use uuid::Uuid;

use crate::status::{job_pill, liveness_pill};

/// The stream keeps this many entries.
pub const MAX_ENTRIES: usize = 50;

/// One thing that happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Increases with every entry: a stable key for keyed lists.
    pub id: u64,
    /// When the browser saw it, unix ms.
    pub at_ms: i64,
    /// What happened, as a sentence.
    pub text: String,
    /// Colour role.
    pub tone: Tone,
    /// The flow it is about, to link to.
    pub flow_id: Option<Uuid>,
}

/// The stream, newest entry first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityLog {
    entries: VecDeque<Entry>,
    next_id: u64,
}

impl ActivityLog {
    /// An empty stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The entries, newest first.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    /// Records what changed from `prev` to `next`, both seen at `now_ms`.
    pub fn observe(&mut self, now_ms: i64, prev: &AttemptState, next: &AttemptState) {
        let prev_jobs: HashMap<Uuid, &JobView> = prev.jobs().map(|j| (j.id, j)).collect();
        let prev_runs: HashMap<Uuid, &AgentRunView> =
            prev.agent_runs().map(|r| (r.id, r)).collect();
        let prev_flows: HashMap<Uuid, &FlowView> = prev.flows().map(|f| (f.id, f)).collect();
        let mut found: Vec<(String, Tone, Option<Uuid>)> = Vec::new();

        for job in next.jobs() {
            let name = job.name.as_deref().unwrap_or(&job.job_key);
            match prev_jobs.get(&job.id) {
                None if job.lifecycle != "closed" => {
                    found.push((format!("Job {name} started"), Tone::Info, None));
                }
                Some(p) if p.lifecycle != job.lifecycle && job.lifecycle == "closed" => {
                    let pill = job_pill(&job.lifecycle, job.outcome.as_deref());
                    found.push((
                        format!("Job {name} closed: {}", pill.label),
                        pill.tone,
                        None,
                    ));
                }
                _ => {}
            }
        }
        for run in next.agent_runs() {
            let name = run.runner_name.as_deref().unwrap_or("agent");
            match prev_runs.get(&run.id) {
                None => found.push((format!("Runner {name} connected"), Tone::Info, None)),
                Some(p) if p.liveness != run.liveness && run.liveness.is_some() => {
                    let pill = liveness_pill(run.liveness);
                    found.push((
                        format!("Runner {name} is now {}", pill.label),
                        pill.tone,
                        None,
                    ));
                }
                Some(_) => {}
            }
        }
        for flow in next.flows() {
            let before = prev_flows.get(&flow.id).map(|f| f.state.as_str());
            if before == Some(flow.state.as_str()) || (before.is_none() && flow.state == "pending")
            {
                continue;
            }
            let name = &flow.name;
            let entry = match flow.state.as_str() {
                "pending" => continue,
                "running" => (format!("Flow {name} started"), Tone::Info),
                "passed" => (format!("Flow {name} passed"), Tone::Success),
                "failed" => match &flow.failure_message {
                    Some(why) => (format!("Flow {name} failed: {why}"), Tone::Danger),
                    None => (format!("Flow {name} failed"), Tone::Danger),
                },
                "skipped" => (format!("Flow {name} skipped"), Tone::Muted),
                "interrupted" => (format!("Flow {name} interrupted"), Tone::Warning),
                _ => (format!("Flow {name}: outcome unknown"), Tone::Neutral),
            };
            found.push((entry.0, entry.1, Some(flow.id)));
        }
        for (text, tone, flow_id) in found {
            self.entries.push_front(Entry {
                id: self.next_id,
                at_ms: now_ms,
                text,
                tone,
                flow_id,
            });
            self.next_id += 1;
        }
        self.entries.truncate(MAX_ENTRIES);
    }
}

#[cfg(test)]
mod tests {
    use fleet_protocol::live::{EntityView, LiveFrame};
    use fleet_ui::live_store::LiveStore;

    use super::{super::fixtures::*, *};

    fn store(flows: Vec<fleet_protocol::live::FlowView>) -> LiveStore {
        let mut store = LiveStore::new();
        let _ = store.apply(snapshot(
            1,
            vec![job(JOB, "e2e", "running", None, None)],
            vec![run(RUN, JOB, "online")],
            vec![suite(SUITE, JOB, None)],
            flows,
        ));
        store
    }

    fn delta(store: &mut LiveStore, version: u64, entities: Vec<EntityView>) -> AttemptState {
        let _ = store.apply(LiveFrame::Delta {
            project_id: id(100),
            attempt_id: id(ATTEMPT),
            version,
            entities,
        });
        store.attempt(id(ATTEMPT)).unwrap().clone()
    }

    fn texts(log: &ActivityLog) -> Vec<&str> {
        log.entries().map(|e| e.text.as_str()).collect()
    }

    #[test]
    fn a_flows_life_reads_as_started_then_passed_newest_first() {
        let mut s = store(vec![flow(51, SUITE, "login", "pending", "provisional")]);
        let mut log = ActivityLog::new();
        let before = s.attempt(id(ATTEMPT)).unwrap().clone();
        let running = delta(
            &mut s,
            2,
            vec![EntityView::Flow(flow(
                51,
                SUITE,
                "login",
                "running",
                "provisional",
            ))],
        );
        log.observe(1000, &before, &running);
        let passed = delta(
            &mut s,
            3,
            vec![EntityView::Flow(flow(
                51,
                SUITE,
                "login",
                "passed",
                "provisional",
            ))],
        );
        log.observe(2000, &running, &passed);
        assert_eq!(texts(&log), ["Flow login passed", "Flow login started"]);
        let first = log.entries().next().unwrap();
        assert_eq!(
            (first.at_ms, first.tone, first.flow_id),
            (2000, Tone::Success, Some(id(51)))
        );
        assert!(first.id > log.entries().nth(1).unwrap().id);
    }

    #[test]
    fn a_failure_says_why_when_the_server_did() {
        let mut s = store(vec![flow(51, SUITE, "checkout", "running", "provisional")]);
        let before = s.attempt(id(ATTEMPT)).unwrap().clone();
        let mut failed = flow(51, SUITE, "checkout", "failed", "provisional");
        failed.failure_message = Some("Assertion is false".into());
        let after = delta(&mut s, 2, vec![EntityView::Flow(failed)]);
        let mut log = ActivityLog::new();
        log.observe(1000, &before, &after);
        assert_eq!(texts(&log), ["Flow checkout failed: Assertion is false"]);
        assert_eq!(log.entries().next().unwrap().tone, Tone::Danger);
    }

    #[test]
    fn planned_flows_and_unchanged_flows_are_not_news() {
        let mut s = store(vec![flow(51, SUITE, "login", "passed", "verified")]);
        let before = s.attempt(id(ATTEMPT)).unwrap().clone();
        let after = delta(
            &mut s,
            2,
            vec![
                EntityView::Flow(flow(52, SUITE, "later", "pending", "provisional")),
                EntityView::Flow(flow(51, SUITE, "login", "passed", "verified")),
            ],
        );
        let mut log = ActivityLog::new();
        log.observe(1000, &before, &after);
        assert_eq!(texts(&log), Vec::<&str>::new());
    }

    #[test]
    fn a_runner_going_quiet_is_reported_once() {
        let mut s = store(vec![]);
        let before = s.attempt(id(ATTEMPT)).unwrap().clone();
        let _ = s.apply(LiveFrame::Liveness {
            attempt_id: id(ATTEMPT),
            agent_run_id: id(RUN),
            liveness: fleet_protocol::live::Liveness::Stale,
        });
        let quiet = s.attempt(id(ATTEMPT)).unwrap().clone();
        let mut log = ActivityLog::new();
        log.observe(1000, &before, &quiet);
        log.observe(2000, &quiet, &quiet);
        assert_eq!(texts(&log), ["Runner runner-1 is now Stale"]);
        assert_eq!(log.entries().next().unwrap().tone, Tone::Warning);
    }

    #[test]
    fn a_job_closing_is_reported_with_its_outcome() {
        let mut s = store(vec![]);
        let before = s.attempt(id(ATTEMPT)).unwrap().clone();
        let after = delta(
            &mut s,
            2,
            vec![EntityView::Job(job(
                JOB,
                "e2e",
                "closed",
                Some("failed"),
                Some("junit_final"),
            ))],
        );
        let mut log = ActivityLog::new();
        log.observe(1000, &before, &after);
        assert_eq!(texts(&log), ["Job e2e closed: Failed"]);
    }

    #[test]
    fn the_stream_keeps_only_the_newest_entries() {
        let mut s = store(vec![]);
        let mut log = ActivityLog::new();
        let mut prev = s.attempt(id(ATTEMPT)).unwrap().clone();
        for n in 0..(MAX_ENTRIES as u128 + 10) {
            let next = delta(
                &mut s,
                2 + u64::try_from(n).unwrap(),
                vec![EntityView::Flow(flow(
                    100 + n,
                    SUITE,
                    &format!("f{n}"),
                    "running",
                    "provisional",
                ))],
            );
            log.observe(i64::try_from(n).unwrap(), &prev, &next);
            prev = next;
        }
        assert_eq!(log.entries().count(), MAX_ENTRIES);
        assert_eq!(log.entries().next().unwrap().text, "Flow f59 started");
    }
}
