// SPDX-License-Identifier: AGPL-3.0-only
//! Screen F's view-model: the attempt pickers, the comparison grouped into sections, and the
//! wording of every explanation.
//!
//! The server's explanations are facts drawn from the two sides' evidence; this module only puts
//! them in words. It adds no reason of its own: a row the server did not explain says nothing
//! about why it differs, and a duration change is never called a regression, only shown, with a
//! note when the conditions differ (spec §8: "do not treat differences … as direct performance
//! regressions without explaining the mismatch").

use fleet_protocol::{
    live::AttemptView,
    rest::history::{
        ChangeKind, CompareFlowView, CompareRowView, CompareSideView, CompareView, DimChangeView,
        EvidenceView, Explanation, Side,
    },
};
use fleet_ui::{charts::format::format_duration, provenance::Provenance};
use uuid::Uuid;

use crate::{
    clock::unix_ms,
    format::{ref_name, short_commit},
    paths::{pipeline_href, query_param},
    screens::common::{Ctx, finality_note, parse_source, source_label},
    status::{Pill, flow_provenance, flow_state_pill},
};

/// The URL parameter naming attempt A.
pub const PARAM_A: &str = "a";
/// The URL parameter naming attempt B.
pub const PARAM_B: &str = "b";

/// The two attempts the URL names; a side is `None` until it names a valid attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompareQuery {
    /// The first attempt.
    pub a: Option<Uuid>,
    /// The second attempt.
    pub b: Option<Uuid>,
}

impl CompareQuery {
    /// Reads `a` and `b` from the page's URL query; anything that is not a UUID is no attempt.
    #[must_use]
    pub fn parse(query: &str) -> Self {
        let id = |key| query_param(query, key).and_then(|v| v.parse().ok());
        Self {
            a: id(PARAM_A),
            b: id(PARAM_B),
        }
    }

    /// Both attempts, when both are named.
    #[must_use]
    pub fn both(&self) -> Option<(Uuid, Uuid)> {
        self.a.zip(self.b)
    }
}

/// One entry of an attempt picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptOption {
    /// The attempt.
    pub id: Uuid,
    /// `acme/app:42 · attempt 1 · main · 0123456 · 2026-01-01 10:00:00`.
    pub label: String,
}

/// The pickers' entries, in the order the server lists the attempts (newest first).
#[must_use]
pub fn attempt_options(attempts: &[AttemptView], ctx: &Ctx) -> Vec<AttemptOption> {
    attempts
        .iter()
        .map(|a| AttemptOption {
            id: a.id,
            label: attempt_label(a, ctx),
        })
        .collect()
}

fn attempt_label(a: &AttemptView, ctx: &Ctx) -> String {
    let mut parts = vec![a.run_key.clone(), format!("attempt {}", a.attempt)];
    parts.extend(a.git_ref.as_deref().map(|r| ref_name(r).to_owned()));
    parts.extend(a.commit_sha.as_deref().map(short_commit));
    parts.push(ctx.time(unix_ms(a.first_seen_at)));
    parts.join(" · ")
}

/// One compared attempt at a glance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideSummary {
    /// `A` or `B`.
    pub name: &'static str,
    /// The attempt's label.
    pub label: String,
    /// The attempt's page.
    pub href: String,
    /// `3 flows: 2 passed, 1 failed`.
    pub flows: String,
}

fn side_summary(name: &'static str, side: &CompareSideView, ctx: &Ctx) -> SideSummary {
    let c = &side.flows;
    let total = c.pending + c.running + c.passed + c.failed + c.skipped + c.interrupted + c.unknown;
    let mut parts = Vec::new();
    for (n, word) in [
        (c.passed, "passed"),
        (c.failed, "failed"),
        (c.skipped, "skipped"),
        (c.interrupted, "interrupted"),
        (c.running, "running"),
        (c.pending, "pending"),
        (c.unknown, "unknown"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {word}"));
        }
    }
    let flows = if total == 0 {
        "no flows".to_owned()
    } else {
        format!(
            "{total} {}: {}",
            if total == 1 { "flow" } else { "flows" },
            parts.join(", ")
        )
    };
    SideSummary {
        name,
        label: attempt_label(&side.attempt, ctx),
        href: pipeline_href(&ctx.project, side.attempt.id),
        flows,
    }
}

/// A flow as one side ran it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideCell {
    /// The flow execution's page.
    pub href: String,
    /// Its state.
    pub state: Pill,
    /// What the state rests on (invariant I4).
    pub provenance: Provenance,
    /// How settled it is, in words.
    pub finality_note: &'static str,
    /// How long it took.
    pub duration: Option<String>,
    /// The failure fingerprint of a failed run.
    pub failure_fingerprint: Option<String>,
    /// More than one execution in the attempt: the latest is shown.
    pub executions_note: Option<String>,
    /// Whether the run failed.
    pub failed: bool,
}

fn cell(f: &CompareFlowView, ctx: &Ctx) -> SideCell {
    SideCell {
        href: crate::paths::flow_href(&ctx.project, f.flow_id),
        state: flow_state_pill(&f.state),
        failed: f.state == "failed",
        provenance: flow_provenance(&f.finality, &f.state, None),
        finality_note: finality_note(&f.finality),
        duration: f.duration_ms.map(format_duration),
        failure_fingerprint: f.failure_fingerprint.clone(),
        executions_note: (f.executions > 1)
            .then(|| format!("{} executions; the latest is shown", f.executions)),
    }
}

/// Why a row differs, in words, with the kind for browser tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplanationText {
    /// The server's kind: `flow_missing`, `condition_changed`, `failure_changed`,
    /// `provenance_differs`.
    pub kind: &'static str,
    /// The sentence.
    pub text: String,
}

/// How a duration moved from A to B, and why it may not be comparable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurationChange {
    /// `1.5 s → 3.0 s (+1.5 s)`.
    pub text: String,
    /// Set when the evidence says the two runs are not like for like.
    pub caveat: Option<&'static str>,
}

/// One flow, A against B.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The flow's identity.
    pub flow: String,
    /// What happened to it.
    pub change: ChangeKind,
    /// Its latest execution in A.
    pub a: Option<SideCell>,
    /// Its latest execution in B.
    pub b: Option<SideCell>,
    /// How its duration moved, when both sides have one.
    pub duration: Option<DurationChange>,
    /// Why the sides differ, from the evidence; empty when the evidence says nothing differs.
    pub explanations: Vec<ExplanationText>,
}

/// Rows that changed in the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The change.
    pub change: ChangeKind,
    /// A stable hook for browser tests.
    pub id: &'static str,
    /// The heading.
    pub heading: &'static str,
    /// What the heading means.
    pub meaning: &'static str,
    /// Whether the section starts open: the unchanged rows don't.
    pub open: bool,
    /// Whether every result in the section is verified: then the heading says so once.
    pub verified: bool,
    /// The rows, by flow.
    pub rows: Vec<Row>,
}

/// One headline count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryItem {
    /// A stable hook for browser tests.
    pub id: &'static str,
    /// What is counted.
    pub label: &'static str,
    /// How many.
    pub count: usize,
}

/// Screen F.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareModel {
    /// Attempt A.
    pub a: SideSummary,
    /// Attempt B.
    pub b: SideSummary,
    /// Flows that failed in B and not in A: the answer to "what did this run break?".
    pub failures_only_in_b: usize,
    /// The counts that matter: newly failing always, the others only when not zero.
    pub summary: Vec<SummaryItem>,
    /// How many flows ran under different conditions in the two attempts.
    pub conditions_changed: usize,
    /// Whether either attempt is still open, so its results may still change.
    pub running: bool,
    /// The non-empty sections, the changes first.
    pub sections: Vec<Section>,
    /// Set when the server cut a side short.
    pub truncated_note: Option<&'static str>,
}

/// The sections in display order: what needs attention first.
const SECTIONS: [(ChangeKind, &str, &str, &str, bool); 6] = [
    (
        ChangeKind::NewlyFailing,
        "newly-failing",
        "Newly failing",
        "Passed in A, failed in B.",
        true,
    ),
    (
        ChangeKind::Recovered,
        "recovered",
        "Recovered",
        "Failed in A, passed in B.",
        true,
    ),
    (
        ChangeKind::Changed,
        "changed",
        "Changed",
        "Ended in a different state, other than passed to failed or back.",
        true,
    ),
    (
        ChangeKind::OnlyInA,
        "only-in-a",
        "Only in A",
        "Executed in A and never in B.",
        true,
    ),
    (
        ChangeKind::OnlyInB,
        "only-in-b",
        "Only in B",
        "Executed in B and never in A.",
        true,
    ),
    (
        ChangeKind::Unchanged,
        "unchanged",
        "Unchanged",
        "The same state on both sides.",
        false,
    ),
];

impl CompareModel {
    /// The screen for `view`.
    #[must_use]
    pub fn build(view: &CompareView, ctx: &Ctx) -> Self {
        let rows: Vec<Row> = view.rows.iter().map(|r| row(r, ctx)).collect();
        let summary = SECTIONS
            .iter()
            .map(|(change, id, label, _, _)| SummaryItem {
                id,
                label,
                count: rows.iter().filter(|r| r.change == *change).count(),
            })
            .filter(|item| item.id == "newly-failing" || item.count > 0)
            .collect();
        let sections = SECTIONS
            .iter()
            .filter_map(|(change, id, heading, meaning, open)| {
                let rows: Vec<Row> = rows
                    .iter()
                    .filter(|r| r.change == *change)
                    .cloned()
                    .collect();
                let verified = rows.iter().all(|r| {
                    r.a.iter()
                        .chain(r.b.iter())
                        .all(|c| c.provenance == Provenance::Final)
                });
                (!rows.is_empty()).then_some(Section {
                    change: *change,
                    id,
                    heading,
                    meaning,
                    open: *open,
                    verified,
                    rows,
                })
            })
            .collect();
        Self {
            a: side_summary("A", &view.a, ctx),
            b: side_summary("B", &view.b, ctx),
            failures_only_in_b: rows
                .iter()
                .filter(|r| {
                    r.b.as_ref().is_some_and(|b| b.failed)
                        && !r.a.as_ref().is_some_and(|a| a.failed)
                })
                .count(),
            conditions_changed: rows
                .iter()
                .filter(|r| r.explanations.iter().any(|e| e.kind == "condition_changed"))
                .count(),
            running: view.a.attempt.open_jobs > 0 || view.b.attempt.open_jobs > 0,
            summary,
            sections,
            truncated_note: view
                .truncated
                .then_some("An attempt has more than 5,000 flows: the comparison is incomplete."),
        }
    }
}

fn row(r: &CompareRowView, ctx: &Ctx) -> Row {
    let has_condition_change = r
        .explanations
        .iter()
        .any(|e| matches!(e, Explanation::ConditionChanged { .. }));
    Row {
        flow: r.flow.clone(),
        change: r.change,
        a: r.a.as_ref().map(|f| cell(f, ctx)),
        b: r.b.as_ref().map(|f| cell(f, ctx)),
        duration: r.a.as_ref().zip(r.b.as_ref()).and_then(|(a, b)| {
            Some(duration_change(
                a.duration_ms?,
                b.duration_ms?,
                has_condition_change,
            ))
        }),
        explanations: r.explanations.iter().map(explain).collect(),
    }
}

fn duration_change(a: i64, b: i64, conditions_differ: bool) -> DurationChange {
    let delta = b.saturating_sub(a);
    let change = match delta {
        0 => "no change".to_owned(),
        d if d > 0 => format!("+{}", format_duration(d)),
        d => format!("-{}", format_duration(d.saturating_abs())),
    };
    DurationChange {
        text: format!("{} → {} ({change})", format_duration(a), format_duration(b)),
        caveat: conditions_differ
            .then_some("The conditions differ, so this is not a like-for-like comparison."),
    }
}

fn explain(e: &Explanation) -> ExplanationText {
    match e {
        Explanation::FlowMissing { side } => ExplanationText {
            kind: "flow_missing",
            text: format!("Not executed in {}.", side_name(*side)),
        },
        Explanation::ConditionChanged { changes, .. } => ExplanationText {
            kind: "condition_changed",
            text: if changes.is_empty() {
                "Ran under different conditions.".to_owned()
            } else {
                format!(
                    "Ran under different conditions: {}.",
                    changes
                        .iter()
                        .map(change_text)
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            },
        },
        Explanation::FailureChanged { a, b } => ExplanationText {
            kind: "failure_changed",
            text: format!(
                "Failed differently: the failure fingerprints are {a} in A and {b} in B."
            ),
        },
        Explanation::ProvenanceDiffers { a, b } => ExplanationText {
            kind: "provenance_differs",
            text: format!(
                "The outcomes rest on different evidence: A is {}; B is {}.",
                evidence_text(a),
                evidence_text(b)
            ),
        },
    }
}

const fn side_name(side: Side) -> &'static str {
    match side {
        Side::A => "A",
        Side::B => "B",
    }
}

fn change_text(c: &DimChangeView) -> String {
    let dim = match c.dim.as_str() {
        "arch" => "CPU architecture",
        "os" => "host OS",
        "device_model" => "device model",
        "device_api_level" => "Android API level",
        "host_memory" => "host memory",
        "host_cores" => "host cores",
        "maestro" => "Maestro version",
        "ci_provider" => "CI provider",
        other => other,
    };
    let value = |v: &str| {
        if v == "unknown" {
            "not reported".to_owned()
        } else {
            v.to_owned()
        }
    };
    format!("{dim} {} in A, {} in B", value(&c.a), value(&c.b))
}

fn evidence_text(e: &EvidenceView) -> String {
    let finality = match e.finality.as_str() {
        "verified" => "verified",
        "provisional" => "provisional",
        "unverified_final" => "final but unverified",
        "inferred" => "inferred",
        _ => "of unknown finality",
    };
    match &e.outcome_source {
        Some(source) => format!("{finality} ({})", source_label(parse_source(source))),
        None => format!("{finality}, with no evidence yet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "00000000-0000-0000-0000-0000000000a1";
    const B: &str = "00000000-0000-0000-0000-0000000000b2";
    const F1: &str = "00000000-0000-0000-0000-0000000000f1";
    const F2: &str = "00000000-0000-0000-0000-0000000000f2";
    const F3: &str = "00000000-0000-0000-0000-0000000000f3";

    fn ctx() -> Ctx {
        Ctx {
            project: "shop".to_owned(),
            now_ms: 1_767_265_200_000,
            utc_offset_minutes: 0,
        }
    }

    fn attempt(id: &str, run: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "project_id": "00000000-0000-0000-0000-000000000001",
            "provider": "github_actions", "run_key": run, "repository": "acme/app",
            "commit_sha": "0123456789abcdef", "git_ref": "refs/heads/main", "attempt": 1,
            "version": 3, "planned_total": null, "jobs": 1, "open_jobs": 0,
            "first_seen_at": "2026-01-01T10:00:00Z"
        })
    }

    fn flow(id: &str, state: &str, source: &str, finality: &str, ms: i64) -> serde_json::Value {
        serde_json::json!({
            "flow_id": id, "name": "checkout", "state": state, "outcome_source": source,
            "finality": finality, "duration_ms": ms, "executions": 1,
            "condition_fingerprint": null, "failure_fingerprint": null,
            "failure_message": null
        })
    }

    fn view() -> CompareView {
        serde_json::from_value(serde_json::json!({
            "a": {"attempt": attempt(A, "acme/app:41"),
                  "flows": {"pending": 0, "running": 0, "passed": 2, "failed": 0, "skipped": 0,
                            "interrupted": 0, "unknown": 0}},
            "b": {"attempt": attempt(B, "acme/app:42"),
                  "flows": {"pending": 0, "running": 0, "passed": 0, "failed": 1, "skipped": 0,
                            "interrupted": 0, "unknown": 0}},
            "truncated": false,
            "rows": [
                {"flow": "flows/checkout.yaml", "change": "newly_failing",
                 "a": flow(F1, "passed", "junit_final", "verified", 1500),
                 "b": flow(F2, "failed", "live_console", "provisional", 4000),
                 "explanations": [
                    {"kind": "condition_changed", "a": "aa", "b": "bb", "changes": [
                        {"dim": "arch", "a": "x86_64", "b": "aarch64"},
                        {"dim": "host_memory", "a": "16GiB", "b": "unknown"}]},
                    {"kind": "provenance_differs",
                     "a": {"outcome_source": "junit_final", "finality": "verified"},
                     "b": {"outcome_source": "live_console", "finality": "provisional"}}]},
                {"flow": "flows/legacy.yaml", "change": "only_in_a",
                 "a": flow(F3, "passed", "junit_final", "verified", 900), "b": null,
                 "explanations": [{"kind": "flow_missing", "side": "b"}]},
                {"flow": "flows/login.yaml", "change": "unchanged",
                 "a": flow(F1, "passed", "junit_final", "verified", 1000),
                 "b": flow(F2, "passed", "junit_final", "verified", 1000),
                 "explanations": []}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn the_url_names_two_attempts_and_ignores_what_is_not_an_id() {
        let q = CompareQuery::parse(&format!("?project=shop&a={A}&b={B}"));
        assert_eq!(q.both(), Some((A.parse().unwrap(), B.parse().unwrap())));
        let half = CompareQuery::parse(&format!("a={A}&b=nope"));
        assert_eq!(half.a, Some(A.parse().unwrap()));
        assert_eq!(half.b, None);
        assert_eq!(half.both(), None);
    }

    #[test]
    fn the_pickers_label_an_attempt_by_what_a_person_knows_of_it() {
        let attempts: Vec<AttemptView> =
            vec![serde_json::from_value(attempt(A, "acme/app:41")).unwrap()];
        assert_eq!(
            attempt_options(&attempts, &ctx()),
            [AttemptOption {
                id: A.parse().unwrap(),
                label: "acme/app:41 · attempt 1 · main · 0123456 · 2026-01-01 10:00:00".to_owned()
            }]
        );
    }

    #[test]
    fn both_sides_are_summarised_and_the_sections_put_what_changed_first() {
        let m = CompareModel::build(&view(), &ctx());
        assert_eq!(m.a.flows, "2 flows: 2 passed");
        assert_eq!(m.b.flows, "1 flow: 1 failed");
        assert_eq!(m.a.href, format!("/p/shop/pipelines/{A}"));
        let sections: Vec<_> = m
            .sections
            .iter()
            .map(|s| (s.id, s.rows.len(), s.open))
            .collect();
        assert_eq!(
            sections,
            [
                ("newly-failing", 1, true),
                ("only-in-a", 1, true),
                ("unchanged", 1, false),
            ],
            "empty sections are left out"
        );
        let counts: Vec<_> = m.summary.iter().map(|s| (s.id, s.count)).collect();
        assert_eq!(
            counts,
            [("newly-failing", 1), ("only-in-a", 1), ("unchanged", 1)],
            "newly failing is always there; the other counts only when not zero"
        );
        assert_eq!(m.failures_only_in_b, 1);
        assert_eq!(m.conditions_changed, 1);
        assert!(!m.running, "both attempts are finished");
        assert_eq!(m.truncated_note, None);
    }

    #[test]
    fn an_explanation_is_the_servers_evidence_in_words_and_nothing_more() {
        let m = CompareModel::build(&view(), &ctx());
        let failing = &m.sections[0].rows[0];
        let texts: Vec<_> = failing
            .explanations
            .iter()
            .map(|e| (e.kind, e.text.as_str()))
            .collect();
        assert_eq!(
            texts,
            [
                (
                    "condition_changed",
                    "Ran under different conditions: CPU architecture x86_64 in A, aarch64 in B; \
                     host memory 16GiB in A, not reported in B."
                ),
                (
                    "provenance_differs",
                    "The outcomes rest on different evidence: A is verified (Final JUnit report); \
                     B is provisional (Live console)."
                ),
            ]
        );
        let missing = &m.sections[1].rows[0];
        assert_eq!(missing.explanations[0].text, "Not executed in B.");
        assert!(missing.b.is_none());
        let unchanged = &m.sections[2].rows[0];
        assert!(unchanged.explanations.is_empty(), "no evidence, no reason");
    }

    #[test]
    fn a_duration_change_is_shown_not_judged_and_carries_a_caveat_when_conditions_differ() {
        let m = CompareModel::build(&view(), &ctx());
        let failing = m.sections[0].rows[0].duration.as_ref().unwrap();
        assert_eq!(failing.text, "1.5 s → 4 s (+2.5 s)");
        assert_eq!(
            failing.caveat,
            Some("The conditions differ, so this is not a like-for-like comparison.")
        );
        let same = m.sections[2].rows[0].duration.as_ref().unwrap();
        assert_eq!(same.text, "1 s → 1 s (no change)");
        assert_eq!(same.caveat, None);
        assert_eq!(
            m.sections[1].rows[0].duration, None,
            "one side has no duration"
        );
    }

    #[test]
    fn a_section_is_verified_as_a_whole_only_when_every_result_in_it_is() {
        let m = CompareModel::build(&view(), &ctx());
        let verified: Vec<_> = m.sections.iter().map(|s| (s.id, s.verified)).collect();
        // The newly failing row's B side is provisional (read live); the others are final.
        assert_eq!(
            verified,
            [
                ("newly-failing", false),
                ("only-in-a", true),
                ("unchanged", true)
            ]
        );
    }

    #[test]
    fn a_run_still_open_is_said_so_because_its_results_may_change() {
        let mut v = view();
        v.b.attempt.open_jobs = 1;
        assert!(CompareModel::build(&v, &ctx()).running);
    }

    #[test]
    fn a_failure_in_b_that_a_did_not_have_counts_even_when_a_was_not_run() {
        let mut v = view();
        // The only-in-A row failed in B instead of being absent there: a failure only B has.
        v.rows[1].b = v.rows[0].b.clone();
        let m = CompareModel::build(&v, &ctx());
        assert_eq!(m.failures_only_in_b, 2, "the new failure and this one");
    }

    #[test]
    fn a_side_shows_its_outcome_provenance_and_the_one_of_several_executions_it_picked() {
        let mut v = view();
        v.rows[0].b.as_mut().unwrap().executions = 3;
        let m = CompareModel::build(&v, &ctx());
        let b = m.sections[0].rows[0].b.as_ref().unwrap();
        assert_eq!(b.state.label, "Failed");
        assert_eq!(b.provenance, Provenance::Provisional);
        assert_eq!(b.href, format!("/flows/{F2}?project=shop"));
        assert_eq!(
            b.executions_note.as_deref(),
            Some("3 executions; the latest is shown")
        );
        let a = m.sections[0].rows[0].a.as_ref().unwrap();
        assert_eq!(a.provenance, Provenance::Final);
        assert_eq!(a.executions_note, None);
    }
}
