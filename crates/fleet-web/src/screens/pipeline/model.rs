// SPDX-License-Identifier: AGPL-3.0-only
//! What screen B shows of an attempt, computed from the live store's state: the jobs → suites →
//! flows tree, the header with the Maestro result, and the failures list.

use std::collections::HashSet;

use fleet_protocol::{
    live::{AgentRunView, FlowView, JobView, SuiteView},
    rest::JobCounts,
};
use fleet_ui::{components::live::LiveState, live_store::AttemptState, provenance::Provenance};
use uuid::Uuid;

use crate::{
    clock::unix_ms,
    format::{ref_name, short_commit, time_of_day},
    live::socket::Connection,
    paths::{ci_run_url, flow_href},
    status::{Pill, attempt_pill, flow_provenance, flow_state_pill, job_pill, source_provenance},
};

/// Jobs, with the agent runs and suites under them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tree {
    /// By job key.
    pub jobs: Vec<JobNode>,
    /// Flows whose suite is not in the state: shown, never dropped.
    pub orphan_flows: Vec<Uuid>,
}

/// One job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobNode {
    /// The job.
    pub id: Uuid,
    /// Its agent runs.
    pub runs: Vec<Uuid>,
    /// Its suites, oldest first.
    pub suites: Vec<SuiteNode>,
}

/// One suite and its flows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteNode {
    /// The suite.
    pub id: Uuid,
    /// Its flows by shard, then order, then name.
    pub flows: Vec<Uuid>,
}

impl Tree {
    /// Groups the state's entities. Only ids and order: each row reads its entity's own signal.
    #[must_use]
    pub fn build(state: &AttemptState) -> Self {
        let mut jobs: Vec<&JobView> = state.jobs().collect();
        jobs.sort_by(|a, b| a.job_key.cmp(&b.job_key).then(a.id.cmp(&b.id)));
        let mut flows: Vec<&FlowView> = state.flows().collect();
        flows.sort_by(|a, b| {
            (a.shard_number, a.ordinal, &a.name, a.id).cmp(&(
                b.shard_number,
                b.ordinal,
                &b.name,
                b.id,
            ))
        });
        let mut suites: Vec<&SuiteView> = state.suites().collect();
        suites.sort_by_key(|s| s.id);
        let mut runs: Vec<&AgentRunView> = state.agent_runs().collect();
        runs.sort_by_key(|r| r.id);
        let known_suites: HashSet<Uuid> = suites.iter().map(|s| s.id).collect();
        Self {
            jobs: jobs
                .iter()
                .map(|job| JobNode {
                    id: job.id,
                    runs: runs
                        .iter()
                        .filter(|r| r.job_id == job.id)
                        .map(|r| r.id)
                        .collect(),
                    suites: suites
                        .iter()
                        .filter(|s| s.job_id == job.id)
                        .map(|s| SuiteNode {
                            id: s.id,
                            flows: flows
                                .iter()
                                .filter(|f| f.suite_execution_id == s.id)
                                .map(|f| f.id)
                                .collect(),
                        })
                        .collect(),
                })
                .collect(),
            orphan_flows: flows
                .iter()
                .filter(|f| !known_suites.contains(&f.suite_execution_id))
                .map(|f| f.id)
                .collect(),
        }
    }
}

/// The page header.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    /// The repository, else the run key.
    pub title: String,
    /// Git ref, as people say it.
    pub git_ref: Option<String>,
    /// Commit, shortened.
    pub commit: Option<String>,
    /// Re-run attempt, from 1.
    pub attempt_number: i32,
    /// First seen, unix ms.
    pub started_ms: i64,
    /// The CI provider's page for the run, when known.
    pub ci_url: Option<String>,
    /// The Maestro result across the jobs.
    pub result: Pill,
    /// How settled the result is: the weakest evidence among the closed jobs.
    pub result_provenance: Provenance,
    /// Whether every job has closed: no further updates will come for this attempt.
    pub settled: bool,
}

/// The header of an attempt.
#[must_use]
pub fn header(state: &AttemptState) -> Header {
    let at = state.attempt();
    let jobs: Vec<&JobView> = state.jobs().collect();
    let (result, result_provenance) = result(&jobs);
    let settled = !jobs.is_empty() && jobs.iter().all(|j| j.lifecycle == "closed");
    Header {
        title: at.repository.clone().unwrap_or_else(|| at.run_key.clone()),
        git_ref: at.git_ref.as_deref().map(|r| ref_name(r).to_owned()),
        commit: at.commit_sha.as_deref().map(short_commit),
        attempt_number: at.attempt,
        started_ms: unix_ms(at.first_seen_at),
        ci_url: ci_run_url(&at.provider, at.repository.as_deref(), &at.run_key),
        result,
        result_provenance,
        settled,
    }
}

/// What the top bar's live indicator shows while the page is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveView {
    /// The state's word.
    pub state: LiveState,
    /// What follows the word, such as "updated 2 s ago". Empty when there is nothing to add.
    pub detail: String,
    /// The menu's action, such as "Pause". `None` when there is no menu.
    pub action: Option<String>,
}

/// What the page knows at one moment, all in unix milliseconds except the counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveInputs {
    /// The socket's state.
    pub connection: Connection,
    /// Whether the person paused updates.
    pub paused: bool,
    /// Frames held while paused.
    pub held: usize,
    /// Whether every job has closed: no further updates will come.
    pub settled: bool,
    /// When the attempt was first seen settled, if it has been.
    pub settled_at: Option<i64>,
    /// The time now.
    pub now: i64,
    /// When the last frame arrived.
    pub last_update: i64,
    /// The viewer's offset from UTC, in minutes.
    pub utc_offset_minutes: i32,
}

/// The live indicator's state, detail and menu action. A settled run wins over everything: its
/// feed is over, so there is nothing to pause. Otherwise a lost socket wins over a pause, and a
/// pause over the socket's own state.
#[must_use]
pub fn live_view(i: LiveInputs) -> LiveView {
    let state = if i.settled {
        LiveState::Settled
    } else if i.connection == Connection::Reconnecting {
        LiveState::Reconnecting
    } else if i.paused {
        LiveState::Paused
    } else if i.connection == Connection::Live {
        LiveState::Live
    } else {
        LiveState::Connecting
    };
    let detail = match state {
        LiveState::Live => format!("updated {} s ago", ((i.now - i.last_update) / 1000).max(0)),
        LiveState::Reconnecting => format!(
            "showing data from {}",
            time_of_day(i.last_update, i.utc_offset_minutes)
        ),
        LiveState::Settled => i
            .settled_at
            .map_or_else(String::new, |t| time_of_day(t, i.utc_offset_minutes)),
        LiveState::Connecting | LiveState::Paused => String::new(),
    };
    let action = match state {
        LiveState::Live => Some("Pause".to_owned()),
        LiveState::Paused if i.held == 0 => Some("Resume updates".to_owned()),
        LiveState::Paused => Some(format!("Show {} updates", i.held)),
        LiveState::Connecting | LiveState::Reconnecting | LiveState::Settled => None,
    };
    LiveView {
        state,
        detail,
        action,
    }
}

/// The Maestro result across jobs: running while any is open, else the worst outcome, with the
/// weakest evidence any closed job rests on.
fn result(jobs: &[&JobView]) -> (Pill, Provenance) {
    let counts = JobCounts {
        starting: count(jobs, "starting"),
        running: count(jobs, "running"),
        finalizing: count(jobs, "finalizing"),
        closed: count(jobs, "closed"),
        pending: 0,
    };
    if jobs.is_empty() {
        return (attempt_pill(&counts), Provenance::Missing);
    }
    if jobs.iter().any(|j| j.lifecycle != "closed") {
        return (attempt_pill(&counts), Provenance::Provisional);
    }
    let worst = jobs
        .iter()
        .map(|j| j.outcome.as_deref())
        .max_by_key(|o| severity(*o))
        .flatten();
    let weakest = jobs
        .iter()
        .map(|j| source_provenance(j.outcome_source.as_deref()))
        .min_by_key(|p| strength(*p))
        .unwrap_or(Provenance::Missing);
    (job_pill("closed", worst), weakest)
}

fn count(jobs: &[&JobView], lifecycle: &str) -> i64 {
    i64::try_from(jobs.iter().filter(|j| j.lifecycle == lifecycle).count()).unwrap_or(i64::MAX)
}

/// How bad a closed job's outcome is; the worst one names the attempt.
fn severity(outcome: Option<&str>) -> u8 {
    match outcome {
        Some("failed") => 5,
        Some("interrupted") => 4,
        Some("completed") => 1,
        Some("cancelled") => 2,
        _ => 3,
    }
}

/// How much a provenance can be trusted; the weakest one describes a whole.
fn strength(p: Provenance) -> u8 {
    match p {
        Provenance::Missing => 0,
        Provenance::Stale => 1,
        Provenance::Inferred => 2,
        Provenance::Provisional => 3,
        Provenance::Observed => 4,
        Provenance::Derived => 5,
        Provenance::Final => 6,
    }
}

/// A flow that failed or was cut short.
#[derive(Debug, Clone, PartialEq)]
pub struct FailureRow {
    /// The flow execution.
    pub flow_id: Uuid,
    /// Its name.
    pub name: String,
    /// `Failed` or `Interrupted`.
    pub state: Pill,
    /// Why, redacted by the server.
    pub message: Option<String>,
    /// How settled that is.
    pub provenance: Provenance,
    /// Where its page is.
    pub href: String,
}

/// The key a failure row is reused under: the view rebuilds the row when any of these change.
#[must_use]
pub fn failure_key(row: &FailureRow) -> (Uuid, Option<String>, &'static str, Provenance) {
    (
        row.flow_id,
        row.message.clone(),
        row.state.label,
        row.provenance,
    )
}

/// The attempt's failed and interrupted flows, by name, each linking to its investigation page.
#[must_use]
pub fn failures(state: &AttemptState, project: &str) -> Vec<FailureRow> {
    let mut rows: Vec<FailureRow> = state
        .flows()
        .filter(|f| matches!(f.state.as_str(), "failed" | "interrupted"))
        .map(|f| FailureRow {
            flow_id: f.id,
            name: f.name.clone(),
            state: flow_state_pill(&f.state),
            message: f.failure_message.clone(),
            provenance: flow_provenance(&f.finality, &f.state, None),
            href: flow_href(project, f.id),
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.flow_id.cmp(&b.flow_id)));
    rows
}

#[cfg(test)]
mod tests {
    use fleet_protocol::live::LiveFrame;
    use fleet_ui::{live_store::LiveStore, mark::Mark};

    use super::{super::fixtures::*, *};

    fn state(frame: LiveFrame) -> AttemptState {
        let mut store = LiveStore::new();
        let _ = store.apply(frame);
        store.attempt(id(ATTEMPT)).unwrap().clone()
    }

    #[test]
    fn flows_group_under_their_suite_and_job_in_a_stable_order() {
        let s = state(snapshot(
            3,
            vec![
                job(11, "b-job", "running", None, None),
                job(JOB, "a-job", "running", None, None),
            ],
            vec![run(RUN, JOB, "online")],
            vec![suite(SUITE, JOB, Some(3)), suite(31, 11, None)],
            vec![
                flow(52, SUITE, "second", "pending", "provisional"),
                flow(51, SUITE, "first", "running", "provisional"),
                flow(53, 31, "other-job", "passed", "verified"),
                flow(54, 99, "lost", "passed", "verified"),
            ],
        ));
        let tree = Tree::build(&s);
        let jobs: Vec<Uuid> = tree.jobs.iter().map(|j| j.id).collect();
        assert_eq!(jobs, [id(JOB), id(11)], "by job key: a-job before b-job");
        assert_eq!(tree.jobs[0].runs, [id(RUN)]);
        assert_eq!(tree.jobs[0].suites.len(), 1);
        assert_eq!(
            tree.jobs[0].suites[0].flows,
            [id(51), id(52)],
            "by ordinal, though 52 arrived first"
        );
        assert_eq!(tree.jobs[1].suites[0].flows, [id(53)]);
        assert_eq!(
            tree.orphan_flows,
            [id(54)],
            "a flow whose suite is unknown is kept, not dropped"
        );
    }

    #[test]
    fn the_result_is_running_while_a_job_is_open() {
        let s = state(snapshot(
            1,
            vec![
                job(JOB, "a", "closed", Some("failed"), Some("junit_final")),
                job(11, "b", "running", None, None),
            ],
            vec![],
            vec![],
            vec![],
        ));
        let h = header(&s);
        assert_eq!((h.result.label, h.result.mark), ("Running", Mark::Running));
    }

    #[test]
    fn the_closed_result_is_the_worst_job_outcome_with_the_weakest_evidence() {
        let s = state(snapshot(
            1,
            vec![
                job(JOB, "a", "closed", Some("completed"), Some("junit_final")),
                job(11, "b", "closed", Some("failed"), Some("process_exit")),
            ],
            vec![],
            vec![],
            vec![],
        ));
        let h = header(&s);
        assert_eq!((h.result.label, h.result.mark), ("Failed", Mark::Failed));
        assert_eq!(
            h.result_provenance,
            Provenance::Observed,
            "the exit code alone is weaker than the report"
        );
    }

    #[test]
    fn a_header_names_the_repository_ref_and_short_commit() {
        let s = state(snapshot(1, vec![], vec![], vec![], vec![]));
        let h = header(&s);
        assert_eq!(h.title, "acme/app");
        assert_eq!(h.git_ref.as_deref(), Some("main"));
        assert_eq!(h.commit.as_deref(), Some("0123456"));
        assert_eq!(h.attempt_number, 2);
        assert_eq!(h.started_ms, 1_767_225_600_000);
        assert_eq!(
            h.ci_url.as_deref(),
            Some("https://github.com/acme/app/actions/runs/9001")
        );
        assert_eq!(h.result.label, "No jobs yet");
    }

    #[test]
    fn failures_list_failed_and_interrupted_flows_with_a_link_and_their_reason() {
        let mut failed = flow(52, SUITE, "checkout", "failed", "verified");
        failed.failure_message = Some("Assertion is false".into());
        let s = state(snapshot(
            1,
            vec![job(JOB, "a", "running", None, None)],
            vec![],
            vec![suite(SUITE, JOB, None)],
            vec![
                flow(51, SUITE, "login", "passed", "verified"),
                failed,
                flow(53, SUITE, "attach", "interrupted", "inferred"),
                flow(54, SUITE, "wait", "running", "provisional"),
            ],
        ));
        let rows = failures(&s, "app");
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["attach", "checkout"]);
        assert_eq!(rows[1].state.label, "Failed");
        assert_eq!(rows[1].message.as_deref(), Some("Assertion is false"));
        assert_eq!(rows[1].provenance, Provenance::Final);
        assert_eq!(
            rows[1].href,
            "/flows/00000000-0000-0000-0000-000000000034?project=app"
        );
        assert_eq!(rows[0].provenance, Provenance::Inferred);
    }

    #[test]
    fn a_failure_whose_provenance_settles_gets_a_new_key_so_its_row_is_rebuilt() {
        let at = |finality: &str| {
            let mut failed = flow(52, SUITE, "checkout", "failed", finality);
            failed.failure_message = Some("Assertion is false".into());
            state(snapshot(
                1,
                vec![job(JOB, "a", "running", None, None)],
                vec![],
                vec![suite(SUITE, JOB, None)],
                vec![failed],
            ))
        };
        let provisional = failures(&at("provisional"), "app");
        let verified = failures(&at("verified"), "app");
        assert_eq!(provisional[0].provenance, Provenance::Provisional);
        assert_eq!(verified[0].provenance, Provenance::Final);
        assert_ne!(failure_key(&provisional[0]), failure_key(&verified[0]));
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    const T0: i64 = 1_700_000_000_000; // 22:13:20 UTC

    fn inputs() -> LiveInputs {
        LiveInputs {
            connection: Connection::Live,
            paused: false,
            held: 0,
            settled: false,
            settled_at: None,
            now: T0 + 3_000,
            last_update: T0,
            utc_offset_minutes: 0,
        }
    }

    #[test]
    fn a_live_page_says_how_long_since_the_last_update_and_offers_pause() {
        let v = live_view(inputs());
        assert_eq!(v.state, LiveState::Live);
        assert_eq!(v.detail, "updated 3 s ago");
        assert_eq!(v.action.as_deref(), Some("Pause"));
    }

    #[test]
    fn a_clock_behind_the_last_frame_never_reads_negative() {
        let v = live_view(LiveInputs {
            now: T0 - 5_000,
            ..inputs()
        });
        assert_eq!(v.detail, "updated 0 s ago");
    }

    #[test]
    fn a_pause_holds_the_page_and_offers_to_show_what_was_held() {
        let v = live_view(LiveInputs {
            paused: true,
            held: 12,
            ..inputs()
        });
        assert_eq!(v.state, LiveState::Paused);
        assert_eq!(v.detail, "");
        assert_eq!(v.action.as_deref(), Some("Show 12 updates"));
        let none_held = live_view(LiveInputs {
            paused: true,
            ..inputs()
        });
        assert_eq!(none_held.action.as_deref(), Some("Resume updates"));
    }

    #[test]
    fn a_lost_socket_shows_the_time_of_the_data_on_screen_and_no_menu() {
        let v = live_view(LiveInputs {
            connection: Connection::Reconnecting,
            paused: true,
            ..inputs()
        });
        assert_eq!(v.state, LiveState::Reconnecting);
        assert_eq!(v.detail, "showing data from 22:13:20");
        assert_eq!(v.action, None);
    }

    #[test]
    fn a_settled_run_says_when_it_settled_and_has_no_pause() {
        let v = live_view(LiveInputs {
            settled: true,
            paused: true,
            held: 4,
            settled_at: Some(T0 + 60_000),
            connection: Connection::Reconnecting,
            ..inputs()
        });
        assert_eq!(v.state, LiveState::Settled);
        assert_eq!(v.detail, "22:14:20");
        assert_eq!(v.action, None);
    }

    #[test]
    fn the_first_load_is_connecting_with_nothing_to_add() {
        let v = live_view(LiveInputs {
            connection: Connection::Connecting,
            ..inputs()
        });
        assert_eq!(
            v,
            LiveView {
                state: LiveState::Connecting,
                detail: String::new(),
                action: None,
            }
        );
    }
}
