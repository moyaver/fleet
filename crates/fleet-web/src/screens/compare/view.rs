// SPDX-License-Identifier: AGPL-3.0-only
//! Screen F's Leptos pieces over [`super::model`]. Flow names and explanations are rendered as
//! text nodes, never as HTML (PLAN §10.1).

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs"
)]

use fleet_ui::components::display::{Card, EmptyState, ProvenanceBadge, StatTile};
use leptos::prelude::*;
use uuid::Uuid;

use crate::{
    screens::compare::model::{
        AttemptOption, CompareModel, ExplanationText, Row, Section, SideCell, SideSummary,
    },
    screens::parts::{Notice, PillView},
};
use fleet_ui::tone::Tone;

const TH: &str = "px-2 py-1 text-left font-semibold";
const TD: &str = "px-2 py-1 align-top";
const SELECT: &str = "rounded border border-border bg-surface px-2 py-1";

/// Which picker was changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// Attempt A.
    A,
    /// Attempt B.
    B,
}

/// Two attempt pickers. `on_pick` gets the picker and the chosen attempt, `None` for the blank
/// entry.
#[component]
pub fn AttemptPickers(
    /// The attempts to choose from, newest first.
    options: Vec<AttemptOption>,
    /// The attempt A now.
    #[prop(into)]
    a: Signal<Option<Uuid>>,
    /// The attempt B now.
    #[prop(into)]
    b: Signal<Option<Uuid>>,
    /// Called when a picker changes.
    on_pick: Callback<(Which, Option<Uuid>)>,
) -> impl IntoView {
    let picker = move |which: Which,
                       id: &'static str,
                       label: &'static str,
                       current: Signal<Option<Uuid>>| {
        let options = options.clone();
        view! {
            <div>
                <label for=id class="block text-sm">{label}</label>
                <select
                    id=id
                    class=SELECT
                    data-testid=id
                    on:change=move |ev| on_pick.run((which, event_target_value(&ev).parse().ok()))
                >
                    <option value="" selected=move || current.get().is_none()>"Choose an attempt"</option>
                    {options
                        .into_iter()
                        .map(|o| {
                            let value = o.id.to_string();
                            let id = o.id;
                            view! {
                                <option value=value selected=move || current.get() == Some(id)>
                                    {o.label}
                                </option>
                            }
                        })
                        .collect_view()}
                </select>
            </div>
        }
    };
    view! {
        <form class="flex flex-wrap items-end gap-4" aria-label="Attempts to compare" on:submit=|ev| ev.prevent_default()>
            {picker(Which::A, "compare-a", "Attempt A", a)}
            {picker(Which::B, "compare-b", "Attempt B", b)}
        </form>
    }
}

/// The comparison: the two attempts, the counts, then a section per kind of change.
#[component]
pub fn ComparisonView(
    /// What to show.
    model: CompareModel,
) -> impl IntoView {
    let CompareModel {
        a,
        b,
        summary,
        sections,
        truncated_note,
    } = model;
    view! {
        <div class="space-y-4" data-testid="comparison">
            {truncated_note.map(|t| view! { <Notice tone=Tone::Warning text=t /> })}
            <div class="grid gap-3 md:grid-cols-2">
                <SideCard side=a />
                <SideCard side=b />
            </div>
            <div class="grid grid-cols-2 gap-3 md:grid-cols-4 lg:grid-cols-7">
                {summary
                    .into_iter()
                    .map(|s| {
                        view! {
                            <div data-testid="compare-count" data-count=s.id>
                                <StatTile label=s.label value=Some(s.count.to_string()) />
                            </div>
                        }
                    })
                    .collect_view()}
            </div>
            {if sections.is_empty() {
                view! { <EmptyState title="Nothing to compare" description="Neither attempt has executed a flow." /> }
                    .into_any()
            } else {
                sections.into_iter().map(section).collect_view().into_any()
            }}
        </div>
    }
}

#[component]
fn SideCard(side: SideSummary) -> impl IntoView {
    view! {
        <Card title=format!("Attempt {}", side.name)>
            <div data-testid="compare-side" data-side=side.name>
                <a class="font-medium text-primary hover:underline" href=side.href>{side.label}</a>
                <p class="text-sm text-muted" data-testid="compare-side-flows">{side.flows}</p>
            </div>
        </Card>
    }
}

fn section(s: Section) -> impl IntoView {
    let title_id = format!("section-{}", s.id);
    view! {
        <details open=s.open data-testid="compare-section" data-section=s.id class="rounded-lg border border-border bg-surface p-3">
            <summary class="cursor-pointer text-base font-semibold" id=title_id>
                {format!("{} ({})", s.heading, s.rows.len())}
                <span class="ml-2 text-sm font-normal text-muted">{s.meaning}</span>
            </summary>
            <table class="mt-2 w-full border-collapse text-sm">
                <caption class="sr-only">{s.heading}</caption>
                <thead class="border-b border-border">
                    <tr>
                        {["Flow", "A", "B", "Duration", "Why they differ"]
                            .map(|h| view! { <th scope="col" class=TH>{h}</th> })}
                    </tr>
                </thead>
                <tbody>{s.rows.into_iter().map(compare_row).collect_view()}</tbody>
            </table>
        </details>
    }
}

fn compare_row(r: Row) -> impl IntoView {
    view! {
        <tr class="border-b border-border" data-testid="compare-row" data-flow=r.flow.clone() data-change=format!("{:?}", r.change)>
            <td class=TD>{r.flow.clone()}</td>
            <td class=TD data-side="A">{side_cell(r.a)}</td>
            <td class=TD data-side="B">{side_cell(r.b)}</td>
            <td class=TD>
                {r
                    .duration
                    .map(|d| {
                        view! {
                            <span data-testid="compare-duration">{d.text}</span>
                            {d.caveat.map(|c| view! { <p class="text-xs text-muted" data-testid="duration-caveat">{c}</p> })}
                        }
                    })}
            </td>
            <td class=TD>{explanations(r.explanations)}</td>
        </tr>
    }
}

fn side_cell(cell: Option<SideCell>) -> AnyView {
    match cell {
        None => view! { <span class="italic text-muted">"Not executed"</span> }.into_any(),
        Some(c) => view! {
            <span class="inline-flex flex-wrap items-center gap-2" title=c.finality_note>
                <PillView pill=c.state />
                <ProvenanceBadge provenance=c.provenance />
                <a class="text-xs text-primary underline" href=c.href>"Open"</a>
            </span>
            {c
                .failure_fingerprint
                .map(|f| view! { <div><code class="text-xs">{f}</code></div> })}
            {c.executions_note.map(|n| view! { <div class="text-xs text-muted">{n}</div> })}
        }
        .into_any(),
    }
}

fn explanations(list: Vec<ExplanationText>) -> AnyView {
    if list.is_empty() {
        return view! { <span class="text-muted">"—"</span> }.into_any();
    }
    view! {
        <ul class="list-disc space-y-1 pl-4">
            {list
                .into_iter()
                .map(|e| view! { <li data-testid="explanation" data-kind=e.kind>{e.text}</li> })
                .collect_view()}
        </ul>
    }
    .into_any()
}
