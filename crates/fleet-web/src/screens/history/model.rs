// SPDX-License-Identifier: AGPL-3.0-only
//! Screen E's view-model: the page's URL query, the REST query it asks for, and what the answers
//! look like as rows and a panel.
//!
//! Nothing is invented. A pass is shown with the provenance the server sent, a flow is flaky only
//! because the server's verdict names runs that show it (the links are those runs), and a figure
//! the server doesn't have (no duration samples, no percentile) reads as unavailable, never as 0.

use fleet_protocol::{
    frame::RunOrigin,
    rest::history::{
        FailureTriageView, Flakiness, FlakyEvidenceView, FlowCatalogPage, FlowFlakinessView,
        FlowStatsView, HistoryEntryView, HistoryPage, HistoryTrendView, TrendDayView,
    },
};
use fleet_ui::{
    charts::{
        format::{Unit, format_duration},
        series::{Metric, Point, Reading, Series},
    },
    filter_bar::{FilterField, FilterKind, FilterSpec},
    provenance::Provenance,
    tone::Tone,
};
use uuid::Uuid;

use crate::{
    clock::unix_ms,
    format::{ref_name, short_commit},
    paths::{escape, flow_href, query_param, with_query_param},
    screens::common::{Ctx, finality_note},
    screens::rerun::{RerunCell, manifest_href},
    status::{Pill, flow_provenance, flow_state_pill},
};

/// The CI providers screen E's filter offers: the ones the store records.
pub const CI_PROVIDERS: [&str; 2] = ["github_actions", "generic"];

/// The flow states the outcome filter offers.
pub const OUTCOMES: [&str; 5] = ["passed", "failed", "skipped", "interrupted", "unknown"];

/// The filters of the page, in the order of the filter bar and the URL.
#[must_use]
pub fn filter_spec() -> FilterSpec {
    FilterSpec::new(vec![
        FilterField::new("flow", FilterKind::Text),
        FilterField::new("tag", FilterKind::Text),
        FilterField::new("branch", FilterKind::Text),
        FilterField::new(
            "outcome",
            FilterKind::Choice(OUTCOMES.iter().map(|o| (*o).to_owned()).collect()),
        ),
        FilterField::new("device_model", FilterKind::Text),
        FilterField::new("os_version", FilterKind::Text),
        FilterField::new("runner", FilterKind::Text),
        FilterField::new("device", FilterKind::Text),
        FilterField::new(
            "ci_provider",
            FilterKind::Choice(CI_PROVIDERS.iter().map(|c| (*c).to_owned()).collect()),
        ),
        FilterField::new("commit", FilterKind::Text),
        FilterField::new("fingerprint", FilterKind::Text),
        FilterField::new("suite", FilterKind::Text),
        FilterField::new("from", FilterKind::Instant),
        FilterField::new("to", FilterKind::Instant),
    ])
}

/// The URL parameter that opens a flow's stats panel.
pub const STATS_PARAM: &str = "stats";

/// What the URL asks the page to show.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HistoryQuery {
    /// Text the flow's path or name contains.
    pub flow: Option<String>,
    /// A tag the flow's header carries.
    pub tag: Option<String>,
    /// A branch.
    pub branch: Option<String>,
    /// A flow state.
    pub outcome: Option<String>,
    /// Text the device's model contains.
    pub device_model: Option<String>,
    /// Text the device's OS version contains.
    pub os_version: Option<String>,
    /// Text the runner's name contains.
    pub runner: Option<String>,
    /// Text the device's serial contains.
    pub device: Option<String>,
    /// The CI provider.
    pub ci_provider: Option<String>,
    /// A commit SHA, or the start of one.
    pub commit: Option<String>,
    /// A failure fingerprint.
    pub fingerprint: Option<String>,
    /// A suite ID.
    pub suite: Option<String>,
    /// Window start, RFC 3339.
    pub from: Option<String>,
    /// Window end, RFC 3339.
    pub to: Option<String>,
    /// The flow whose stats panel is open.
    pub stats: Option<String>,
}

/// The filter parameters in the order the request and the URL write them.
const FILTER_PARAMS: [&str; 12] = [
    "flow",
    "tag",
    "branch",
    "outcome",
    "device_model",
    "os_version",
    "runner",
    "device",
    "ci_provider",
    "commit",
    "fingerprint",
    "suite",
];

impl HistoryQuery {
    /// Reads the page's URL query (`location.search`, with or without `?`). A value a filter
    /// doesn't accept is dropped, as the filter bar drops it.
    #[must_use]
    pub fn parse(query: &str) -> Self {
        let spec = filter_spec();
        let state = spec.decode(query);
        let get = |key: &str| state.get(&spec, key).map(str::to_owned);
        Self {
            flow: get("flow"),
            tag: get("tag"),
            branch: get("branch"),
            outcome: get("outcome"),
            device_model: get("device_model"),
            os_version: get("os_version"),
            runner: get("runner"),
            device: get("device"),
            ci_provider: get("ci_provider"),
            commit: get("commit"),
            fingerprint: get("fingerprint"),
            suite: get("suite"),
            from: get("from"),
            to: get("to"),
            stats: query_param(query, STATS_PARAM),
        }
    }

    /// The history request's query string (no `?`), continuing after `cursor`.
    #[must_use]
    pub fn rest_query(&self, cursor: Option<&str>) -> String {
        let mut parts = Vec::new();
        for (key, value) in self.params() {
            if let Some(v) = value {
                parts.push(format!("{key}={}", escape(v)));
            }
        }
        if let Some(c) = cursor {
            parts.push(format!("cursor={}", escape(c)));
        }
        parts.join("&")
    }

    /// Every filter and the window, by parameter name, in request order.
    fn params(&self) -> Vec<(&'static str, &Option<String>)> {
        let values = [
            &self.flow,
            &self.tag,
            &self.branch,
            &self.outcome,
            &self.device_model,
            &self.os_version,
            &self.runner,
            &self.device,
            &self.ci_provider,
            &self.commit,
            &self.fingerprint,
            &self.suite,
        ];
        let mut params: Vec<(&'static str, &Option<String>)> =
            FILTER_PARAMS.into_iter().zip(values).collect();
        params.push(("from", &self.from));
        params.push(("to", &self.to));
        params
    }

    /// The stats request's query string: the same window.
    #[must_use]
    pub fn window_query(&self) -> String {
        let mut parts = Vec::new();
        for (key, value) in [("from", &self.from), ("to", &self.to)] {
            if let Some(v) = value {
                parts.push(format!("{key}={}", escape(v)));
            }
        }
        parts.join("&")
    }
}

/// A link to the flow execution that is part of the evidence for a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceLink {
    /// What the run was.
    pub label: String,
    /// Its flow page.
    pub href: String,
}

/// A flow the server judges flaky, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlakyFlow {
    /// The flow's identity.
    pub flow: String,
    /// `Flaky (confirmed)` or `Suspected flaky`.
    pub label: &'static str,
    /// Its colour role.
    pub tone: Tone,
    /// One line per piece of evidence.
    pub reasons: Vec<String>,
    /// The runs behind the verdict.
    pub links: Vec<EvidenceLink>,
}

/// The marker on a row of a flaky flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlakyBadge {
    /// The words.
    pub label: &'static str,
    /// The colour role.
    pub tone: Tone,
}

/// What a failed run says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureCell {
    /// The failure fingerprint: failures with the same one are the same failure.
    pub fingerprint: String,
    /// The failure message, redacted.
    pub message: Option<String>,
    /// The project the failure belongs to, for its triage control.
    pub project: String,
    /// What was recorded about the fingerprint (issue #84).
    pub triage: Option<FailureTriageView>,
}

/// One finished flow execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    /// The flow execution.
    pub flow_id: Uuid,
    /// Its page.
    pub href: String,
    /// The flow's identity, which names its stats panel.
    pub flow: String,
    /// The flow's name.
    pub name: String,
    /// When it ended, in the viewer's zone.
    pub time: String,
    /// Its state.
    pub state: Pill,
    /// What the state rests on (invariant I4).
    pub provenance: Provenance,
    /// How settled it is, in words.
    pub finality_note: &'static str,
    /// Whether the run was published after the fact (shown as a label, never as live).
    pub origin: RunOrigin,
    /// The branch, or the ref when it isn't a branch or tag.
    pub branch: Option<String>,
    /// The commit, shortened.
    pub commit: Option<String>,
    /// How long it took.
    pub duration: Option<String>,
    /// Its failure, for a failed run.
    pub failure: Option<FailureCell>,
    /// The conditions it ran under: the first 8 digits of the fingerprint.
    pub condition: Option<String>,
    /// The flaky marker, when the flow is judged flaky.
    pub flaky: Option<FlakyBadge>,
    /// Set when the device was lost during the run, so the failure is the device's (issue #145):
    /// the label's provenance, which is the failure's.
    pub infrastructure: Option<Provenance>,
    /// The page address that opens this flow's stats panel.
    pub stats_query: String,
    /// Whether this flow's stats panel is the open one.
    pub stats_open: bool,
    /// The flow's tags, from its header.
    pub tags: Vec<String>,
    /// What the run ran on, as lines: the device, its OS, the runner, the CI and the suite.
    pub facts: Vec<String>,
    /// For a failed run, how to rerun it (issue #82).
    pub rerun: Option<RerunCell>,
}

/// Screen E's table and the flaky flows on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryTable {
    /// The executions, newest first.
    pub rows: Vec<HistoryRow>,
    /// The flaky flows among them, with their evidence.
    pub flaky: Vec<FlakyFlow>,
    /// Pass this to the server for more rows; `None` when the window is exhausted.
    pub next_cursor: Option<String>,
    /// What window the rows are from.
    pub window: String,
    /// Whether every row is verified: then the table says so once and rows stay silent.
    pub verified: bool,
}

impl HistoryTable {
    /// The table of `first` and the pages loaded after it.
    #[must_use]
    pub fn build(first: &HistoryPage, more: &[HistoryPage], ctx: &Ctx, current: &str) -> Self {
        let pages: Vec<&HistoryPage> = std::iter::once(first).chain(more).collect();
        let verdicts = |flow: &str| pages.iter().flat_map(|p| &p.flows).find(|v| v.flow == flow);
        let rows: Vec<HistoryRow> = pages
            .iter()
            .flat_map(|p| &p.items)
            .map(|e| row(e, verdicts(&e.flow), ctx, current))
            .collect();
        let mut flaky: Vec<FlakyFlow> = Vec::new();
        for v in pages.iter().flat_map(|p| &p.flows) {
            if v.flakiness != Flakiness::NotFlaky && flaky.iter().all(|f| f.flow != v.flow) {
                flaky.push(flaky_flow(v, ctx));
            }
        }
        let verified = rows.iter().all(|r| r.provenance == Provenance::Final);
        Self {
            verified,
            rows,
            flaky,
            next_cursor: pages.last().and_then(|p| p.next_cursor.clone()),
            window: format!(
                "{} to {}",
                ctx.time(unix_ms(first.from)),
                ctx.time(unix_ms(first.to))
            ),
        }
    }
}

fn row(
    e: &HistoryEntryView,
    verdict: Option<&FlowFlakinessView>,
    ctx: &Ctx,
    current: &str,
) -> HistoryRow {
    HistoryRow {
        flow_id: e.flow_id,
        href: flow_href(&ctx.project, e.flow_id),
        flow: e.flow.clone(),
        name: e.name.clone(),
        time: ctx.time(unix_ms(e.at)),
        state: flow_state_pill(&e.state),
        provenance: flow_provenance(&e.finality, &e.state, e.outcome_source.as_deref(), None),
        finality_note: finality_note(&e.finality),
        origin: e.origin,
        branch: e.git_ref.as_deref().map(|r| ref_name(r).to_owned()),
        commit: e.commit_sha.as_deref().map(short_commit),
        duration: e.duration_ms.map(format_duration),
        failure: e.failure_fingerprint.as_ref().map(|f| FailureCell {
            fingerprint: f.clone(),
            message: e.failure_message.clone(),
            project: ctx.project.clone(),
            triage: e.triage.clone(),
        }),
        condition: e
            .condition_fingerprint
            .as_deref()
            .map(|c| c.chars().take(8).collect()),
        flaky: verdict.and_then(|v| badge(v.flakiness)),
        infrastructure: e
            .infrastructure
            .as_ref()
            .map(|i| flow_provenance(&i.finality, "failed", None, None)),
        stats_query: with_query_param(current, STATS_PARAM, Some(&e.flow)),
        stats_open: query_param(current, STATS_PARAM).as_deref() == Some(e.flow.as_str()),
        tags: e.tags.clone(),
        facts: run_facts(e),
        rerun: (e.state == "failed").then(|| RerunCell {
            repro: e.reproduction.clone(),
            manifest: manifest_href(&ctx.project, e.attempt_id),
        }),
    }
}

/// What a run ran on, one line each, for the facts the server knows; a fact it doesn't is left out.
fn run_facts(e: &HistoryEntryView) -> Vec<String> {
    let mut facts = Vec::new();
    if let Some(model) = &e.device_model {
        facts.push(format!("Device model {model}"));
    }
    if let Some(os) = &e.os_version {
        facts.push(format!("OS version {os}"));
    }
    if let Some(device) = &e.device {
        facts.push(format!("Device {device}"));
    }
    if let Some(runner) = &e.runner {
        facts.push(format!("Runner {runner}"));
    }
    if let Some(ci) = &e.ci_provider {
        facts.push(format!("CI {ci}"));
    }
    if let Some(suite) = e.suite_id {
        facts.push(format!("Suite {suite}"));
    }
    facts
}

fn badge(flakiness: Flakiness) -> Option<FlakyBadge> {
    match flakiness {
        Flakiness::NotFlaky => None,
        Flakiness::Suspected => Some(FlakyBadge {
            label: "Suspected flaky",
            tone: Tone::Warning,
        }),
        Flakiness::Confirmed => Some(FlakyBadge {
            label: "Flaky (confirmed)",
            tone: Tone::Danger,
        }),
    }
}

fn flaky_flow(v: &FlowFlakinessView, ctx: &Ctx) -> FlakyFlow {
    let badge = badge(v.flakiness).unwrap_or(FlakyBadge {
        label: "Not flaky",
        tone: Tone::Neutral,
    });
    let mut reasons = Vec::new();
    let mut links = Vec::new();
    for e in &v.evidence {
        match e {
            FlakyEvidenceView::PassedAndFailed {
                commit,
                passed,
                failed,
                ..
            } => {
                let short = short_commit(commit);
                reasons.push(format!(
                    "Passed and failed on commit {short} under the same conditions"
                ));
                links.extend(numbered("Passed", &short, passed, ctx));
                links.extend(numbered("Failed", &short, failed, ctx));
            }
            FlakyEvidenceView::Flips { runs, flips, .. } => {
                reasons.push(format!(
                    "{flips} {} between consecutive default-branch runs in its last {}",
                    if *flips == 1 { "flip" } else { "flips" },
                    runs.len()
                ));
                links.extend(runs.iter().enumerate().map(|(i, id)| EvidenceLink {
                    label: format!("Run {} of {}", i + 1, runs.len()),
                    href: flow_href(&ctx.project, *id),
                }));
            }
        }
    }
    FlakyFlow {
        flow: v.flow.clone(),
        label: badge.label,
        tone: badge.tone,
        reasons,
        links,
    }
}

/// Links to `runs`, named `Passed on abc1234`, numbered when there are several.
fn numbered(what: &str, commit: &str, runs: &[Uuid], ctx: &Ctx) -> Vec<EvidenceLink> {
    runs.iter()
        .enumerate()
        .map(|(i, id)| EvidenceLink {
            label: if runs.len() == 1 {
                format!("{what} on {commit}")
            } else {
                format!("{what} on {commit} ({})", i + 1)
            },
            href: flow_href(&ctx.project, *id),
        })
        .collect()
}

/// One headline figure of the stats panel; `None` reads "Unavailable".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tile {
    /// What it is.
    pub label: &'static str,
    /// The figure.
    pub value: Option<String>,
    /// A stable hook for browser tests.
    pub id: &'static str,
}

/// A kind of failure of the flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureGroup {
    /// The failure fingerprint.
    pub fingerprint: String,
    /// The project the failures belong to, for their triage control.
    pub project: String,
    /// What was recorded about the fingerprint (issue #84).
    pub triage: Option<FailureTriageView>,
    /// The failing step's description, when a step failed.
    pub step: Option<String>,
    /// The failure message with volatile parts replaced.
    pub message: String,
    /// `3 failures`.
    pub count: String,
    /// When the latest one happened.
    pub last_seen: String,
    /// Some of them, newest first.
    pub links: Vec<EvidenceLink>,
}

/// Screen E's per-flow panel.
#[derive(Debug, Clone, PartialEq)]
pub struct StatsPanel {
    /// The flow's identity.
    pub flow: String,
    /// The window the figures are from.
    pub window: String,
    /// The headline figures: the counts and the pass rate, then the other figures that are known
    /// and not zero.
    pub tiles: Vec<Tile>,
    /// Each settled execution's duration, in the server's order, as text.
    pub durations: Vec<String>,
    /// Duration of settled executions over time; `None` below [`CHART_MIN_POINTS`].
    pub chart: Option<Series>,
    /// The latest kinds of failure.
    pub failures: Vec<FailureGroup>,
    /// The flow's flaky verdict, when it is flaky.
    pub flaky: Option<FlakyFlow>,
    /// How many failed runs a lost device excluded from flakiness, in words; `None` when none.
    pub excluded: Option<String>,
}

impl StatsPanel {
    /// The panel of `view`.
    #[must_use]
    pub fn build(view: &FlowStatsView, ctx: &Ctx) -> Self {
        let c = &view.counts;
        let settled = c.passed + c.failed;
        let pass_rate = (settled > 0).then(|| {
            // Whole percent, rounded half up: the figure is for orientation, the counts are exact.
            format!("{}%", (c.passed * 100 + settled / 2) / settled)
        });
        // The pass rate is always asked for: with nothing settled it is unavailable, not zero.
        // The other figures are shown only when they say something (a zero or a missing value
        // is left out, never printed as a number).
        let mut tiles = vec![
            tile("Passed", Some(c.passed.to_string()), "passed"),
            tile("Failed", Some(c.failed.to_string()), "failed"),
            tile("Pass rate", pass_rate, "pass-rate"),
        ];
        if c.other > 0 {
            tiles.push(tile("Not settled", Some(c.other.to_string()), "other"));
        }
        if let Some(p50) = view.duration.p50_ms {
            tiles.push(tile(
                "Median duration (passes)",
                Some(format_duration(p50)),
                "p50",
            ));
        }
        if let Some(p90) = view.duration.p90_ms {
            tiles.push(tile(
                "p90 duration (passes)",
                Some(format_duration(p90)),
                "p90",
            ));
        }
        if view.flaky_commits > 0 {
            tiles.push(tile(
                "Flaky commits",
                Some(view.flaky_commits.to_string()),
                "flaky-commits",
            ));
        }
        let durations: Vec<String> = view
            .durations
            .iter()
            .map(|d| format_duration(d.duration_ms))
            .collect();
        let points: Vec<Point> = view
            .durations
            .iter()
            .map(|d| Point {
                at_ms: unix_ms(d.at),
                reading: Reading::Value(duration_value(d.duration_ms)),
            })
            .collect();
        let chart = (points.len() >= CHART_MIN_POINTS).then(|| {
            Series::new(
                Metric {
                    name: "Duration".to_owned(),
                    unit: Unit::Millis,
                },
                points,
            )
        });
        Self {
            flow: view.flow.clone(),
            window: format!(
                "{} to {}",
                ctx.time(unix_ms(view.from)),
                ctx.time(unix_ms(view.to))
            ),
            tiles,
            durations,
            chart,
            failures: view
                .recent_failures
                .iter()
                .map(|g| FailureGroup {
                    fingerprint: g.fingerprint.clone(),
                    project: ctx.project.clone(),
                    triage: g.triage.clone(),
                    step: Some(g.step_kind.clone()).filter(|s| !s.is_empty()),
                    message: g.message.clone(),
                    count: if g.count == 1 {
                        "1 failure".to_owned()
                    } else {
                        format!("{} failures", g.count)
                    },
                    last_seen: ctx.time(unix_ms(g.last_at)),
                    links: g
                        .flow_ids
                        .iter()
                        .enumerate()
                        .map(|(i, id)| EvidenceLink {
                            label: format!("Failure {}", i + 1),
                            href: flow_href(&ctx.project, *id),
                        })
                        .collect(),
                })
                .collect(),
            flaky: (view.flakiness.flakiness != Flakiness::NotFlaky)
                .then(|| flaky_flow(&view.flakiness, ctx)),
            excluded: excluded_note(view.flakiness.excluded),
        }
    }
}

/// Fewer settled durations than this are listed in text, not drawn: a line of three points or
/// fewer says little more than its numbers.
pub const CHART_MIN_POINTS: usize = 3;

/// The note on runs a lost device excluded, `None` when there are none.
fn excluded_note(excluded: i64) -> Option<String> {
    match excluded {
        n if n <= 0 => None,
        1 => Some("1 run excluded (device lost)".to_owned()),
        n => Some(format!("{n} runs excluded (device lost)")),
    }
}

fn tile(label: &'static str, value: Option<String>, id: &'static str) -> Tile {
    Tile { label, value, id }
}

/// A duration in milliseconds as a chart value.
#[expect(
    clippy::cast_precision_loss,
    reason = "durations are far below 2^53 milliseconds"
)]
fn duration_value(ms: i64) -> f64 {
    ms as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_ui::mark::Mark;

    const A: &str = "00000000-0000-0000-0000-0000000000a1";
    const B: &str = "00000000-0000-0000-0000-0000000000b2";
    const C: &str = "00000000-0000-0000-0000-0000000000c3";

    fn ctx() -> Ctx {
        Ctx {
            project: "shop".to_owned(),
            now_ms: 1_767_265_200_000, // 2026-01-01T11:00:00Z
            utc_offset_minutes: 0,
        }
    }

    #[test]
    fn the_url_makes_the_rest_query_and_drops_what_a_filter_does_not_accept() {
        let q = HistoryQuery::parse(
            "?project=shop&flow=login&branch=feat%2Fpay&outcome=failed\
             &from=2026-01-01T00%3A00%3A00Z&stats=flows%2Flogin.yaml",
        );
        assert_eq!(q.flow.as_deref(), Some("login"));
        assert_eq!(q.branch.as_deref(), Some("feat/pay"));
        assert_eq!(q.outcome.as_deref(), Some("failed"));
        assert_eq!(q.from.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(q.to, None);
        assert_eq!(q.stats.as_deref(), Some("flows/login.yaml"));
        assert_eq!(
            q.rest_query(None),
            "flow=login&branch=feat%2Fpay&outcome=failed&from=2026-01-01T00%3A00%3A00Z"
        );
        assert_eq!(
            q.rest_query(Some("a b")),
            "flow=login&branch=feat%2Fpay&outcome=failed&from=2026-01-01T00%3A00%3A00Z&cursor=a%20b"
        );
        assert_eq!(q.window_query(), "from=2026-01-01T00%3A00%3A00Z");
        // The project is not a filter, and a wrong outcome or time is ignored, as the bar does.
        let bad = HistoryQuery::parse("?outcome=great&from=yesterday");
        assert_eq!(bad, HistoryQuery::default());
        assert_eq!(bad.rest_query(None), "");
    }

    fn page(next: Option<&str>) -> HistoryPage {
        serde_json::from_value(serde_json::json!({
            "from": "2025-12-02T11:00:00Z",
            "to": "2026-01-01T11:00:00Z",
            "items": [
                {"flow_id": A, "flow": "flows/login.yaml", "name": "login",
                 "attempt_id": C, "repository": "o/r", "commit_sha": "0123456789abcdef",
                 "git_ref": "refs/heads/feat/pay", "state": "failed",
                 "outcome_source": "junit_final", "finality": "verified",
                 "at": "2026-01-01T10:30:00Z", "duration_ms": 12400,
                 "failure_message": "Element not found: Sign in",
                 "failure_fingerprint": "v1:0123456789abcdef",
                 "condition_fingerprint": "474c43fe3e9c4de0d9dbc6a9f76dd8439ad6c745cbb9acf44cd14959a5fe626b"},
                {"flow_id": B, "flow": "flows/login.yaml", "name": "login",
                 "attempt_id": C, "repository": null, "commit_sha": "0123456789abcdef",
                 "git_ref": "refs/heads/feat/pay", "state": "passed",
                 "outcome_source": "junit_final", "finality": "verified",
                 "at": "2026-01-01T10:00:00Z", "duration_ms": null,
                 "failure_message": null, "failure_fingerprint": null,
                 "condition_fingerprint": null},
                {"flow_id": C, "flow": "profile", "name": "profile",
                 "attempt_id": C, "repository": null, "commit_sha": null,
                 "git_ref": null, "state": "unknown",
                 "outcome_source": "server_inferred", "finality": "inferred",
                 "at": "2026-01-01T09:00:00Z", "duration_ms": null,
                 "failure_message": null, "failure_fingerprint": null,
                 "condition_fingerprint": null}
            ],
            "next_cursor": next,
            "flows": [
                {"flow": "flows/login.yaml", "flakiness": "confirmed", "evidence": [
                    {"kind": "passed_and_failed", "commit": "0123456789abcdef",
                     "condition": "474c43fe", "passed": [B], "failed": [A]}]},
                {"flow": "profile", "flakiness": "not_flaky", "evidence": []}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn a_row_says_what_ran_how_it_ended_and_on_what_evidence() {
        let table = HistoryTable::build(&page(None), &[], &ctx(), "project=shop");
        let [failed, passed, inferred] = table.rows.as_slice() else {
            panic!("three rows, got {}", table.rows.len());
        };
        assert_eq!(failed.href, format!("/flows/{A}?project=shop"));
        assert_eq!(failed.time, "2026-01-01 10:30:00");
        assert_eq!(
            (failed.state.label, failed.state.mark),
            ("Failed", Mark::Failed)
        );
        assert_eq!(failed.provenance, Provenance::Final);
        assert_eq!(failed.branch.as_deref(), Some("feat/pay"));
        assert_eq!(failed.commit.as_deref(), Some("0123456"));
        assert_eq!(failed.duration.as_deref(), Some("12 s"));
        assert_eq!(
            failed.failure,
            Some(FailureCell {
                fingerprint: "v1:0123456789abcdef".to_owned(),
                message: Some("Element not found: Sign in".to_owned()),
                project: "shop".to_owned(),
                triage: None,
            })
        );
        assert_eq!(failed.condition.as_deref(), Some("474c43fe"));
        assert_eq!(failed.infrastructure, None, "an assertion is the app's");
        assert_eq!(passed.failure, None);
        assert_eq!(passed.duration, None, "no duration is not a zero duration");
        assert_eq!(passed.condition, None);
        // An inference is shown as one, never as a pass (I3, I4).
        assert_eq!(inferred.state.label, "Unknown");
        assert_eq!(inferred.provenance, Provenance::Inferred);
        assert_eq!(inferred.branch, None);
        assert_eq!(table.window, "2025-12-02 11:00:00 to 2026-01-01 11:00:00");
    }

    #[test]
    fn a_lost_device_row_is_labelled_infrastructure_and_stays_failed() {
        let page: HistoryPage = serde_json::from_value(serde_json::json!({
            "from": "2025-12-02T11:00:00Z",
            "to": "2026-01-01T11:00:00Z",
            "items": [
                {"flow_id": A, "flow": "flows/slow.yaml", "name": "slow",
                 "attempt_id": C, "repository": "o/r", "commit_sha": null,
                 "git_ref": null, "state": "failed",
                 "outcome_source": "junit_final", "finality": "verified",
                 "at": "2026-01-01T10:30:00Z", "duration_ms": 37790,
                 "failure_message": "maestro.android.DeviceServerDiedException: Device server died",
                 "failure_fingerprint": "v1:0123456789abcdef",
                 "condition_fingerprint": null,
                 "infrastructure": {"evidence": "maestro.android.DeviceServerDiedException",
                                    "warning": "run/debug/slow/commands.json is incomplete: the flow was cut short",
                                    "source": "junit_final", "finality": "verified"}}
            ],
            "next_cursor": null,
            "flows": []
        }))
        .unwrap();
        let table = HistoryTable::build(&page, &[], &ctx(), "project=shop");
        let [row] = table.rows.as_slice() else {
            panic!("one row");
        };
        assert_eq!(
            row.infrastructure,
            Some(Provenance::Final),
            "the label carries the failure's provenance"
        );
        assert_eq!(
            (row.state.label, row.state.mark),
            ("Failed", Mark::Failed),
            "the label does not change the status"
        );
    }

    #[test]
    fn a_lost_device_label_carries_its_failures_provenance_and_marks_only_an_exception() {
        // A failure still read from the live console is provisional: the label says so with a mark.
        let page: HistoryPage = serde_json::from_value(serde_json::json!({
            "from": "2025-12-02T11:00:00Z",
            "to": "2026-01-01T11:00:00Z",
            "items": [
                {"flow_id": A, "flow": "flows/slow.yaml", "name": "slow",
                 "attempt_id": C, "repository": "o/r", "commit_sha": null,
                 "git_ref": null, "state": "failed",
                 "outcome_source": "process_exit", "finality": "provisional",
                 "at": "2026-01-01T10:30:00Z", "duration_ms": 37790,
                 "failure_message": "maestro.android.DeviceServerDiedException: Device server died",
                 "failure_fingerprint": null, "condition_fingerprint": null,
                 "infrastructure": {"evidence": "maestro.android.DeviceServerDiedException",
                                    "warning": "run/debug/slow/commands.json is incomplete: the flow was cut short",
                                    "source": "process_exit", "finality": "provisional"}}
            ],
            "next_cursor": null,
            "flows": []
        }))
        .unwrap();
        let table = HistoryTable::build(&page, &[], &ctx(), "project=shop");
        let [row] = table.rows.as_slice() else {
            panic!("one row");
        };
        assert_eq!(row.infrastructure, Some(Provenance::Provisional));
        assert!(
            row.infrastructure.is_some_and(Provenance::marks_a_row),
            "an exception carries its own mark"
        );
    }

    #[test]
    fn a_flaky_flow_is_marked_on_its_rows_and_named_with_the_runs_that_show_it() {
        let table = HistoryTable::build(&page(None), &[], &ctx(), "project=shop");
        let badge = |i: usize| table.rows[i].flaky.map(|b| b.label);
        assert_eq!(badge(0), Some("Flaky (confirmed)"));
        assert_eq!(badge(1), Some("Flaky (confirmed)"));
        assert_eq!(badge(2), None);

        let [flow] = table.flaky.as_slice() else {
            panic!("one flaky flow");
        };
        assert_eq!(flow.flow, "flows/login.yaml");
        assert_eq!(flow.label, "Flaky (confirmed)");
        assert_eq!(
            flow.reasons,
            ["Passed and failed on commit 0123456 under the same conditions"]
        );
        assert_eq!(
            flow.links,
            [
                EvidenceLink {
                    label: "Passed on 0123456".to_owned(),
                    href: format!("/flows/{B}?project=shop")
                },
                EvidenceLink {
                    label: "Failed on 0123456".to_owned(),
                    href: format!("/flows/{A}?project=shop")
                },
            ]
        );
    }

    #[test]
    fn a_row_opens_its_flows_stats_and_keeps_the_filters() {
        let table = HistoryTable::build(&page(None), &[], &ctx(), "project=shop&outcome=failed");
        let q = HistoryQuery::parse(&table.rows[0].stats_query);
        assert_eq!(q.stats.as_deref(), Some("flows/login.yaml"));
        assert_eq!(q.outcome.as_deref(), Some("failed"));
        assert!(table.rows[0].stats_query.contains("project=shop"));
        assert!(!table.rows[0].stats_open, "no panel is open");
        let open = HistoryTable::build(
            &page(None),
            &[],
            &ctx(),
            "project=shop&stats=flows%2Flogin.yaml",
        );
        assert_eq!(
            open.rows.iter().map(|r| r.stats_open).collect::<Vec<_>>(),
            [true, true, false],
            "the rows of the open flow say so, and only them"
        );
    }

    #[test]
    fn the_table_states_verified_once_only_when_every_row_is() {
        let table = HistoryTable::build(&page(None), &[], &ctx(), "");
        assert!(!table.verified, "the inferred row is not verified");
        let mut all = page(None);
        all.items.truncate(2);
        assert!(HistoryTable::build(&all, &[], &ctx(), "").verified);
    }

    #[test]
    fn more_pages_extend_the_rows_and_the_last_cursor_decides_whether_more_follow() {
        let first = page(Some("c1"));
        let second = page(None);
        let table = HistoryTable::build(&first, std::slice::from_ref(&second), &ctx(), "");
        assert_eq!(table.rows.len(), 6);
        assert_eq!(table.next_cursor, None);
        assert_eq!(table.flaky.len(), 1, "a flow is listed once");
        let open = HistoryTable::build(&first, &[], &ctx(), "");
        assert_eq!(open.next_cursor.as_deref(), Some("c1"));
    }

    fn stats(passed: i64, failed: i64, p50: Option<i64>) -> FlowStatsView {
        serde_json::from_value(serde_json::json!({
            "flow": "flows/login.yaml",
            "from": "2025-12-02T11:00:00Z", "to": "2026-01-01T11:00:00Z",
            "counts": {"passed": passed, "failed": failed, "other": 1},
            "flakiness": {"flow": "flows/login.yaml", "flakiness": "suspected", "evidence": [
                {"kind": "flips", "condition": "ab", "runs": [A, B, C], "flips": 2}]},
            "flaky_commits": 0,
            "duration": {"samples": if p50.is_some() { passed } else { 0 },
                         "p50_ms": p50, "p90_ms": p50},
            "durations": if p50.is_some() { serde_json::json!([
                {"flow_id": A, "at": "2026-01-01T10:00:00Z", "duration_ms": 3000, "state": "passed"},
                {"flow_id": B, "at": "2026-01-01T10:30:00Z", "duration_ms": 1500, "state": "failed"},
                {"flow_id": C, "at": "2026-01-01T10:45:00Z", "duration_ms": 4500, "state": "passed"}
            ]) } else { serde_json::json!([]) },
            "recent_failures": [
                {"fingerprint": "v1:0123456789abcdef", "step_kind": "Tap on \"Pay\"",
                 "message": "Device server died on <serial>", "count": 2,
                 "first_at": "2026-01-01T09:00:00Z", "last_at": "2026-01-01T10:30:00Z",
                 "flow_ids": [B, A]}
            ]
        }))
        .unwrap()
    }

    fn tiles(panel: &StatsPanel) -> Vec<(&'static str, Option<&str>)> {
        panel
            .tiles
            .iter()
            .map(|t| (t.id, t.value.as_deref()))
            .collect()
    }

    #[test]
    fn the_stats_panel_gives_exact_counts_a_pass_rate_and_the_durations_of_passes() {
        let panel = StatsPanel::build(&stats(3, 1, Some(3000)), &ctx());
        assert_eq!(
            tiles(&panel),
            [
                ("passed", Some("3")),
                ("failed", Some("1")),
                ("pass-rate", Some("75%")),
                ("other", Some("1")),
                ("p50", Some("3 s")),
                ("p90", Some("3 s")),
            ],
            "a zero flaky-commit count is left out"
        );
        assert_eq!(panel.durations, ["3 s", "1.5 s", "4.5 s"]);
        let chart = panel.chart.expect("three points to draw");
        assert_eq!(chart.metric.unit, Unit::Millis);
        assert_eq!(chart.points().len(), 3);
        assert_eq!(chart.points()[0].reading, Reading::Value(3000.0));
        let [group] = panel.failures.as_slice() else {
            panic!("one failure group");
        };
        assert_eq!(group.count, "2 failures");
        assert_eq!(group.step.as_deref(), Some("Tap on \"Pay\""));
        assert_eq!(group.last_seen, "2026-01-01 10:30:00");
        assert_eq!(group.links[0].href, format!("/flows/{B}?project=shop"));
        assert_eq!(panel.flaky.expect("suspected").label, "Suspected flaky");
    }

    #[test]
    fn fewer_than_three_durations_are_listed_not_drawn() {
        let mut view = stats(3, 1, Some(3000));
        view.durations.truncate(2);
        let panel = StatsPanel::build(&view, &ctx());
        assert_eq!(panel.chart, None);
        assert_eq!(panel.durations, ["3 s", "1.5 s"]);
    }

    #[test]
    fn the_stats_panel_says_how_many_runs_a_lost_device_excluded_and_hides_a_zero() {
        let mut view = stats(3, 1, Some(3000));
        assert_eq!(StatsPanel::build(&view, &ctx()).excluded, None);
        view.flakiness.excluded = 1;
        assert_eq!(
            StatsPanel::build(&view, &ctx()).excluded.as_deref(),
            Some("1 run excluded (device lost)")
        );
        view.flakiness.excluded = 2;
        assert_eq!(
            StatsPanel::build(&view, &ctx()).excluded.as_deref(),
            Some("2 runs excluded (device lost)")
        );
    }

    #[test]
    fn a_flow_with_no_settled_runs_has_no_pass_rate_and_no_durations_rather_than_zeros() {
        let panel = StatsPanel::build(&stats(0, 0, None), &ctx());
        assert_eq!(
            tiles(&panel),
            [
                ("passed", Some("0")),
                ("failed", Some("0")),
                ("pass-rate", None),
                ("other", Some("1")),
            ],
            "the pass rate reads unavailable; the durations it has none of are left out"
        );
        assert_eq!(panel.durations, Vec::<String>::new());
        assert_eq!(panel.chart, None);
    }
}

/// One flow in the catalog: what it is, its tags, and what it last did.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogRow {
    /// The flow's identity (its path, or its name).
    pub flow: String,
    /// The flow's name, when it differs from its identity.
    pub name: Option<String>,
    /// The flow's tags.
    pub tags: Vec<String>,
    /// Its latest finished run, or that it has none.
    pub last: LastRun,
    /// The URL that opens its stats panel.
    pub stats_query: String,
    /// Whether its stats panel is open.
    pub stats_open: bool,
}

/// What a catalog row says about a flow's last run.
#[derive(Debug, Clone, PartialEq)]
pub enum LastRun {
    /// No execution of the flow was ever recorded. Plain words, not a mark (issue #149).
    NeverRun,
    /// Executions were recorded, but none has finished.
    NoFinishedRun,
    /// The latest finished run, with its state and the provenance of that state.
    Ran {
        /// The state, as the history names it.
        pill: Pill,
        /// When it ended, in the viewer's time.
        time: String,
        /// The provenance of the state.
        provenance: Provenance,
        /// What the finality means, for a hover title.
        finality_note: &'static str,
    },
}

/// The flow catalog's rows, in the server's order.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogTable {
    /// The flows shown so far.
    pub rows: Vec<CatalogRow>,
    /// Pass this as the cursor for the next page; `None` on the last one.
    pub next_cursor: Option<String>,
}

impl CatalogTable {
    /// The rows of the pages shown so far, with the stats links for `current`.
    #[must_use]
    pub fn build(pages: &[FlowCatalogPage], ctx: &Ctx, current: &str) -> Self {
        let rows = pages
            .iter()
            .flat_map(|p| p.items.iter())
            .map(|f| {
                let name = (f.name != f.flow).then(|| f.name.clone());
                let last = match &f.last {
                    _ if f.executions == 0 => LastRun::NeverRun,
                    None => LastRun::NoFinishedRun,
                    Some(last) => LastRun::Ran {
                        pill: flow_state_pill(&last.state),
                        time: ctx.time(unix_ms(last.at)),
                        provenance: flow_provenance(
                            &last.finality,
                            &last.state,
                            last.outcome_source.as_deref(),
                            None,
                        ),
                        finality_note: finality_note(&last.finality),
                    },
                };
                CatalogRow {
                    flow: f.flow.clone(),
                    name,
                    tags: f.tags.clone(),
                    last,
                    stats_query: with_query_param(current, STATS_PARAM, Some(&f.flow)),
                    stats_open: query_param(current, STATS_PARAM).as_deref()
                        == Some(f.flow.as_str()),
                }
            })
            .collect();
        let next_cursor = pages.last().and_then(|p| p.next_cursor.clone());
        Self { rows, next_cursor }
    }
}

/// One day of the trend, as the table shows it. A figure the server doesn't have reads `—`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrendDayRow {
    /// The day, `YYYY-MM-DD`, in UTC.
    pub day: String,
    /// Settled passes.
    pub passed: String,
    /// Settled failures.
    pub failed: String,
    /// Flaky runs.
    pub flaky: String,
    /// Median duration.
    pub p50: String,
    /// 95th percentile duration.
    pub p95: String,
}

/// The project trend over the filtered runs: the headline figures, the days, and the duration
/// charts (drawn from three days on, listed in words before that).
#[derive(Debug, Clone, PartialEq)]
pub struct TrendPanel {
    /// The window, in the viewer's time.
    pub window: String,
    /// The headline figures.
    pub tiles: Vec<Tile>,
    /// One row per day that has a run, oldest first.
    pub days: Vec<TrendDayRow>,
    /// The median duration per day, when there are enough days to draw.
    pub median_chart: Option<Series>,
    /// The 95th percentile duration per day, when there are enough days to draw.
    pub p95_chart: Option<Series>,
}

impl TrendPanel {
    /// The panel of `view`.
    #[must_use]
    pub fn build(view: &HistoryTrendView, ctx: &Ctx) -> Self {
        let value = |v: Option<i64>| v.map_or_else(|| "—".to_owned(), format_duration);
        let count = |n: i64| n.to_string();
        let tiles = vec![
            tile("Passed", Some(count(view.passed)), "passed"),
            tile("Failed", Some(count(view.failed)), "failed"),
            tile("Flaky", Some(count(view.flaky)), "flaky"),
            tile(
                "Median duration",
                view.duration_p50_ms.map(format_duration),
                "p50",
            ),
            tile(
                "95th percentile",
                view.duration_p95_ms.map(format_duration),
                "p95",
            ),
        ];
        let days: Vec<&TrendDayView> = view.days.iter().collect();
        let median: Vec<Point> = days
            .iter()
            .filter_map(|d| {
                Some(Point {
                    at_ms: day_ms(&d.day)?,
                    reading: Reading::Value(duration_value(d.duration_p50_ms?)),
                })
            })
            .collect();
        let p95: Vec<Point> = days
            .iter()
            .filter_map(|d| {
                Some(Point {
                    at_ms: day_ms(&d.day)?,
                    reading: Reading::Value(duration_value(d.duration_p95_ms?)),
                })
            })
            .collect();
        let chart = |name: &str, points: Vec<Point>| {
            (points.len() >= CHART_MIN_POINTS).then(|| {
                Series::new(
                    Metric {
                        name: name.to_owned(),
                        unit: Unit::Millis,
                    },
                    points,
                )
            })
        };
        Self {
            window: format!(
                "{} to {}",
                ctx.time(unix_ms(view.from)),
                ctx.time(unix_ms(view.to))
            ),
            tiles,
            days: days
                .into_iter()
                .map(|d| TrendDayRow {
                    day: d.day.clone(),
                    passed: count(d.passed),
                    failed: count(d.failed),
                    flaky: count(d.flaky),
                    p50: value(d.duration_p50_ms),
                    p95: value(d.duration_p95_ms),
                })
                .collect(),
            median_chart: chart("Median duration", median),
            p95_chart: chart("95th percentile duration", p95),
        }
    }
}

/// Midnight UTC of a `YYYY-MM-DD` day, in unix ms; `None` for any other text.
fn day_ms(day: &str) -> Option<i64> {
    let mut parts = day.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let date = time::Date::from_calendar_date(
        year,
        time::Month::try_from(month).ok()?,
        parts.next()?.parse().ok()?,
    )
    .ok()?;
    let at = date.midnight().assume_utc();
    Some(unix_ms(at))
}

#[cfg(test)]
mod catalog_and_trend_tests {
    use super::*;
    use time::OffsetDateTime;

    fn ctx() -> Ctx {
        Ctx {
            project: "shop".to_owned(),
            now_ms: 1_767_265_200_000,
            utc_offset_minutes: 0,
        }
    }

    #[test]
    fn the_filters_of_issue_85_read_from_the_address_and_are_written_back_to_it() {
        let q = HistoryQuery::parse(
            "flow=flows%2F&tag=smoke&ci_provider=github_actions&suite=0000&device_model=Pixel&os_version=16&runner=r-1&device=5554&commit=abc&fingerprint=v1%3A0123456789abcdef&from=2026-01-01T00%3A00%3A00Z",
        );
        assert_eq!(q.tag.as_deref(), Some("smoke"));
        assert_eq!(q.ci_provider.as_deref(), Some("github_actions"));
        let rest = q.rest_query(None);
        for part in [
            "flow=flows%2F",
            "tag=smoke",
            "ci_provider=github_actions",
            "device_model=Pixel",
            "os_version=16",
            "runner=r-1",
            "device=5554",
            "commit=abc",
            "fingerprint=v1%3A0123456789abcdef",
            "suite=0000",
        ] {
            assert!(rest.contains(part), "{part} is missing from {rest}");
        }
        // A CI provider the page doesn't offer is dropped, as the filter bar drops it.
        assert_eq!(HistoryQuery::parse("ci_provider=bogus").ci_provider, None);
    }

    fn catalog_flow(
        flow: &str,
        executions: i64,
        last: bool,
    ) -> fleet_protocol::rest::history::CatalogFlowView {
        fleet_protocol::rest::history::CatalogFlowView {
            flow: flow.to_owned(),
            name: flow.to_owned(),
            tags: vec!["smoke".to_owned()],
            executions,
            last: last.then(|| fleet_protocol::rest::history::CatalogLastRunView {
                flow_id: Uuid::nil(),
                state: "failed".to_owned(),
                finality: "verified".to_owned(),
                outcome_source: Some("junit_final".to_owned()),
                at: OffsetDateTime::from_unix_timestamp(1_767_265_000).unwrap(),
            }),
        }
    }

    #[test]
    fn a_flow_that_never_ran_reads_never_run_in_words_and_a_run_shows_its_provenance() {
        let page = FlowCatalogPage {
            items: vec![
                catalog_flow("flows/new.yaml", 0, false),
                catalog_flow("flows/old.yaml", 3, true),
            ],
            next_cursor: None,
        };
        let table = CatalogTable::build(&[page], &ctx(), "");
        assert!(matches!(table.rows[0].last, LastRun::NeverRun));
        match &table.rows[1].last {
            LastRun::Ran { pill, .. } => assert_eq!(pill.label, "Failed"),
            other => panic!("a finished run is shown as one: {other:?}"),
        }
        assert_eq!(table.rows[0].tags, ["smoke"]);
    }

    #[test]
    fn the_trend_draws_from_three_days_and_a_day_without_a_duration_reads_a_dash() {
        let day = |d: &str, p50: Option<i64>| TrendDayView {
            day: d.to_owned(),
            passed: 2,
            failed: 1,
            flaky: 0,
            duration_p50_ms: p50,
            duration_p95_ms: p50.map(|v| v * 2),
        };
        let view = |days: Vec<TrendDayView>| HistoryTrendView {
            from: OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap(),
            to: OffsetDateTime::from_unix_timestamp(1_767_571_200).unwrap(),
            days,
            passed: 6,
            failed: 3,
            flaky: 0,
            duration_samples: 9,
            duration_p50_ms: Some(3000),
            duration_p95_ms: Some(6000),
        };
        let two = TrendPanel::build(
            &view(vec![day("2026-01-01", Some(3000)), day("2026-01-02", None)]),
            &ctx(),
        );
        assert!(two.median_chart.is_none(), "two days are listed, not drawn");
        assert_eq!(
            two.days[1].p50, "—",
            "a figure the server doesn't have is a dash, not 0"
        );
        let three = TrendPanel::build(
            &view(vec![
                day("2026-01-01", Some(1000)),
                day("2026-01-02", Some(2000)),
                day("2026-01-03", Some(3000)),
            ]),
            &ctx(),
        );
        let chart = three.median_chart.expect("three days are drawn");
        assert_eq!(chart.points().len(), 3);
        assert_eq!(
            chart.points()[0].at_ms,
            1_767_225_600_000,
            "midnight UTC of the first day"
        );
        assert_eq!(
            three.p95_chart.expect("p95 too").points()[2].reading,
            Reading::Value(6000.0)
        );
    }
}
