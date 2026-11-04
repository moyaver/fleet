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
        display::{Badge, Card, EmptyState, ProvenanceBadge, StatTile},
        forms::FilterBar,
    },
};
use leptos::prelude::*;

use crate::{
    screens::history::model::{
        EvidenceLink, FailureGroup, FlakyFlow, HistoryRow, HistoryTable, StatsPanel, filter_spec,
    },
    screens::parts::PillView,
};

const TH: &str = "px-2 py-1 text-left font-semibold";
const TD: &str = "px-2 py-1 align-top";

/// The filter bar of screen E: every edit rewrites the URL query through `on_change`.
#[component]
pub fn HistoryFilters(
    /// The current URL query string.
    #[prop(into)]
    query: Signal<String>,
    /// Called with the new query string (no leading `?`).
    on_change: Callback<String>,
) -> impl IntoView {
    view! {
        <section aria-label="History filters" data-testid="history-filters">
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

/// The flows the server judges flaky, each with the runs that show it.
#[component]
fn FlakyFlows(
    /// The flows.
    flows: Vec<FlakyFlow>,
) -> impl IntoView {
    view! {
        <Card title="Flaky flows">
            <ul class="space-y-3" data-testid="flaky-flows">
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
    }
}

/// The history: flaky flows first, then one row per finished execution, with a "Load more" button
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
        window,
    } = table;
    let empty = rows.is_empty();
    view! {
        <div class="space-y-4">
            <p class="text-sm text-muted" data-testid="history-window">{format!("Window: {window}")}</p>
            {(!flaky.is_empty()).then(|| view! { <FlakyFlows flows=flaky /> })}
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
                    <table data-testid="history-table" class="w-full border-collapse text-sm">
                        <caption class="sr-only">"Finished flow executions, newest first"</caption>
                        <thead class="border-b border-border">
                            <tr>
                                {["When", "Flow", "Outcome", "Branch", "Duration", "Failure", "Conditions", ""]
                                    .map(|h| view! { <th scope="col" class=TH>{h}</th> })}
                            </tr>
                        </thead>
                        <tbody>
                            {rows
                                .into_iter()
                                .map(|r| history_row(r, on_stats))
                                .collect_view()}
                        </tbody>
                    </table>
                }
                    .into_any()
            }}
            {next_cursor
                .map(|cursor| {
                    view! {
                        <div>
                            <Button
                                variant=ButtonVariant::Secondary
                                on_click=Callback::new(move |()| on_more.run(cursor.clone()))
                            >
                                "Load more"
                            </Button>
                        </div>
                    }
                })}
        </div>
    }
}

fn history_row(r: HistoryRow, on_stats: Callback<String>) -> impl IntoView {
    let stats_query = r.stats_query.clone();
    let flow_label = format!("Stats for {}", r.flow);
    view! {
        <tr
            class="border-b border-border"
            data-testid="history-row"
            data-flow-id=r.flow_id.to_string()
            data-flow=r.flow.clone()
            data-state=r.state.label
        >
            <td class=TD>{r.time}</td>
            <td class=TD>
                <a class="font-medium text-primary hover:underline" href=r.href>{r.name}</a>
                <div class="text-xs text-muted">{r.flow.clone()}</div>
            </td>
            <td class=TD>
                <span class="inline-flex flex-wrap items-center gap-2" title=r.finality_note>
                    <PillView pill=r.state />
                    <ProvenanceBadge provenance=r.provenance />
                    {r
                        .flaky
                        .map(|b| view! { <span data-testid="flaky-badge"><Badge tone=b.tone>{b.label}</Badge></span> })}
                </span>
            </td>
            <td class=TD>
                {r.branch.unwrap_or_default()}
                <div class="text-xs text-muted">{r.commit.unwrap_or_default()}</div>
            </td>
            <td class=TD>{r.duration.unwrap_or_else(|| "—".to_owned())}</td>
            <td class=TD>
                {r
                    .failure
                    .map(|f| {
                        let fingerprint = f.fingerprint.clone();
                        view! {
                            <div data-testid="failure-fingerprint" data-fingerprint=fingerprint>
                                <code class="text-xs">{f.fingerprint}</code>
                            </div>
                            <div class="text-xs text-muted">{f.message.unwrap_or_default()}</div>
                        }
                    })}
            </td>
            <td class=TD>
                {r
                    .condition
                    .map(|c| view! { <code class="text-xs" data-testid="condition">{c}</code> })}
            </td>
            <td class=TD>
                <button
                    type="button"
                    class="rounded border border-border px-2 py-1 text-xs hover:bg-tone-neutral-bg"
                    aria-label=flow_label
                    data-testid="open-stats"
                    on:click=move |_| on_stats.run(stats_query.clone())
                >
                    "Stats"
                </button>
            </td>
        </tr>
    }
}

/// The per-flow panel: headline figures, the duration chart and the kinds of failure.
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
        chart,
        failures,
        flaky,
    } = panel;
    let (label, hook) = (format!("Stats of {flow}"), flow.clone());
    view! {
        <section aria-label=label class="space-y-3 rounded-lg border border-border bg-surface p-4" data-testid="flow-stats" data-flow=hook>
            <div class="flex flex-wrap items-center gap-3">
                <h3 class="text-lg font-semibold">{flow}</h3>
                <span class="text-sm text-muted">{window}</span>
                <Button variant=ButtonVariant::Secondary on_click=on_close>"Close"</Button>
            </div>
            {flaky
                .map(|f| {
                    view! {
                        <p class="flex flex-wrap items-center gap-2 text-sm">
                            <Badge tone=f.tone>{f.label}</Badge>
                            {f.reasons.join("; ")}
                        </p>
                    }
                })}
            <div class="grid grid-cols-2 gap-3 md:grid-cols-4 lg:grid-cols-7">
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
            </div>
            <figure class="space-y-1" data-testid="duration-chart">
                <figcaption class="text-sm font-medium">"Duration of settled executions"</figcaption>
                {match chart {
                    Some(series) => {
                        view! {
                            <TimeSeriesChart
                                series=Signal::derive(move || series.clone())
                                utc_offset_minutes=utc_offset_minutes
                            />
                        }
                            .into_any()
                    }
                    None => {
                        view! {
                            <p class="italic text-muted" data-unavailable>
                                "No settled execution reported a duration in this window."
                            </p>
                        }
                            .into_any()
                    }
                }}
            </figure>
            <FailureGroups groups=failures />
        </section>
    }
}

#[component]
fn FailureGroups(groups: Vec<FailureGroup>) -> impl IntoView {
    if groups.is_empty() {
        return view! { <p class="text-sm text-muted">"No failures in this window."</p> }
            .into_any();
    }
    view! {
        <div class="space-y-2">
            <h4 class="text-sm font-semibold">"Recent failures, grouped by fingerprint"</h4>
            <ul class="space-y-2" data-testid="failure-groups">
                {groups
                    .into_iter()
                    .map(|g| {
                        let fingerprint = g.fingerprint.clone();
                        view! {
                            <li class="rounded border border-border p-2" data-testid="failure-group" data-fingerprint=fingerprint>
                                <p class="flex flex-wrap items-center gap-2 text-sm">
                                    <code class="text-xs">{g.fingerprint}</code>
                                    <span class="font-medium">{g.count}</span>
                                    <span class="text-muted">{format!("last {}", g.last_seen)}</span>
                                </p>
                                {g
                                    .step
                                    .map(|s| view! { <p class="text-xs text-muted">{format!("Step: {s}")}</p> })}
                                <p class="text-sm">{g.message}</p>
                                <Links links=g.links testid="failure-link" />
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        </div>
    }
    .into_any()
}
