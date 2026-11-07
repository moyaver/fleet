// SPDX-License-Identifier: AGPL-3.0-only
//! Screen E's Leptos pieces over [`super::model`]. All user and agent text (flow names, branches,
//! failure messages) is rendered as text nodes, never as HTML (PLAN §10.1).

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs"
)]

use fleet_ui::{
    charts::view::TimeSeriesChart,
    components::{
        controls::{Button, ButtonVariant},
        display::{
            Badge, Card, EmptyState, ProvenanceBadge, ProvenanceStatement, ReadoutStrip, StatTile,
            TableBox,
        },
        forms::FilterBar,
    },
    provenance::Provenance,
};
use leptos::prelude::*;

use crate::{
    screens::history::model::{
        EvidenceLink, FailureGroup, FlakyFlow, HistoryRow, HistoryTable, StatsPanel, filter_spec,
    },
    screens::parts::PillView,
};

/// A small control on a row: the stats buttons and the copy button.
const SMALL_BUTTON: &str = "inline-flex min-h-8 items-center rounded border border-line-strong px-2.5 text-sm text-fg hover:bg-band max-[720px]:min-h-11";

/// The filter band of screen E: every edit rewrites the URL query through `on_change`.
#[component]
pub fn HistoryFilters(
    /// The current URL query string.
    #[prop(into)]
    query: Signal<String>,
    /// Called with the new query string (no leading `?`).
    on_change: Callback<String>,
) -> impl IntoView {
    view! {
        <section aria-label="History filters" data-testid="history-filters" class="border-y border-line py-4">
            <FilterBar spec=filter_spec() query=query on_change=on_change />
        </section>
    }
}

/// The links to the runs behind a verdict.
#[component]
fn Links(
    /// The links.
    links: Vec<EvidenceLink>,
    /// A stable hook for browser tests.
    testid: &'static str,
) -> impl IntoView {
    view! {
        <ul class="flex flex-wrap gap-x-3 gap-y-1 text-sm">
            {links
                .into_iter()
                .map(|l| {
                    view! {
                        <li>
                            <a data-testid=testid class="text-primary underline" href=l.href>{l.label}</a>
                        </li>
                    }
                })
                .collect_view()}
        </ul>
    }
}

/// A failure's fingerprint, behind a disclosure, with a button that copies it.
#[component]
pub fn FailureFingerprint(
    /// The fingerprint, `v1:…`.
    fingerprint: String,
) -> impl IntoView {
    let copy = fingerprint.clone();
    let attr = fingerprint.clone();
    view! {
        <details class="mt-1.5 text-[12.5px] text-muted" data-testid="failure-fingerprint" data-fingerprint=attr>
            <summary class="inline-flex min-h-8 cursor-pointer items-center text-fg underline decoration-line-strong underline-offset-[3px]">
                "Fingerprint"
            </summary>
            <p class="mt-1 flex flex-wrap items-center gap-2.5">
                <code class="font-mono text-[13px] text-fg">{fingerprint}</code>
                <button
                    type="button"
                    class=SMALL_BUTTON
                    aria-label="Copy failure fingerprint"
                    on:click=move |_| copy_text(&copy)
                >
                    "Copy"
                </button>
            </p>
        </details>
    }
}

/// Puts `text` on the clipboard. The browser may refuse (a page that is not secure): then the
/// fingerprint is still on screen to copy by hand.
fn copy_text(text: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.navigator().clipboard().write_text(text);
    }
}

/// The flows the server judges flaky, each with the runs that show it. With none, one line says
/// so, and says it is derived.
#[component]
fn FlakyFlows(
    /// The flows.
    flows: Vec<FlakyFlow>,
) -> impl IntoView {
    if flows.is_empty() {
        return view! {
            <p class="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-muted" data-testid="flaky-section">
                "No flaky flows in this window"
                <ProvenanceBadge provenance=Provenance::Derived />
            </p>
        }
        .into_any();
    }
    view! {
        <div data-testid="flaky-section">
            <Card title="Flaky flows">
                <ProvenanceStatement provenance=Provenance::Derived>
                    "Fleet's verdict from the runs below."
                </ProvenanceStatement>
                <ul class="list mt-3" data-testid="flaky-flows">
                    {flows
                        .into_iter()
                        .map(|f| {
                            view! {
                                <li data-testid="flaky-flow" data-flow=f.flow.clone() class="space-y-1">
                                    <p class="flex flex-wrap items-center gap-2">
                                        <span class="font-medium">{f.flow.clone()}</span>
                                        <Badge tone=f.tone>{f.label}</Badge>
                                    </p>
                                    <ul class="list-disc pl-5 text-sm text-muted">
                                        {f.reasons.into_iter().map(|r| view! { <li>{r}</li> }).collect_view()}
                                    </ul>
                                    <Links links=f.links testid="flaky-evidence" />
                                </li>
                            }
                        })
                        .collect_view()}
                </ul>
            </Card>
        </div>
    }
    .into_any()
}

/// The history: the flaky flows, then one row per finished execution, with a "Load more" button
/// while the server has more.
#[component]
pub fn HistoryTableView(
    /// What to show.
    table: HistoryTable,
    /// Called with the cursor when "Load more" is pressed.
    on_more: Callback<String>,
    /// Called with a row's stats address (a URL query) when its "Stats" button is pressed.
    on_stats: Callback<String>,
) -> impl IntoView {
    let HistoryTable {
        rows,
        flaky,
        next_cursor,
        verified,
        ..
    } = table;
    let empty = rows.is_empty();
    let count = rows.len();
    view! {
        <div class="space-y-9">
            <FlakyFlows flows=flaky />
            {if empty {
                view! {
                    <EmptyState
                        title="No executions in this window"
                        description="Widen the time range or clear a filter."
                    />
                }
                    .into_any()
            } else {
                view! {
                    <Card title="Finished flow executions">
                        <TableBox label="Finished flow executions">
                            <table data-testid="history-table" class="tbl tbl-cards">
                                <caption class="pb-2.5 text-left text-[12.5px] text-muted">
                                    {format!("{count} flow executions, newest first.")}
                                    {verified.then(|| view! {
                                        " Results: "
                                        <ProvenanceBadge provenance=Provenance::Final />
                                    })}
                                </caption>
                                <thead>
                                    <tr>
                                        <th scope="col">"When (finished)"</th>
                                        <th scope="col">"Flow"</th>
                                        <th scope="col">"Result"</th>
                                        <th scope="col">"Branch"</th>
                                        <th scope="col" class="num">"Duration"</th>
                                        <th scope="col">"Failure"</th>
                                        <th scope="col">"Conditions"</th>
                                        <th scope="col"><span class="sr-only">"Stats"</span></th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {rows
                                        .into_iter()
                                        .map(|r| history_row(r, verified, on_stats))
                                        .collect_view()}
                                </tbody>
                            </table>
                        </TableBox>
                        {next_cursor
                            .map(|cursor| {
                                view! {
                                    <div class="pt-4">
                                        <Button
                                            variant=ButtonVariant::Secondary
                                            on_click=Callback::new(move |()| on_more.run(cursor.clone()))
                                        >
                                            "Load more"
                                        </Button>
                                    </div>
                                }
                            })}
                    </Card>
                }
                    .into_any()
            }}
        </div>
    }
}

fn history_row(r: HistoryRow, verified: bool, on_stats: Callback<String>) -> impl IntoView {
    let stats_query = r.stats_query.clone();
    let flow_label = format!("Stats for {}", r.flow);
    let failed = r.state.label == "Failed";
    let row_class = if failed {
        "border-b border-line bg-fail-tint"
    } else {
        "border-b border-line"
    };
    // The name is the flow's title; its identity shows under it only when the two differ.
    let identity = (r.name != r.flow).then(|| r.flow.clone());
    view! {
        <tr
            class=row_class
            data-testid="history-row"
            data-flow-id=r.flow_id.to_string()
            data-flow=r.flow.clone()
            data-state=r.state.label
        >
            <td data-label="When (finished)" class="num">{r.time}</td>
            <td data-label="Flow">
                <a class="cell-title text-fg underline decoration-line-strong underline-offset-[3px]" href=r.href>{r.name}</a>
                {identity.map(|i| view! { <div class="cell-sub trunc num">{i}</div> })}
            </td>
            <td data-label="Result">
                <span class="inline-flex flex-wrap items-center gap-x-3 gap-y-1" title=r.finality_note>
                    <PillView pill=r.state />
                    {(!verified).then(|| view! { <ProvenanceBadge provenance=r.provenance /> })}
                    {r
                        .flaky
                        .map(|b| view! { <span data-testid="flaky-badge"><Badge tone=b.tone>{b.label}</Badge></span> })}
                </span>
            </td>
            <td data-label="Branch">
                {r.branch.unwrap_or_default()}
                <div class="num text-xs text-muted">{r.commit.unwrap_or_default()}</div>
            </td>
            <td data-label="Duration" class="num">{r.duration.unwrap_or_else(|| "—".to_owned())}</td>
            <td data-label="Failure">
                {match r.failure {
                    Some(f) => view! {
                        <span class="block font-mono text-[13px] leading-[1.45]">{f.message.unwrap_or_default()}</span>
                        <FailureFingerprint fingerprint=f.fingerprint />
                    }
                        .into_any(),
                    None => view! { <span class="text-muted">"None"</span> }.into_any(),
                }}
            </td>
            <td data-label="Conditions">
                {r
                    .condition
                    .map(|c| view! { <span class="font-mono text-[12.5px]" data-testid="condition">{c}</span> })}
            </td>
            <td>
                <button
                    type="button"
                    class=SMALL_BUTTON
                    aria-label=flow_label
                    aria-expanded=r.stats_open.to_string()
                    data-testid="open-stats"
                    on:click=move |_| on_stats.run(stats_query.clone())
                >
                    "Stats"
                </button>
            </td>
        </tr>
    }
}

/// The per-flow panel: the counts and rates, the duration of settled executions and the kinds of
/// failure. It has no box: a heading on a hairline, like every section.
#[component]
pub fn StatsView(
    /// The panel.
    panel: StatsPanel,
    /// The viewer's offset from UTC in minutes.
    utc_offset_minutes: i32,
    /// Called when the panel is closed.
    on_close: Callback<()>,
) -> impl IntoView {
    let StatsPanel {
        flow,
        window,
        tiles,
        durations,
        chart,
        failures,
        flaky,
        excluded,
    } = panel;
    let (label, hook) = (format!("Stats of {flow}"), flow.clone());
    view! {
        <section aria-label=label class="mt-11 border-t border-line pt-1" data-testid="flow-stats" data-flow=hook>
            <div class="flex flex-wrap items-center gap-4 py-3">
                <h3 class="min-w-0 break-all font-mono text-[15px] font-semibold">{flow}</h3>
                <div class="ml-auto">
                    <Button variant=ButtonVariant::Secondary on_click=on_close>"Close"</Button>
                </div>
            </div>
            <ProvenanceStatement provenance=Provenance::Derived>
                {format!("Statistics for {window}")}
            </ProvenanceStatement>
            <div class="mt-3">
                <ReadoutStrip>
                    {tiles
                        .into_iter()
                        .map(|t| {
                            view! {
                                <div data-testid="stat-tile" data-stat=t.id>
                                    <StatTile label=t.label value=t.value />
                                </div>
                            }
                        })
                        .collect_view()}
                </ReadoutStrip>
            </div>
            {(flaky.is_some() || excluded.is_some())
                .then(|| {
                    view! {
                        <p class="mt-4 flex flex-wrap items-center gap-x-3 gap-y-1 text-sm">
                            {flaky
                                .map(|f| {
                                    view! {
                                        <Badge tone=f.tone>{f.label}</Badge>
                                        <span>{f.reasons.join("; ")}</span>
                                    }
                                })}
                            {excluded
                                .map(|e| {
                                    view! {
                                        <span class="text-muted" data-testid="excluded-runs" title="Failures a lost device caused never count toward flakiness">{e}</span>
                                    }
                                })}
                        </p>
                    }
                })}
            <DurationOfSettled chart durations utc_offset_minutes />
            <FailureGroups groups=failures />
        </section>
    }
}

/// The duration of settled executions: drawn from three on, listed in words before that.
#[component]
fn DurationOfSettled(
    /// The series to draw, when there are enough points.
    chart: Option<fleet_ui::charts::series::Series>,
    /// The durations in words, in the order the server gave them.
    durations: Vec<String>,
    /// The viewer's offset from UTC in minutes.
    utc_offset_minutes: i32,
) -> impl IntoView {
    match chart {
        Some(series) => view! {
            <figure class="mt-7 space-y-1" data-testid="duration-chart">
                <figcaption class="text-sm font-medium">"Duration of settled executions"</figcaption>
                <TimeSeriesChart
                    series=Signal::derive(move || series.clone())
                    utc_offset_minutes=utc_offset_minutes
                />
            </figure>
        }
        .into_any(),
        None if durations.is_empty() => view! {
            <p class="mt-7 italic text-muted" data-unavailable>
                "No settled execution reported a duration in this window."
            </p>
        }
        .into_any(),
        None => {
            let n = durations.len();
            view! {
                <p class="mt-7 text-sm" data-testid="duration-summary">
                    {format!(
                        "Duration of settled executions: {n} {}: {}",
                        if n == 1 { "execution" } else { "executions" },
                        durations.join(", ")
                    )}
                </p>
            }
            .into_any()
        }
    }
}

#[component]
fn FailureGroups(groups: Vec<FailureGroup>) -> impl IntoView {
    if groups.is_empty() {
        return view! { <p class="mt-7 text-sm text-muted">"No failures in this window."</p> }
            .into_any();
    }
    let count = groups.len();
    view! {
        <section class="mt-9" aria-labelledby="fg-h">
            <div class="flex flex-wrap items-baseline gap-4 border-t border-line py-3">
                <h3 id="fg-h" class="text-[15px] font-semibold">"Recent failures, grouped by fingerprint"</h3>
                <span class="note">{if count == 1 { "1 group".to_owned() } else { format!("{count} groups") }}</span>
            </div>
            <ul class="list" data-testid="failure-groups">
                {groups
                    .into_iter()
                    .map(|g| {
                        view! {
                            <li data-testid="failure-group">
                                <p class="font-mono text-[13px] leading-[1.45]">{g.message}</p>
                                <FailureFingerprint fingerprint=g.fingerprint />
                                <p class="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-sm">
                                    <span class="chip chip-fail">{g.count}</span>
                                    <span class="text-muted">{format!("last {}", g.last_seen)}</span>
                                    {g.step.map(|s| view! { <span class="text-muted">{format!("Step: {s}")}</span> })}
                                </p>
                                <div class="mt-2">
                                    <Links links=g.links testid="failure-link" />
                                </div>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        </section>
    }
    .into_any()
}
