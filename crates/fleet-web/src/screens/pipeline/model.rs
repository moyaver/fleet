// SPDX-License-Identifier: AGPL-3.0-only
//! What screen B shows of an attempt, computed from the live store's state: the jobs → suites →
//! flows tree, the header with the Maestro result, and the failures list.

use std::collections::HashSet;

use fleet_protocol::{
    live::{AgentRunView, FlowView, JobView, SuiteView},
    rest::JobCounts,
};
use fleet_ui::{
    components::live::LiveState, live_store::AttemptState, mark::Mark, provenance::Provenance,
};
use uuid::Uuid;

use crate::{
    clock::unix_ms,
    format::{ref_name, short_commit, time_of_day},
    live::socket::Connection,
    paths::{ci_run_url, flow_href},
    status::{Pill, attempt_pill, flow_provenance, flow_state_pill},
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
    /// The CI run key, which names the page.
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
    /// The verdict, derived from the flows: the rules are in `verdict`.
    pub result: Pill,
    /// How the verdict is known: provisional while open, derived once closed.
    pub result_provenance: Provenance,
    /// How many flows the verdict was derived from.
    pub flows: usize,
    /// Whether every job has closed: no further updates will come for this attempt.
    pub settled: bool,
}

/// The header of an attempt.
#[must_use]
pub fn header(state: &AttemptState) -> Header {
    let at = state.attempt();
    let jobs: Vec<&JobView> = state.jobs().collect();
    let flows: Vec<&FlowView> = state.flows().collect();
    let (result, result_provenance) = verdict(&jobs, &flows, at.planned_total);
    let settled = !jobs.is_empty() && jobs.iter().all(|j| j.lifecycle == "closed");
    Header {
        title: at.run_key.clone(),
        git_ref: at.git_ref.as_deref().map(|r| ref_name(r).to_owned()),
        commit: at.commit_sha.as_deref().map(short_commit),
        attempt_number: at.attempt,
        started_ms: unix_ms(at.first_seen_at),
        ci_url: ci_run_url(&at.provider, at.repository.as_deref(), &at.run_key),
        result,
        result_provenance,
        flows: flows.len(),
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
        // No finish time is on the attempt or job data, so the word alone is shown: a time the
        // page merely saw would be a made-up number.
        LiveState::Settled | LiveState::Connecting | LiveState::Paused => String::new(),
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

/// The pipeline's verdict, derived from its flows (Foundations rule 4, issue #149): Failed if any
/// flow failed, whether or not the jobs have closed; Passed only when a plan's total is known,
/// every planned flow has been seen and each one is Verified Passed; otherwise No failures. While
/// a job is still open and no flow has failed, it reads Running and is provisional. Always derived,
/// never a verdict the job outcome sets.
fn verdict(jobs: &[&JobView], flows: &[&FlowView], planned: Option<i64>) -> (Pill, Provenance) {
    if jobs.is_empty() {
        return (attempt_pill(&JobCounts::default()), Provenance::Missing);
    }
    let pill = if flows.iter().any(|f| f.state == "failed") {
        Pill {
            label: "Failed",
            mark: Mark::Failed,
        }
    } else if jobs.iter().any(|j| j.lifecycle != "closed") {
        return (
            Pill {
                label: "Running",
                mark: Mark::Running,
            },
            Provenance::Provisional,
        );
    } else if accounted_for(flows, planned) {
        Pill {
            label: "Passed",
            mark: Mark::Passed,
        }
    } else {
        Pill {
            label: "No failures",
            mark: Mark::End,
        }
    };
    (pill, Provenance::Derived)
}

/// Whether a plan's total is known and every planned flow is seen and Verified Passed.
fn accounted_for(flows: &[&FlowView], planned: Option<i64>) -> bool {
    planned.is_some_and(|total| {
        total > 0
            && i64::try_from(flows.len()).is_ok_and(|seen| seen >= total)
            && flows.iter().all(|f| {
                f.state == "passed"
                    && flow_provenance(&f.finality, &f.state, None) == Provenance::Final
            })
    })
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

/// The message a failure is grouped under: whitespace runs collapse to one space, so a message
/// that wraps differently still groups with its twins. Empty when no message was reported.
#[must_use]
pub fn normalised_message(message: Option<&str>) -> String {
    message
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The failures grouped by normalised message, groups in message order and each group's flows
/// in name order (the order [`failures`] gives them).
#[must_use]
pub fn message_groups(rows: &[FailureRow]) -> Vec<(String, Vec<FailureRow>)> {
    let mut groups: Vec<(String, Vec<FailureRow>)> = Vec::new();
    for row in rows {
        let key = normalised_message(row.message.as_deref());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, flows)) => flows.push(row.clone()),
            None => groups.push((key, vec![row.clone()])),
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    groups
}

/// What to say about the warnings an agent run's files raised (issue #148), one line per code.
#[must_use]
pub fn file_warning_notes(codes: &[String]) -> Vec<&'static str> {
    let mut out = Vec::new();
    if codes.iter().any(|c| c == "commands_flow_unnamed") {
        out.push(
            "A commands.json names a flow the log never started, so its steps are not settled; no flow page shows it.",
        );
    }
    out
}

/// The provenance a row shows, when it differs from its group's. A group is Final once the run
/// has settled and Provisional while it is open. A flow not yet run is a dash while the run is
/// open, never Missing (Foundations, rule 2).
#[must_use]
pub fn row_mark(provenance: Provenance, settled: bool) -> Option<Provenance> {
    let group = if settled {
        Provenance::Final
    } else {
        Provenance::Provisional
    };
    match provenance {
        Provenance::Missing if !settled => None,
        p if p == group => None,
        p => Some(p),
    }
}

/// The state a flow is filtered and counted under: one of the seven states the pipeline's
/// readouts show. A state this build does not know is `unknown`, never passed.
#[must_use]
pub fn bucket(state: &str) -> &'static str {
    match state {
        "passed" => "passed",
        "failed" => "failed",
        "running" => "running",
        "interrupted" => "interrupted",
        "pending" => "pending",
        "skipped" => "skipped",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use fleet_protocol::live::LiveFrame;
    use fleet_ui::live_store::LiveStore;

    use super::{super::fixtures::*, *};

    fn state(frame: LiveFrame) -> AttemptState {
        let mut store = LiveStore::new();
        let _ = store.apply(frame);
        store.attempt(id(ATTEMPT)).unwrap().clone()
    }

    fn failure_row(f: &fleet_protocol::live::FlowView) -> FailureRow {
        FailureRow {
            flow_id: f.id,
            name: f.name.clone(),
            state: flow_state_pill(&f.state),
            message: f.failure_message.clone(),
            provenance: flow_provenance(&f.finality, &f.state, None),
            href: flow_href("app", f.id),
        }
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
    fn a_closed_run_with_a_failed_flow_is_failed_and_derived_from_its_flows() {
        let mut failed = flow(52, SUITE, "checkout", "failed", "verified");
        failed.failure_message = Some("Assertion is false".into());
        let s = state(snapshot(
            1,
            vec![
                job(JOB, "a", "closed", Some("completed"), Some("junit_final")),
                job(11, "b", "closed", Some("failed"), Some("process_exit")),
            ],
            vec![],
            vec![suite(SUITE, JOB, None)],
            vec![flow(51, SUITE, "login", "passed", "verified"), failed],
        ));
        let h = header(&s);
        assert_eq!((h.result.label, h.result.mark), ("Failed", Mark::Failed));
        assert_eq!(h.result_provenance, Provenance::Derived);
        assert_eq!(h.flows, 2);
    }

    #[test]
    fn the_job_outcome_does_not_decide_the_verdict_the_flows_do() {
        let s = state(snapshot(
            1,
            vec![job(
                JOB,
                "a",
                "closed",
                Some("failed"),
                Some("process_exit"),
            )],
            vec![],
            vec![suite(SUITE, JOB, None)],
            vec![flow(51, SUITE, "login", "passed", "verified")],
        ));
        let h = header(&s);
        assert_eq!(
            (h.result.label, h.result.mark),
            ("No failures", Mark::End),
            "no plan and no failed flow: the job's failure is shown separately"
        );
    }

    #[test]
    fn a_failed_flow_reads_failed_even_while_the_pipeline_is_still_open() {
        let open_with = |flows: Vec<FlowView>| {
            header(&state(snapshot(
                1,
                vec![job(JOB, "a", "running", None, None)],
                vec![],
                vec![suite(SUITE, JOB, None)],
                flows,
            )))
        };
        let failed = open_with(vec![flow(51, SUITE, "login", "failed", "verified")]);
        assert_eq!(
            (failed.result.label, failed.result.mark),
            ("Failed", Mark::Failed),
            "the flows decide the verdict; open or closed"
        );
        let clean = open_with(vec![flow(52, SUITE, "wait", "passed", "verified")]);
        assert_eq!(
            (clean.result.label, clean.result.mark),
            ("Running", Mark::Running),
            "open with no failure yet: still running, never Passed"
        );
    }

    #[test]
    fn a_closed_run_passes_only_when_a_plan_total_is_accounted_for_by_verified_passes() {
        let jobs = || {
            vec![job(
                JOB,
                "a",
                "closed",
                Some("completed"),
                Some("junit_final"),
            )]
        };
        let verified = || {
            vec![
                flow(51, SUITE, "login", "passed", "verified"),
                flow(52, SUITE, "wait", "passed", "verified"),
            ]
        };
        let mut planned = snapshot(1, jobs(), vec![], vec![suite(SUITE, JOB, None)], verified());
        if let LiveFrame::Snapshot { snapshot } = &mut planned {
            snapshot.attempt.planned_total = Some(2);
        }
        assert_eq!(header(&state(planned)).result.label, "Passed");

        let unplanned = state(snapshot(
            1,
            jobs(),
            vec![],
            vec![suite(SUITE, JOB, None)],
            verified(),
        ));
        assert_eq!(header(&unplanned).result.label, "No failures");

        let mut short = snapshot(1, jobs(), vec![], vec![suite(SUITE, JOB, None)], verified());
        if let LiveFrame::Snapshot { snapshot } = &mut short {
            snapshot.attempt.planned_total = Some(3);
        }
        assert_eq!(
            header(&state(short)).result.label,
            "No failures",
            "a planned flow not seen yet means not every flow is accounted for"
        );
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
    fn failures_group_by_their_message_with_whitespace_normalised() {
        let at = |n: u128, message: &str| {
            let mut f = flow(n, SUITE, &format!("flow {n}"), "failed", "verified");
            f.failure_message = Some(message.into());
            failure_row(&f)
        };
        let rows = vec![
            at(51, "Element not found:  \"Order total\""),
            at(52, "Timed out after 15 s"),
            at(53, "Element not found:\n\"Order total\""),
        ];
        let groups = message_groups(&rows);
        let keys: Vec<&str> = groups.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["Element not found: \"Order total\"", "Timed out after 15 s"]
        );
        assert_eq!(groups[0].1.len(), 2, "the two spellings are one group");
    }

    #[test]
    fn a_failure_without_a_message_groups_under_the_empty_message() {
        let f = flow(51, SUITE, "silent", "interrupted", "inferred");
        let groups = message_groups(&[failure_row(&f)]);
        assert_eq!(groups[0].0, "");
    }

    #[test]
    fn a_row_marks_only_what_differs_from_its_group() {
        assert_eq!(row_mark(Provenance::Provisional, false), None);
        assert_eq!(row_mark(Provenance::Final, true), None, "settled is silent");
        assert_eq!(
            row_mark(Provenance::Inferred, true),
            Some(Provenance::Inferred),
            "an interrupted flow the report did not decide says so"
        );
        assert_eq!(
            row_mark(Provenance::Stale, false),
            Some(Provenance::Stale),
            "a running flow on a quiet agent"
        );
    }

    #[test]
    fn a_flow_not_yet_run_is_a_dash_while_open_and_missing_once_settled() {
        assert_eq!(row_mark(Provenance::Missing, false), None);
        assert_eq!(
            row_mark(Provenance::Missing, true),
            Some(Provenance::Missing)
        );
    }

    #[test]
    fn every_flow_state_falls_in_one_of_seven_readout_buckets() {
        assert_eq!(bucket("pending"), "pending");
        assert_eq!(bucket("interrupted"), "interrupted");
        assert_eq!(bucket("something-new"), "unknown", "never passed");
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
    fn a_settled_run_says_settled_with_no_made_up_time_and_no_pause() {
        let v = live_view(LiveInputs {
            settled: true,
            paused: true,
            held: 4,
            connection: Connection::Reconnecting,
            ..inputs()
        });
        assert_eq!(
            v,
            LiveView {
                state: LiveState::Settled,
                detail: String::new(),
                action: None,
            }
        );
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

    /// A run whose commands file names no flow the log started says so once, and a run with no
    /// such file says nothing (issue #148).
    #[test]
    fn an_unnamed_commands_file_is_said_once_and_nothing_else_is() {
        assert_eq!(file_warning_notes(&[]), Vec::<&str>::new());
        assert_eq!(
            file_warning_notes(&["steps_not_aligned".to_owned()]),
            Vec::<&str>::new()
        );
        let notes = file_warning_notes(&["commands_flow_unnamed".to_owned()]);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("names a flow the log never started"));
    }
}
