// SPDX-License-Identifier: AGPL-3.0-only
//! The flow page's view-model: what `GET …/flows/{execution_id}` says, laid out for debugging.
//!
//! Every claim about the outcome is listed with its source (invariant I4), the winner marked and
//! disagreements highlighted. Steps are numbered as the flow file numbers its commands, with
//! Maestro's own setup commands folded into one Setup row. Evidence (screenshots, logs, reports)
//! is [`super::evidence`].

use std::collections::HashMap;

use fleet_protocol::{
    event::StepStatus,
    frame::RunOrigin,
    live::AgentRunView,
    rest::{
        ClaimView, CrashView, FlowDetailView, InfrastructureView, StepView,
        history::FailureTriageView,
    },
};
use fleet_ui::{
    charts::format::format_duration,
    components::live::LiveState,
    mark::Mark,
    provenance::Provenance,
    steps::tree::{StepExec, StepTree, format_duration as step_duration, row_provenance},
};

use crate::{
    clock::unix_ms,
    live::socket::Connection,
    paths::{history_flow_href, pipeline_href, worker_href},
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
    /// Whether the run that produced the flow was published after the fact.
    pub origin: RunOrigin,
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

/// A step, by its number in the flow file and its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRef {
    /// The number the step is shown with (`6`, or `2.1` inside a block); `None` for Maestro's
    /// setup commands, which the flow file does not number.
    pub number: Option<String>,
    /// Its description.
    pub label: String,
    /// How long it ran, when it ended.
    pub duration: Option<String>,
}

/// Why the flow failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureDetails {
    /// The failure message, redacted; `None` when none was reported.
    pub message: Option<String>,
    /// The first failed step; `None` when the failure named none the steps know.
    pub failed_step: Option<StepRef>,
    /// The last step that completed before it.
    pub last_successful_step: Option<StepRef>,
    /// How many steps the flow file has, as far as they were observed (the `M` in "Step 6 of 6").
    pub total_steps: usize,
    /// Set when the device was lost during the flow, so the failure is the device's (issue #145).
    pub infrastructure: Option<InfrastructureView>,
    /// The failure's fingerprint, when the failure is final (issue #84).
    pub fingerprint: Option<String>,
    /// What was recorded about that fingerprint.
    pub triage: Option<FailureTriageView>,
}

/// Whether the flow failed, and the details when it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// It has not failed.
    NotFailed,
    /// It failed; what is known.
    Failed(Box<FailureDetails>),
}

/// The step tree and what to say about it.
#[derive(Debug, Clone)]
pub struct StepsSection {
    /// The steps, the setup commands folded into one row first.
    pub tree: StepTree,
    /// How many, and whether the total is known.
    pub summary: String,
    /// A caveat about how the steps are listed, when there are steps.
    pub note: Option<&'static str>,
    /// How the steps are known: observed once the flow has ended, provisional while it runs.
    pub provenance: Provenance,
    /// Set when the steps could not all be lined up with the flow's `commands.json`, or the file
    /// disagrees with the log on a step (issue #148). One line each.
    pub warnings: Vec<&'static str>,
}

/// The step a running flow is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentStep {
    /// Its number, as the step list shows it.
    pub number: Option<String>,
    /// Its description.
    pub label: String,
    /// How long it has run so far.
    pub elapsed: String,
}

/// The History line under the header: this flow's recent executions, and where its history is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryLine {
    /// The state of each of the most recent executions, oldest first, this one last.
    pub cells: Vec<Mark>,
    /// The strip, for a screen reader: how many of the cells passed, failed and are still open.
    pub strip_label: String,
    /// What the strip says, in words.
    pub summary: String,
    /// The History page, filtered to this flow with its stats open.
    pub href: String,
}

/// The CI run, job and runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiSection {
    /// What it is.
    pub facts: Vec<Fact>,
    /// Pages for the pipeline and the worker.
    pub links: Vec<Link>,
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
    /// The app crash recorded during the flow, if any: evidence beside the failure, not a verdict.
    pub crash: Option<CrashView>,
    /// The steps.
    pub steps: StepsSection,
    /// The step a running flow is on; `None` once it has ended or before its first step.
    pub current: Option<CurrentStep>,
    /// This flow's recent executions.
    pub history: HistoryLine,
    /// Whether the outcome is settled: the flow has ended and its final report has arrived.
    pub settled: bool,
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

/// Maestro's own commands that run before the flow file's first command. They are not numbered:
/// all of them show as one Setup row.
const SETUP_COMMANDS: [&str; 2] = ["Define variables", "Apply configuration"];

const SETUP_ROW: &str = "Setup · variables and configuration";

/// How many executions the History strip shows.
const HISTORY_CELLS: usize = 8;

fn is_setup(step: &StepView) -> bool {
    step.parent_index.is_none()
        && step
            .label
            .as_deref()
            .is_some_and(|l| SETUP_COMMANDS.contains(&l))
}

/// How the steps are numbered: as the flow file numbers its commands, from 1, without the setup
/// commands. A step inside a block is its block's number and its place in the block (`2.1`).
struct Numbering {
    /// The number of each numbered step, by index.
    numbers: HashMap<i32, String>,
    /// The indexes of the setup commands, in order.
    setup: Vec<i32>,
    /// How many top-level steps there are.
    top_level: usize,
}

impl Numbering {
    /// `steps` in the order Maestro ran them (a block before the steps it ran).
    fn of(steps: &[&StepView]) -> Self {
        let mut numbering = Self {
            numbers: HashMap::new(),
            setup: Vec::new(),
            top_level: 0,
        };
        let mut placed: HashMap<i32, usize> = HashMap::new();
        for step in steps {
            if is_setup(step) {
                numbering.setup.push(step.index);
                continue;
            }
            let block = step
                .parent_index
                .and_then(|p| numbering.numbers.get(&p).cloned().map(|number| (p, number)));
            let number = if let Some((p, block_number)) = block {
                let place = placed.entry(p).or_insert(0);
                *place += 1;
                format!("{block_number}.{place}")
            } else {
                numbering.top_level += 1;
                numbering.top_level.to_string()
            };
            numbering.numbers.insert(step.index, number);
        }
        numbering
    }

    fn number(&self, index: i32) -> Option<String> {
        self.numbers.get(&index).cloned()
    }
}

/// The steps and the failed step as [`evidence`](super::evidence) needs them: by id, with
/// Maestro's description only when the server has one.
#[must_use]
pub fn evidence_steps(view: &FlowDetailView) -> (Vec<StepKey>, Option<StepKey>) {
    let steps: Vec<StepKey> = StepView::flatten(&view.steps)
        .into_iter()
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

/// Whether the flow's outcome is final: it has ended and its final report confirmed it.
#[must_use]
pub fn is_settled(view: &FlowDetailView) -> bool {
    !matches!(view.flow.state.as_str(), "running" | "pending") && view.flow.finality == "verified"
}

/// What the top bar's live indicator says on the flow page: Settled once the outcome is final,
/// otherwise the socket's own state.
#[must_use]
pub fn live_state(connection: Connection, settled: bool) -> LiveState {
    if settled {
        return LiveState::Settled;
    }
    match connection {
        Connection::Connecting => LiveState::Connecting,
        Connection::Live => LiveState::Live,
        Connection::Reconnecting => LiveState::Reconnecting,
    }
}

impl FlowPage {
    /// Lays out `view`.
    #[must_use]
    pub fn build(view: &FlowDetailView, ctx: &Ctx) -> Self {
        let flat = StepView::flatten(&view.steps);
        let numbering = Numbering::of(&flat);
        let settled = is_settled(view);
        Self {
            header: header(view, ctx),
            ci: ci(view, ctx),
            device: view
                .device
                .as_ref()
                .map(|d| DeviceSessionRow::build(d, ctx)),
            claims: claims(&view.claims, ctx),
            failure: failure(view, &flat, &numbering),
            crash: view.crash.clone(),
            steps: steps(view, &flat, &numbering, settled),
            current: current(view, &flat, &numbering, ctx),
            history: history(view, ctx),
            settled,
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
        provenance: flow_provenance(
            &f.finality,
            &f.state,
            f.outcome_source.as_deref(),
            view.agent_run.liveness,
        ),
        finality_note: finality_note(&f.finality),
        origin: f.origin,
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
            Fact::maybe("Maestro version", maestro_version(&view.agent_run)),
        ],
        links,
    }
}

/// The run's Maestro version, flagged when Fleet's parsers were not built for it.
fn maestro_version(run: &AgentRunView) -> Option<String> {
    let version = run.maestro_version.as_ref()?;
    Some(match run.maestro_profile_supported {
        Some(false) => format!("{version} (not in Fleet's supported profile)"),
        _ => version.clone(),
    })
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

/// The step with this index, as the flow file numbers it. `None` when no step has the index.
fn step_ref(flat: &[&StepView], numbering: &Numbering, index: i32) -> Option<StepRef> {
    let step = flat.iter().find(|s| s.index == index)?;
    Some(StepRef {
        number: numbering.number(index),
        label: step_label(step),
        // The step list's own format, so a step reads the same wherever it is named.
        duration: step
            .duration_ms
            .and_then(|d| u64::try_from(d).ok())
            .map(step_duration),
    })
}

fn failure(view: &FlowDetailView, flat: &[&StepView], numbering: &Numbering) -> Failure {
    if view.flow.state != "failed" && view.failure.is_none() {
        return Failure::NotFailed;
    }
    let f = view.failure.as_ref();
    Failure::Failed(Box::new(FailureDetails {
        message: f
            .and_then(|f| f.message.clone())
            .or_else(|| view.flow.failure_message.clone()),
        failed_step: f
            .and_then(|f| f.failed_step_index)
            .and_then(|i| step_ref(flat, numbering, i)),
        last_successful_step: f
            .and_then(|f| f.last_successful_step_index)
            .and_then(|i| step_ref(flat, numbering, i)),
        total_steps: numbering.top_level,
        infrastructure: view.infrastructure.clone(),
        fingerprint: f.and_then(|f| f.fingerprint.clone()),
        triage: f.and_then(|f| f.triage.clone()),
    }))
}

/// The setup commands as one exec, first in the list: the worst of their statuses, and their
/// durations summed when all of them are known.
fn setup_exec(setup: &[&StepView], quiet: bool) -> Option<StepExec> {
    let first = setup.first()?;
    let statuses: Vec<StepStatus> = setup.iter().map(|s| step_status(&s.status)).collect();
    let status = [
        StepStatus::Failed,
        StepStatus::Running,
        StepStatus::Warned,
        StepStatus::Unknown,
        StepStatus::Skipped,
    ]
    .into_iter()
    .find(|s| statuses.contains(s))
    .unwrap_or(StepStatus::Completed);
    let durations: Option<Vec<u64>> = setup
        .iter()
        .map(|s| s.duration_ms.and_then(|d| u64::try_from(d).ok()))
        .collect();
    Some(StepExec {
        // The first setup command's index: the Setup row jumps to where the log's setup begins.
        id: first.index.to_string(),
        parent: None,
        label: SETUP_ROW.to_owned(),
        number: None,
        status,
        source: parse_source(&first.source),
        duration_ms: durations.map(|d| d.iter().sum()),
        attempt: 1,
        quiet,
    })
}

fn steps(
    view: &FlowDetailView,
    flat: &[&StepView],
    numbering: &Numbering,
    settled: bool,
) -> StepsSection {
    let setup: Vec<&StepView> = flat.iter().copied().filter(|s| is_setup(s)).collect();
    // A flow still running whose agent has gone quiet: a value read from the live log is stale.
    let quiet = view.flow.state == "running"
        && matches!(
            view.agent_run.liveness,
            Some(
                fleet_protocol::live::Liveness::Stale
                    | fleet_protocol::live::Liveness::Disconnected
            )
        );
    let mut execs: Vec<StepExec> = setup_exec(&setup, quiet).into_iter().collect();
    execs.extend(flat.iter().filter(|s| !is_setup(s)).map(|s| StepExec {
        id: s.index.to_string(),
        parent: s.parent_index.map(|p| p.to_string()),
        label: step_label(s),
        number: numbering.number(s.index),
        status: step_status(&s.status),
        source: parse_source(&s.source),
        duration_ms: s.duration_ms.and_then(|d| u64::try_from(d).ok()),
        // Unknown attempts count as the first, which is what a step outside a retry is.
        attempt: s.attempt.and_then(|a| u32::try_from(a).ok()).unwrap_or(1),
        quiet,
    }));
    let tree = StepTree::build(&execs);
    // The section says how its steps are known by its weakest row, so one step still read from the
    // log keeps the whole section provisional (issue #148). With no steps it says how the flow ended.
    let weakest = execs
        .iter()
        .map(|e| row_provenance(e.source, e.status, e.quiet))
        .min_by_key(|p| provenance_rank(*p));
    let n = flat.len() - setup.len();
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
            "Listed in the order Maestro ran them. A block (runFlow, retry, repeat) holds the steps it ran; a retried step shows its attempt.",
        ),
        provenance: weakest.unwrap_or(if settled {
            Provenance::Observed
        } else {
            Provenance::Provisional
        }),
        warnings: step_warnings(&view.step_warnings),
    }
}

/// What to say about the step alignment warnings a flow's final file raised (issue #148).
fn step_warnings(codes: &[String]) -> Vec<&'static str> {
    let has = |code: &str| codes.iter().any(|c| c == code);
    let mut out = Vec::new();
    if has("steps_not_aligned") {
        out.push(
            "Some steps could not be matched to the flow's commands.json, so they stay provisional.",
        );
    }
    if has("step_status_disagrees") {
        out.push(
            "The log and commands.json disagree on a step's status; the file's status is shown.",
        );
    }
    out
}

/// How far a provenance can be relied on, lowest first: a section takes its weakest row's.
fn provenance_rank(p: Provenance) -> u8 {
    match p {
        Provenance::Missing => 0,
        Provenance::Stale => 1,
        Provenance::Provisional => 2,
        Provenance::Inferred => 3,
        Provenance::Observed => 4,
        Provenance::Derived => 5,
        Provenance::Final => 6,
    }
}

/// The last step that is still running, while the flow is open.
fn current(
    view: &FlowDetailView,
    flat: &[&StepView],
    numbering: &Numbering,
    ctx: &Ctx,
) -> Option<CurrentStep> {
    if view.flow.state != "running" {
        return None;
    }
    let step = flat
        .iter()
        .rev()
        .find(|s| !is_setup(s) && step_status(&s.status) == StepStatus::Running)?;
    let elapsed = step
        .started_at
        .map(|t| ctx.now_ms.saturating_sub(unix_ms(t)))?;
    Some(CurrentStep {
        number: numbering.number(step.index),
        label: step_label(step),
        elapsed: format_duration(elapsed),
    })
}

/// The History line: the most recent executions of this flow, this one last.
fn history(view: &FlowDetailView, ctx: &Ctx) -> HistoryLine {
    let mut all: Vec<Mark> = view
        .previous
        .iter()
        .rev()
        .map(|p| flow_state_pill(&p.state).mark)
        .collect();
    all.push(flow_state_pill(&view.flow.state).mark);
    let start = all.len().saturating_sub(HISTORY_CELLS);
    let cells: Vec<Mark> = all.into_iter().skip(start).collect();
    let count = |mark: Mark| cells.iter().filter(|m| **m == mark).count();
    let (passed, failed) = (count(Mark::Passed), count(Mark::Failed));
    let summary = if view.previous.is_empty() {
        "First execution of this flow".to_owned()
    } else {
        format!(
            "Failed in {failed} of the last {}",
            plural(cells.len(), "execution", "executions")
        )
    };
    HistoryLine {
        strip_label: format!(
            "Last {}: {passed} passed, {failed} failed, {} other",
            plural(cells.len(), "execution", "executions"),
            cells.len() - passed - failed
        ),
        summary,
        href: history_flow_href(
            &ctx.project,
            view.flow.path.as_deref().unwrap_or(&view.flow.name),
        ),
        cells,
    }
}

#[cfg(test)]
mod tests {
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
                    "maestro_exit_code": null, "maestro_version": "2.12.1", "rev": 3,
                    "liveness": "online"},
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
      "crash": {"package": "com.example.shop",
                "exception": "java.lang.IllegalStateException: cart is empty"},
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
        assert_eq!((h.state.label, h.state.mark), ("Failed", Mark::Failed));
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

    /// The steps section says what its file could not line up, one line per code (issue #148).
    #[test]
    fn the_steps_section_names_each_alignment_warning_its_file_raised() {
        assert_eq!(page().steps.warnings, Vec::<&str>::new());
        let mut v = view();
        v.step_warnings = vec!["step_status_disagrees".into()];
        assert_eq!(
            FlowPage::build(&v, &ctx()).steps.warnings,
            ["The log and commands.json disagree on a step's status; the file's status is shown."]
        );
        v.step_warnings = vec!["steps_not_aligned".into(), "step_status_disagrees".into()];
        let warnings = FlowPage::build(&v, &ctx()).steps.warnings;
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("could not be matched to the flow's commands.json"));
        assert!(warnings[1].contains("disagree"));
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
                number: Some("3".to_owned()),
                label: "Assert visible \"Done\"".to_owned(),
                duration: Some("7.0 s".to_owned()),
            })
        );
        assert_eq!(
            f.last_successful_step,
            Some(StepRef {
                number: Some("2".to_owned()),
                label: "Tap on \"Pay\"".to_owned(),
                duration: Some("3.0 s".to_owned()),
            })
        );
        assert_eq!(f.total_steps, 3);
    }

    /// A step as the server reports it, with the fields a step without nesting leaves out.
    fn step(index: i32, label: &str, status: &str, duration_ms: i64) -> StepView {
        serde_json::from_value(serde_json::json!({
            "index": index, "label": label, "status": status, "source": "debug_log",
            "duration_ms": duration_ms,
        }))
        .unwrap()
    }

    /// Issue: "step 7" for the eighth row. The flow file has six commands; Maestro ran two setup
    /// commands before them, so the failing `assertVisible` is the eighth row, index 7. It is the
    /// sixth command the user wrote, and that is what the page must say.
    #[test]
    fn the_failed_step_is_numbered_as_in_the_flow_file_not_by_its_index() {
        let mut v = view();
        v.steps = vec![
            step(0, "Define variables", "completed", 4),
            step(1, "Apply configuration", "completed", 1),
            step(2, "Launch app", "completed", 2000),
            step(3, "Tap on \"Pay\"", "completed", 3000),
            step(4, "Tap on \"Sport\"", "completed", 1700),
            step(5, "Tap on \"Browse\"", "completed", 1400),
            step(6, "Extended wait until \"Hot deals\"", "completed", 0),
            step(7, "Assert that \"Done\" is visible", "failed", 17_200),
        ];
        v.failure = serde_json::from_value(serde_json::json!({
            "message": "Element not found: Done",
            "failed_step_index": 7,
            "last_successful_step_index": 6,
        }))
        .unwrap();
        let page = FlowPage::build(&v, &ctx());
        let Failure::Failed(f) = page.failure else {
            panic!("the flow failed");
        };
        let failed = f.failed_step.expect("the failed step is known");
        assert_eq!(
            failed.number.as_deref(),
            Some("6"),
            "the sixth command of the flow file"
        );
        assert_eq!(f.total_steps, 6);
        assert_eq!(failed.duration.as_deref(), Some("17.2 s"));

        // The setup commands are one row, unnumbered, and the user's six follow as 1 to 6.
        let state = fleet_ui::steps::tree::TreeState::open_to_attention(&page.steps.tree);
        let rows = state.rows(&page.steps.tree);
        let shown: Vec<(String, String)> = rows
            .iter()
            .map(|r| (r.number.clone().unwrap_or_default(), r.label.clone()))
            .collect();
        assert_eq!(
            shown.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["", "1", "2", "3", "4", "5", "6"]
        );
        assert_eq!(shown[0].1, "Setup · variables and configuration");
        assert_eq!(shown[6].1, "Assert that \"Done\" is visible");
    }

    #[test]
    fn a_failure_at_the_setup_commands_says_setup_not_a_number() {
        let mut v = view();
        v.steps = vec![
            step(0, "Define variables", "failed", 4),
            step(1, "Apply configuration", "skipped", 0),
        ];
        v.failure = serde_json::from_value(serde_json::json!({
            "message": "bad variable", "failed_step_index": 0,
        }))
        .unwrap();
        let Failure::Failed(f) = FlowPage::build(&v, &ctx()).failure else {
            panic!("the flow failed");
        };
        assert_eq!(f.failed_step.map(|s| s.number), Some(None));
        assert_eq!(f.total_steps, 0);
    }

    #[test]
    fn a_crash_is_carried_beside_the_failure_and_is_absent_when_none_was_recorded() {
        let crash = page().crash.expect("the fixture records a crash");
        assert_eq!(crash.package, "com.example.shop");
        assert_eq!(
            crash.exception,
            "java.lang.IllegalStateException: cart is empty"
        );
        let Failure::Failed(f) = page().failure else {
            panic!("the flow failed");
        };
        assert_eq!(
            f.message.as_deref(),
            Some("Element not found: Done"),
            "the verdict stays Maestro's failure"
        );
        let mut v = view();
        v.crash = None;
        assert_eq!(FlowPage::build(&v, &ctx()).crash, None);
    }

    #[test]
    fn a_lost_device_is_carried_with_the_failure_and_the_failure_stays_failed() {
        let mut v = view();
        v.infrastructure = Some(InfrastructureView {
            evidence: "maestro.android.DeviceServerDiedException".to_owned(),
            warning: "run/debug/slow/commands.json is incomplete: the flow was cut short"
                .to_owned(),
            source: Some("junit_final".to_owned()),
            finality: "verified".to_owned(),
        });
        let Failure::Failed(f) = FlowPage::build(&v, &ctx()).failure else {
            panic!("the flow failed");
        };
        assert_eq!(
            f.infrastructure.map(|i| i.evidence).as_deref(),
            Some("maestro.android.DeviceServerDiedException")
        );
        assert_eq!(
            f.message.as_deref(),
            Some("Element not found: Done"),
            "the verdict stays Maestro's failure"
        );
        let Failure::Failed(plain) = page().failure else {
            panic!("the flow failed");
        };
        assert_eq!(plain.infrastructure, None, "an assertion is the app's");
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

    /// A `runFlow` (0) that runs a tap (1) and a `retry` block (2); the block's assertion failed
    /// once, then passed (issue #38). Maestro's log gave the depths, parents and attempts.
    #[test]
    fn nested_steps_sit_under_their_block_and_a_retried_step_shows_its_attempt() {
        let mut v = view();
        v.steps = serde_json::from_str(
            r#"[
              {"index": 0, "label": "Run subflows/checkout.yaml", "status": "failed",
               "source": "debug_log", "started_at": null, "ended_at": null, "duration_ms": null,
               "children": [
                 {"index": 1, "label": "Tap on \"Pay\"", "status": "completed",
                  "source": "debug_log", "started_at": null, "ended_at": null, "duration_ms": null,
                  "parent_index": 0, "depth": 1},
                 {"index": 2, "label": "Retry 2 times", "status": "failed",
                  "source": "debug_log", "started_at": null, "ended_at": null, "duration_ms": null,
                  "parent_index": 0, "depth": 1, "children": [
                    {"index": 3, "label": "Assert visible \"Done\"", "status": "failed",
                     "source": "debug_log", "started_at": null, "ended_at": null,
                     "duration_ms": null, "parent_index": 2, "depth": 2, "attempt": 1},
                    {"index": 4, "label": "Assert visible \"Done\"", "status": "failed",
                     "source": "debug_log", "started_at": null, "ended_at": null,
                     "duration_ms": null, "parent_index": 2, "depth": 2, "attempt": 2}
                  ]}
               ]}
            ]"#,
        )
        .unwrap();
        let steps = FlowPage::build(&v, &ctx()).steps;
        assert_eq!(steps.summary, "5 steps");
        let tree = steps.tree;
        let rows = fleet_ui::steps::tree::TreeState::open_to_attention(&tree).rows(&tree);
        let shape: Vec<_> = rows
            .iter()
            .map(|r| (r.id.as_str(), r.level, r.attempt.as_deref()))
            .collect();
        assert_eq!(
            shape,
            [
                ("0", 1, None),
                ("1", 2, None),
                ("2", 2, None),
                ("3", 3, Some("attempt 1 of 2")),
                ("4", 3, Some("attempt 2 of 2")),
            ]
        );
    }

    #[test]
    fn steps_are_a_flat_tree_in_order_with_status_source_and_duration() {
        let steps = page().steps;
        assert_eq!(steps.summary, "3 steps");
        assert_eq!(
            steps.note,
            Some(
                "Listed in the order Maestro ran them. A block (runFlow, retry, repeat) holds the steps it ran; a retried step shows its attempt."
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
                ("Maestro version", "2.12.1"),
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
    fn a_maestro_version_outside_the_supported_profile_is_flagged() {
        let mut v = view();
        v.agent_run.maestro_version = Some("2.13.0".into());
        v.agent_run.maestro_profile_supported = Some(false);
        let p = FlowPage::build(&v, &ctx());
        let version = p.ci.facts.iter().find(|f| f.label == "Maestro version");
        assert_eq!(
            version.map(Fact::shown),
            Some("2.13.0 (not in Fleet's supported profile)")
        );
    }

    #[test]
    fn a_flow_with_no_known_device_has_none() {
        let mut v = view();
        v.device = None;
        assert_eq!(FlowPage::build(&v, &ctx()).device, None);
    }

    #[test]
    fn the_history_line_strips_this_flows_recent_executions_and_opens_its_history() {
        let h = page().history;
        assert_eq!(h.cells, [Mark::Passed, Mark::Failed]);
        assert_eq!(h.summary, "Failed in 1 of the last 2 executions");
        assert_eq!(
            h.strip_label,
            "Last 2 executions: 1 passed, 1 failed, 0 other"
        );
        assert_eq!(
            h.href,
            "/history?project=shop&flow=flows%2Fcheckout.yaml&stats=flows%2Fcheckout.yaml"
        );
    }

    #[test]
    fn a_flow_with_no_earlier_execution_says_it_is_the_first() {
        let mut v = view();
        v.previous.clear();
        let h = FlowPage::build(&v, &ctx()).history;
        assert_eq!(h.cells, [Mark::Failed]);
        assert_eq!(h.summary, "First execution of this flow");
    }

    #[test]
    fn a_running_flow_names_the_step_it_is_on() {
        let mut v = view();
        v.flow.state = "running".to_owned();
        v.failure = None;
        v.steps[2] = serde_json::from_value(serde_json::json!({
            "index": 2, "label": "Assert visible \"Done\"", "status": "running",
            "source": "debug_log", "started_at": "2026-01-01T10:10:05Z",
        }))
        .unwrap();
        let current = FlowPage::build(&v, &ctx())
            .current
            .expect("a step is running");
        assert_eq!(current.number.as_deref(), Some("3"));
        assert_eq!(current.label, "Assert visible \"Done\"");
        assert_ne!(current.elapsed, "");
        assert_eq!(
            page().current,
            None,
            "a flow that has ended has no current step"
        );
    }

    /// The steps section shows its weakest row's provenance: a step still read from the log keeps the
    /// section provisional, and steps a final file settled are verified (issue #148).
    #[test]
    fn the_steps_section_shows_the_weakest_rows_provenance() {
        let mut v = view();
        for step in &mut v.steps {
            step.source = "artifact_file".to_owned();
        }
        assert_eq!(
            FlowPage::build(&v, &ctx()).steps.provenance,
            Provenance::Final
        );
        v.steps[2].source = "debug_log".to_owned();
        assert_eq!(
            FlowPage::build(&v, &ctx()).steps.provenance,
            Provenance::Provisional
        );
    }

    #[test]
    fn the_live_indicator_is_settled_only_once_the_outcome_is_final() {
        assert!(page().settled);
        // The fixture's steps were all read from the log, so the section is provisional.
        assert_eq!(page().steps.provenance, Provenance::Provisional);
        let mut v = view();
        v.flow.finality = "provisional".to_owned();
        let open = FlowPage::build(&v, &ctx());
        assert!(!open.settled);
        assert_eq!(open.steps.provenance, Provenance::Provisional);
        assert_eq!(live_state(Connection::Live, true), LiveState::Settled);
        assert_eq!(live_state(Connection::Live, false), LiveState::Live);
        assert_eq!(
            live_state(Connection::Reconnecting, false),
            LiveState::Reconnecting
        );
        assert_eq!(
            live_state(Connection::Connecting, false),
            LiveState::Connecting
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
