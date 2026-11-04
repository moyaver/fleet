// SPDX-License-Identifier: AGPL-3.0-only
//! The flow page's view-model: what `GET …/flows/{execution_id}` says, laid out for debugging.
//!
//! Every claim about the outcome is listed with its source (invariant I4), the winner marked and
//! disagreements highlighted. Steps are flat, because the journal doesn't carry nesting or
//! retries yet (issue #68). Evidence (screenshots, logs, reports) is [`super::evidence`].

use fleet_protocol::{
    event::StepStatus,
    rest::{ClaimView, FlowDetailView, StepView},
};
use fleet_ui::{
    charts::format::format_duration,
    provenance::Provenance,
    steps::tree::{StepExec, StepTree},
};

use crate::{
    clock::unix_ms,
    paths::{flow_href, pipeline_href, worker_href},
    screens::{
        common::{Ctx, Fact, finality_note, parse_source, source_label},
        device::model::DeviceSessionRow,
        flow::evidence::StepKey,
    },
    status::{Pill, flow_provenance, flow_state_pill, source_provenance},
};

/// A link to another page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// What it says.
    pub label: &'static str,
    /// Where it goes.
    pub href: String,
}

/// What the flow did and how sure we are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowHeader {
    /// The flow's name.
    pub title: String,
    /// Its file.
    pub path: Option<String>,
    /// Its state.
    pub state: Pill,
    /// What the state rests on (invariant I4).
    pub provenance: Provenance,
    /// How settled it is, in words.
    pub finality_note: &'static str,
    /// Set when two parsed sources disagree: a Maestro compatibility alarm.
    pub conflict_warning: Option<&'static str>,
    /// Start, end and duration.
    pub facts: Vec<Fact>,
}

/// One source's claim about the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRow {
    /// Where the evidence came from.
    pub source: &'static str,
    /// How much that source is worth (invariant I4).
    pub provenance: Provenance,
    /// What it claimed.
    pub outcome: Pill,
    /// When the source observed it.
    pub observed: Option<String>,
    /// When the server stored it.
    pub recorded: String,
    /// The parser and Maestro profile that produced it.
    pub parser: Option<String>,
    /// This claim decided the displayed outcome.
    pub winning: bool,
    /// It contradicts the winning claim: highlight it.
    pub disagrees: bool,
}

/// All the evidence about the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimsSection {
    /// Nothing has been claimed; the text says so.
    Empty(&'static str),
    /// The claims, in arrival order.
    Claims(Vec<ClaimRow>),
}

/// A step, by position and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRef {
    /// Its position among the steps as observed.
    pub index: i32,
    /// Its description.
    pub label: String,
}

/// Why the flow failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureDetails {
    /// The failure message, redacted; `None` when none was reported.
    pub message: Option<String>,
    /// The first failed step.
    pub failed_step: Option<StepRef>,
    /// The last step that completed before it.
    pub last_successful_step: Option<StepRef>,
}

/// Whether the flow failed, and the details when it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// It has not failed.
    NotFailed,
    /// It failed; what is known.
    Failed(FailureDetails),
}

/// The step tree and what to say about it.
#[derive(Debug, Clone)]
pub struct StepsSection {
    /// The steps.
    pub tree: StepTree,
    /// How many, and whether the total is known.
    pub summary: String,
    /// A caveat about how the steps are listed, when there are steps.
    pub note: Option<&'static str>,
}

/// The CI run, job and runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiSection {
    /// What it is.
    pub facts: Vec<Fact>,
    /// Pages for the pipeline and the worker.
    pub links: Vec<Link>,
}

/// An earlier execution of the same flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousRow {
    /// Its state.
    pub state: Pill,
    /// What the state rests on.
    pub provenance: Provenance,
    /// When it started.
    pub started: String,
    /// How long it took.
    pub duration: Option<String>,
    /// Its page.
    pub href: String,
    /// Its pipeline.
    pub pipeline_href: String,
}

/// Screen D, laid out.
#[derive(Debug, Clone)]
pub struct FlowPage {
    /// Result and timing.
    pub header: FlowHeader,
    /// CI environment.
    pub ci: CiSection,
    /// The device it ran on; `None` when no event named one.
    pub device: Option<DeviceSessionRow>,
    /// Every claim about the outcome.
    pub claims: ClaimsSection,
    /// Why it failed.
    pub failure: Failure,
    /// The steps.
    pub steps: StepsSection,
    /// Earlier executions of the same flow, newest first.
    pub previous: Vec<PreviousRow>,
}

fn step_status(status: &str) -> StepStatus {
    match status {
        "running" => StepStatus::Running,
        "completed" => StepStatus::Completed,
        "failed" => StepStatus::Failed,
        "warned" => StepStatus::Warned,
        "skipped" => StepStatus::Skipped,
        _ => StepStatus::Unknown,
    }
}

fn step_label(s: &StepView) -> String {
    s.label
        .clone()
        .unwrap_or_else(|| format!("Step {}", s.index))
}

fn step_ref(steps: &[StepView], index: i32) -> StepRef {
    StepRef {
        index,
        label: steps
            .iter()
            .find(|s| s.index == index)
            .map_or_else(|| format!("Step {index}"), step_label),
    }
}

/// The steps and the failed step as [`evidence`](super::evidence) needs them: by id, with
/// Maestro's description only when the server has one.
#[must_use]
pub fn evidence_steps(view: &FlowDetailView) -> (Vec<StepKey>, Option<StepKey>) {
    let steps: Vec<StepKey> = view
        .steps
        .iter()
        .map(|s| StepKey {
            id: s.index.to_string(),
            label: s.label.clone(),
        })
        .collect();
    let failed = view
        .failure
        .as_ref()
        .and_then(|f| f.failed_step_index)
        .map(|i| {
            let id = i.to_string();
            steps
                .iter()
                .find(|s| s.id == id)
                .cloned()
                .unwrap_or(StepKey { id, label: None })
        });
    (steps, failed)
}

fn provider(p: &str) -> &str {
    match p {
        "github_actions" => "GitHub Actions",
        "generic" => "Generic CI",
        other => other,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

impl FlowPage {
    /// Lays out `view`.
    #[must_use]
    pub fn build(view: &FlowDetailView, ctx: &Ctx) -> Self {
        Self {
            header: header(view, ctx),
            ci: ci(view, ctx),
            device: view
                .device
                .as_ref()
                .map(|d| DeviceSessionRow::build(d, ctx)),
            claims: claims(&view.claims, ctx),
            failure: failure(view),
            steps: steps(view),
            previous: view
                .previous
                .iter()
                .map(|p| PreviousRow {
                    state: flow_state_pill(&p.state),
                    provenance: flow_provenance(&p.finality, &p.state, None),
                    started: ctx.time(unix_ms(p.started_at)),
                    duration: p.duration_ms.map(format_duration),
                    href: flow_href(&ctx.project, p.flow_id),
                    pipeline_href: pipeline_href(&ctx.project, p.pipeline_attempt_id),
                })
                .collect(),
        }
    }
}

fn header(view: &FlowDetailView, ctx: &Ctx) -> FlowHeader {
    let f = &view.flow;
    let started = view.started_at.map(unix_ms);
    let duration = if f.state == "running" {
        started.map(|s| format!("{} so far", format_duration(ctx.now_ms.saturating_sub(s))))
    } else {
        f.duration_ms.map(format_duration)
    };
    FlowHeader {
        title: f.name.clone(),
        path: f.path.clone(),
        state: flow_state_pill(&f.state),
        provenance: flow_provenance(&f.finality, &f.state, view.agent_run.liveness),
        finality_note: finality_note(&f.finality),
        conflict_warning: f.has_conflict.then_some(
            "Two parsed sources disagree about this flow: possible Maestro compatibility problem",
        ),
        facts: vec![
            Fact::maybe("Started", started.map(|t| ctx.time(t))),
            Fact::maybe("Ended", view.ended_at.map(|t| ctx.time(unix_ms(t)))),
            Fact::maybe("Duration", duration),
        ],
    }
}

fn ci(view: &FlowDetailView, ctx: &Ctx) -> CiSection {
    let a = &view.attempt;
    let mut links = vec![Link {
        label: "Pipeline",
        href: pipeline_href(&ctx.project, a.id),
    }];
    if let Some(w) = view.agent_run.worker_id {
        links.push(Link {
            label: "Worker",
            href: worker_href(&ctx.project, w),
        });
    }
    CiSection {
        facts: vec![
            Fact::maybe("Repository", a.repository.clone()),
            Fact::maybe(
                "Commit",
                a.commit_sha
                    .as_ref()
                    .map(|c| c.chars().take(7).collect::<String>()),
            ),
            Fact::maybe("Ref", a.git_ref.clone()),
            Fact::known("CI provider", provider(&a.provider)),
            Fact::known("CI run", format!("{} (attempt {})", a.run_key, a.attempt)),
            Fact::known(
                "Job",
                view.job
                    .name
                    .clone()
                    .unwrap_or_else(|| view.job.job_key.clone()),
            ),
            Fact::maybe(
                "Shard",
                view.flow
                    .shard_number
                    .map(|n| format!("{n} of {}", view.suite.shard_count)),
            ),
            Fact::maybe("Runner", view.agent_run.runner_name.clone()),
        ],
        links,
    }
}

fn claims(claims: &[ClaimView], ctx: &Ctx) -> ClaimsSection {
    if claims.is_empty() {
        return ClaimsSection::Empty("No source has claimed an outcome for this flow yet");
    }
    let winner = claims
        .iter()
        .find(|c| c.winning)
        .map(|c| c.outcome.as_str());
    ClaimsSection::Claims(
        claims
            .iter()
            .map(|c| {
                let source = parse_source(&c.source);
                ClaimRow {
                    source: source_label(source),
                    provenance: source_provenance(Some(&c.source)),
                    outcome: flow_state_pill(&c.outcome),
                    observed: c.observed_at.map(|t| ctx.time(unix_ms(t))),
                    recorded: ctx.time(unix_ms(c.recorded_at)),
                    parser: c.parser.clone(),
                    winning: c.winning,
                    disagrees: !c.winning && winner.is_some_and(|w| w != c.outcome),
                }
            })
            .collect(),
    )
}

fn failure(view: &FlowDetailView) -> Failure {
    if view.flow.state != "failed" && view.failure.is_none() {
        return Failure::NotFailed;
    }
    let f = view.failure.as_ref();
    Failure::Failed(FailureDetails {
        message: f
            .and_then(|f| f.message.clone())
            .or_else(|| view.flow.failure_message.clone()),
        failed_step: f
            .and_then(|f| f.failed_step_index)
            .map(|i| step_ref(&view.steps, i)),
        last_successful_step: f
            .and_then(|f| f.last_successful_step_index)
            .map(|i| step_ref(&view.steps, i)),
    })
}

fn steps(view: &FlowDetailView) -> StepsSection {
    let execs: Vec<StepExec> = view
        .steps
        .iter()
        .map(|s| StepExec {
            id: s.index.to_string(),
            parent: None,
            label: step_label(s),
            status: step_status(&s.status),
            source: parse_source(&s.source),
            duration_ms: s.duration_ms.and_then(|d| u64::try_from(d).ok()),
            attempt: 1,
        })
        .collect();
    let tree = StepTree::build(&execs);
    let n = view.steps.len();
    let open = matches!(view.flow.state.as_str(), "running" | "pending");
    let summary = match (n, open) {
        (0, true) => "No steps reported yet".to_owned(),
        (0, false) => "No steps were reported".to_owned(),
        (n, true) => format!(
            "{} so far; the total is unknown",
            plural(n, "step", "steps")
        ),
        (n, false) => plural(n, "step", "steps"),
    };
    StepsSection {
        tree,
        summary,
        note: (n > 0).then_some(
            "Listed in the order Maestro ran them. Nesting (runFlow) and retries are not reported yet.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use fleet_ui::tone::Tone;

    use super::*;

    const FAILED: &str = r#"{
      "flow": {"id": "00000000-0000-0000-0000-0000000000f2",
               "job_id": "00000000-0000-0000-0000-0000000000b1",
               "suite_execution_id": "00000000-0000-0000-0000-0000000000e1",
               "agent_run_id": "00000000-0000-0000-0000-000000000101",
               "device_session_id": "00000000-0000-0000-0000-0000000000d1",
               "shard_number": 1, "ordinal": 2, "name": "checkout",
               "path": "flows/checkout.yaml", "state": "failed",
               "outcome_source": "junit_final", "finality": "verified", "has_conflict": false,
               "failure_message": "Element not found: Done", "duration_ms": 12400,
               "steps": 3, "rev": 5},
      "started_at": "2026-01-01T10:10:00Z", "ended_at": "2026-01-01T10:10:12Z",
      "attempt": {"id": "00000000-0000-0000-0000-0000000000c1",
                  "project_id": "00000000-0000-0000-0000-000000000001",
                  "provider": "github_actions", "run_key": "acme/shop:700",
                  "repository": "acme/shop", "commit_sha": "0123456789abcdef0123",
                  "git_ref": "refs/heads/main", "attempt": 2, "version": 9,
                  "planned_total": 12, "jobs": 2, "open_jobs": 1,
                  "first_seen_at": "2026-01-01T09:55:00Z"},
      "job": {"id": "00000000-0000-0000-0000-0000000000b1",
              "pipeline_attempt_id": "00000000-0000-0000-0000-0000000000c1",
              "job_key": "e2e-shard-1", "name": "E2E (shard 1)", "lifecycle": "running",
              "outcome": null, "outcome_source": null, "reopened_count": 0, "rev": 3},
      "suite": {"id": "00000000-0000-0000-0000-0000000000e1",
                "job_id": "00000000-0000-0000-0000-0000000000b1",
                "agent_run_id": "00000000-0000-0000-0000-000000000101",
                "shard_mode": "split", "shard_count": 2, "planned_count": 6,
                "plan_source": "fleet_plan", "lifecycle": "running", "outcome": null,
                "outcome_source": null, "report_status": null, "exit_code": null, "rev": 4},
      "agent_run": {"id": "00000000-0000-0000-0000-000000000101",
                    "job_id": "00000000-0000-0000-0000-0000000000b1",
                    "worker_id": "00000000-0000-0000-0000-0000000000a1",
                    "runner_name": "mac-mini-3", "state": "active", "trust_level": "trusted",
                    "committed_seq": 40, "gaps": 0, "last_frame_at": "2026-01-01T10:59:55Z",
                    "late_resume": false, "plan_id": null, "plan_shard_id": null,
                    "maestro_exit_code": null, "rev": 3, "liveness": "online"},
      "device": {"device": {"id": "00000000-0000-0000-0000-0000000000d1",
                            "agent_run_id": "00000000-0000-0000-0000-000000000101",
                            "serial": "emulator-5554", "state": "busy", "rev": 4},
                 "worker_id": "00000000-0000-0000-0000-0000000000a1",
                 "runner_name": "mac-mini-3",
                 "pipeline_attempt_id": "00000000-0000-0000-0000-0000000000c1",
                 "job_id": "00000000-0000-0000-0000-0000000000b1", "job_key": "e2e-shard-1",
                 "first_seen_at": "2026-01-01T10:00:00Z", "last_seen_at": "2026-01-01T10:59:58Z",
                 "profile": {"platform": "android", "kind": "emulator", "model": null,
                             "avd_or_runtime": "Pixel_7_API_34", "os_version": "14",
                             "api_level": 34, "abi": "arm64-v8a"}},
      "claims": [
        {"source": "live_console", "outcome": "failed", "observed_at": "2026-01-01T10:10:12Z",
         "recorded_at": "2026-01-01T10:10:12Z", "parser": "maestro-console@2.1", "winning": false},
        {"source": "junit_final", "outcome": "failed", "observed_at": null,
         "recorded_at": "2026-01-01T10:20:00Z", "parser": "maestro-junit@2.1", "winning": true}
      ],
      "steps": [
        {"index": 0, "label": "Launch app", "status": "completed", "source": "debug_log",
         "started_at": "2026-01-01T10:10:00Z", "ended_at": "2026-01-01T10:10:02Z",
         "duration_ms": 2000},
        {"index": 1, "label": "Tap on \"Pay\"", "status": "completed", "source": "debug_log",
         "started_at": "2026-01-01T10:10:02Z", "ended_at": "2026-01-01T10:10:05Z",
         "duration_ms": 3000},
        {"index": 2, "label": "Assert visible \"Done\"", "status": "failed", "source": "debug_log",
         "started_at": "2026-01-01T10:10:05Z", "ended_at": "2026-01-01T10:10:12Z",
         "duration_ms": 7000}
      ],
      "failure": {"message": "Element not found: Done", "failed_step_index": 2,
                  "last_successful_step_index": 1},
      "previous": [
        {"flow_id": "00000000-0000-0000-0000-0000000000f0",
         "pipeline_attempt_id": "00000000-0000-0000-0000-0000000000c0", "state": "passed",
         "outcome_source": "junit_final", "finality": "verified",
         "started_at": "2025-12-31T10:10:00Z", "duration_ms": 9000}
      ],
      "artifacts": []
    }"#;

    fn ctx() -> Ctx {
        Ctx {
            project: "shop".to_owned(),
            now_ms: 1_767_265_200_000, // 2026-01-01T11:00:00Z
            utc_offset_minutes: 0,
        }
    }

    fn view() -> FlowDetailView {
        serde_json::from_str(FAILED).unwrap()
    }

    fn page() -> FlowPage {
        FlowPage::build(&view(), &ctx())
    }

    #[test]
    fn the_header_gives_name_path_result_provenance_and_duration() {
        let h = page().header;
        assert_eq!(h.title, "checkout");
        assert_eq!(h.path.as_deref(), Some("flows/checkout.yaml"));
        assert_eq!((h.state.label, h.state.tone), ("Failed", Tone::Danger));
        assert_eq!(h.provenance, Provenance::Final);
        assert_eq!(
            h.finality_note,
            "Confirmed by the final test report or per-flow artifacts"
        );
        assert_eq!(h.conflict_warning, None);
        let facts: Vec<_> = h.facts.iter().map(|f| (f.label, f.shown())).collect();
        assert_eq!(
            facts,
            [
                ("Started", "2026-01-01 10:10:00"),
                ("Ended", "2026-01-01 10:10:12"),
                ("Duration", "12 s"),
            ]
        );
    }

    #[test]
    fn a_running_flow_shows_how_long_it_has_run_and_no_end() {
        let mut v = view();
        v.flow.state = "running".to_owned();
        v.flow.outcome_source = None;
        v.flow.finality = "provisional".to_owned();
        v.flow.duration_ms = None;
        v.ended_at = None;
        v.failure = None;
        v.claims.clear();
        let h = FlowPage::build(&v, &ctx()).header;
        assert_eq!(h.state.label, "Running");
        assert_eq!(h.provenance, Provenance::Provisional);
        let fact = |l| {
            h.facts
                .iter()
                .find(|f| f.label == l)
                .unwrap()
                .shown()
                .to_owned()
        };
        assert_eq!(fact("Ended"), "Not reported");
        assert_eq!(fact("Duration"), "50 min so far");
    }

    #[test]
    fn every_claim_is_listed_with_its_source_the_winner_marked_in_arrival_order() {
        let claims = page().claims;
        let ClaimsSection::Claims(rows) = claims else {
            panic!("two claims exist");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].source, "Live console");
        assert_eq!(rows[0].provenance, Provenance::Provisional);
        assert_eq!(rows[0].outcome.label, "Failed");
        assert_eq!(rows[0].observed.as_deref(), Some("2026-01-01 10:10:12"));
        assert_eq!(rows[0].parser.as_deref(), Some("maestro-console@2.1"));
        assert!(!rows[0].winning);
        assert_eq!(rows[1].source, "Final JUnit report");
        assert_eq!(rows[1].provenance, Provenance::Final);
        assert_eq!(rows[1].observed, None);
        assert_eq!(rows[1].recorded, "2026-01-01 10:20:00");
        assert!(rows[1].winning);
        // Both say failed: nothing to highlight.
        assert!(rows.iter().all(|r| !r.disagrees));
    }

    #[test]
    fn a_claim_that_contradicts_the_winner_is_highlighted_and_the_header_warns() {
        let mut v = view();
        v.flow.has_conflict = true;
        v.claims[0].outcome = "passed".to_owned();
        let page = FlowPage::build(&v, &ctx());
        let ClaimsSection::Claims(rows) = page.claims else {
            panic!("claims exist");
        };
        assert!(rows[0].disagrees);
        assert!(!rows[1].disagrees);
        assert_eq!(
            page.header.conflict_warning,
            Some(
                "Two parsed sources disagree about this flow: possible Maestro compatibility problem"
            )
        );
    }

    #[test]
    fn no_claims_is_an_explicit_empty_state() {
        let mut v = view();
        v.claims.clear();
        assert_eq!(
            FlowPage::build(&v, &ctx()).claims,
            ClaimsSection::Empty("No source has claimed an outcome for this flow yet")
        );
    }

    #[test]
    fn the_failure_names_the_message_the_failed_step_and_the_last_good_one() {
        let Failure::Failed(f) = page().failure else {
            panic!("the flow failed");
        };
        assert_eq!(f.message.as_deref(), Some("Element not found: Done"));
        assert_eq!(
            f.failed_step,
            Some(StepRef {
                index: 2,
                label: "Assert visible \"Done\"".to_owned()
            })
        );
        assert_eq!(
            f.last_successful_step,
            Some(StepRef {
                index: 1,
                label: "Tap on \"Pay\"".to_owned()
            })
        );
    }

    #[test]
    fn a_failed_flow_without_details_says_what_is_missing_and_a_passed_one_has_no_failure() {
        let mut v = view();
        v.failure = None;
        v.flow.failure_message = None;
        let Failure::Failed(f) = FlowPage::build(&v, &ctx()).failure else {
            panic!("failed state without details is still a failure");
        };
        assert_eq!(f.message, None);
        assert_eq!((f.failed_step, f.last_successful_step), (None, None));

        v.flow.state = "passed".to_owned();
        assert_eq!(FlowPage::build(&v, &ctx()).failure, Failure::NotFailed);
    }

    #[test]
    fn steps_are_a_flat_tree_in_order_with_status_source_and_duration() {
        let steps = page().steps;
        assert_eq!(steps.summary, "3 steps");
        assert_eq!(
            steps.note,
            Some(
                "Listed in the order Maestro ran them. Nesting (runFlow) and retries are not reported yet."
            )
        );
        let state = fleet_ui::steps::tree::TreeState::open_to_attention(&steps.tree);
        let rows = state.rows(&steps.tree);
        let shown: Vec<_> = rows
            .iter()
            .map(|r| (r.label.as_str(), r.level, r.duration.as_deref()))
            .collect();
        assert_eq!(
            shown,
            [
                ("Launch app", 1, Some("2.0 s")),
                ("Tap on \"Pay\"", 1, Some("3.0 s")),
                ("Assert visible \"Done\"", 1, Some("7.0 s")),
            ]
        );
    }

    #[test]
    fn a_running_flow_says_the_total_is_unknown_and_no_steps_is_explicit() {
        let mut v = view();
        v.flow.state = "running".to_owned();
        v.steps.truncate(2);
        assert_eq!(
            FlowPage::build(&v, &ctx()).steps.summary,
            "2 steps so far; the total is unknown"
        );
        v.steps.clear();
        let s = FlowPage::build(&v, &ctx()).steps;
        assert_eq!(s.summary, "No steps reported yet");
        assert_eq!(s.note, None);
    }

    #[test]
    fn the_ci_environment_and_device_are_spelled_out() {
        let p = page();
        let facts: Vec<_> = p.ci.facts.iter().map(|f| (f.label, f.shown())).collect();
        assert_eq!(
            facts,
            [
                ("Repository", "acme/shop"),
                ("Commit", "0123456"),
                ("Ref", "refs/heads/main"),
                ("CI provider", "GitHub Actions"),
                ("CI run", "acme/shop:700 (attempt 2)"),
                ("Job", "E2E (shard 1)"),
                ("Shard", "1 of 2"),
                ("Runner", "mac-mini-3"),
            ]
        );
        let links: Vec<_> =
            p.ci.links
                .iter()
                .map(|l| (l.label, l.href.as_str()))
                .collect();
        assert_eq!(
            links,
            [
                (
                    "Pipeline",
                    "/p/shop/pipelines/00000000-0000-0000-0000-0000000000c1"
                ),
                (
                    "Worker",
                    "/workers/00000000-0000-0000-0000-0000000000a1?project=shop"
                ),
            ]
        );
        let device = p.device.expect("a device was reported");
        assert_eq!(device.serial, "emulator-5554");
        assert_eq!(
            device.profile.as_deref(),
            Some("Pixel_7_API_34 · Android 14 (API 34) · arm64-v8a")
        );
        assert_eq!(
            device.href,
            "/devices/00000000-0000-0000-0000-0000000000d1?project=shop"
        );
    }

    #[test]
    fn a_flow_with_no_known_device_has_none() {
        let mut v = view();
        v.device = None;
        assert_eq!(FlowPage::build(&v, &ctx()).device, None);
    }

    #[test]
    fn earlier_executions_link_to_their_own_pages_with_their_provenance() {
        let prev = page().previous;
        assert_eq!(prev.len(), 1);
        assert_eq!(prev[0].state.label, "Passed");
        assert_eq!(prev[0].provenance, Provenance::Final);
        assert_eq!(prev[0].started, "2025-12-31 10:10:00");
        assert_eq!(prev[0].duration.as_deref(), Some("9 s"));
        assert_eq!(
            prev[0].href,
            "/flows/00000000-0000-0000-0000-0000000000f0?project=shop"
        );
    }

    #[test]
    fn evidence_knows_the_steps_and_which_one_failed() {
        let (steps, failed) = evidence_steps(&view());
        let shown: Vec<_> = steps
            .iter()
            .map(|s| (s.id.as_str(), s.label.as_deref()))
            .collect();
        assert_eq!(
            shown,
            [
                ("0", Some("Launch app")),
                ("1", Some("Tap on \"Pay\"")),
                ("2", Some("Assert visible \"Done\"")),
            ]
        );
        assert_eq!(
            failed,
            Some(StepKey {
                id: "2".to_owned(),
                label: Some("Assert visible \"Done\"".to_owned())
            })
        );
        let mut v = view();
        v.failure = None;
        v.steps[0].label = None;
        let (steps, failed) = evidence_steps(&v);
        assert_eq!(failed, None, "no failure, no failed step");
        assert_eq!(
            steps[0].label, None,
            "a step with no description stays without one: \"Step 0\" is not in any log"
        );
    }
}
