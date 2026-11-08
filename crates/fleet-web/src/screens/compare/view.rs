// SPDX-License-Identifier: AGPL-3.0-only
//! Screen F's Leptos pieces over [`super::model`]. Flow names and explanations are rendered as
//! text nodes, never as HTML (PLAN §10.1).

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs"
)]

use fleet_protocol::rest::history::ChangeKind;
use fleet_ui::{
    components::display::{
        EmptyState, ProvenanceBadge, ProvenanceStatement, ReadoutStrip, RowProvenance, StatTile,
        TableBox,
    },
    provenance::Provenance,
    tone::Tone,
};
use leptos::prelude::*;
use uuid::Uuid;

use crate::screens::{
    compare::model::{
        AttemptOption, CompareModel, ExplanationText, Row, Section, SideCell, SideSummary,
    },
    history::view::FailureFingerprint,
    parts::{Notice, PillView},
};

const SELECT: &str = "min-h-8 w-[330px] rounded-none border-0 border-b border-line-strong bg-transparent px-0.5 py-1 text-sm text-fg max-[720px]:w-full";
const BUTTON: &str = "inline-flex min-h-8 items-center rounded border border-line-strong px-3 text-sm text-fg hover:bg-band max-[720px]:min-h-11";

/// Which picker was changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// Attempt A.
    A,
    /// Attempt B.
    B,
}

/// Two run pickers, labelled underlined, with the swap between them. `on_pick` gets the picker
/// and the chosen attempt, `None` for the blank entry; `on_swap` is asked to exchange A and B.
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
    /// Called when the swap button is pressed.
    on_swap: Callback<()>,
) -> impl IntoView {
    let picker = move |which: Which,
                       id: &'static str,
                       label: &'static str,
                       current: Signal<Option<Uuid>>| {
        let options = options.clone();
        view! {
            <div class="flex min-w-0 flex-col gap-1">
                <label for=id class="text-xs text-muted">{label}</label>
                <select
                    id=id
                    class=SELECT
                    data-testid=id
                    on:change=move |ev| on_pick.run((which, event_target_value(&ev).parse().ok()))
                >
                    <option value="" selected=move || current.get().is_none()>"Choose a run"</option>
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
        <form
            class="flex flex-wrap items-end gap-x-7 gap-y-5 border-y border-line py-4"
            aria-label="Attempts to compare"
            on:submit=|ev| ev.prevent_default()
        >
            {picker(Which::A, "compare-a", "Baseline (A)", a)}
            <button
                type="button"
                class=BUTTON
                aria-label="Swap baseline (A) and run to check (B)"
                on:click=move |_| on_swap.run(())
            >
                "Swap A and B"
            </button>
            {picker(Which::B, "compare-b", "Run to check (B)", b)}
        </form>
    }
}

/// The comparison: the two runs, the answer, the counts, then a section per kind of change.
#[component]
pub fn ComparisonView(
    /// What to show.
    model: CompareModel,
) -> impl IntoView {
    let CompareModel {
        a,
        b,
        failures_only_in_b,
        summary,
        conditions_changed,
        running,
        sections,
        truncated_note,
    } = model;
    view! {
        <div data-testid="comparison">
            {truncated_note.map(|t| view! { <Notice tone=Tone::Warning text=t /> })}
            {running.then(|| view! {
                <p class="note mt-4" data-testid="running-run-note">
                    "A run still in progress can be chosen. Its side then reads Provisional and its results may still change."
                </p>
            })}
            <div class="mt-5 flex flex-wrap gap-x-10 gap-y-2" data-testid="compare-sides">
                <SideLink side=a />
                <SideLink side=b />
            </div>
            <div class="mt-9">
                <ProvenanceStatement provenance=Provenance::Derived>
                    "Computed from the results of both runs."
                </ProvenanceStatement>
            </div>
            <div class="mt-4 flex flex-wrap items-baseline gap-5" data-testid="failures-only-in-b">
                <span class="text-[15px] font-semibold">"Failures only in B"</span>
                <span class=move || {
                    if failures_only_in_b > 0 { "font-mono text-[34px] font-medium tracking-[-0.02em] text-fail" } else { "font-mono text-[34px] font-medium tracking-[-0.02em]" }
                }>
                    {(failures_only_in_b > 0).then(|| view! { <span aria-hidden="true" class="mr-2 text-[20px]">"✕"</span> })}
                    {failures_only_in_b.to_string()}
                </span>
            </div>
            <ReadoutStrip>
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
            </ReadoutStrip>
            {(conditions_changed > 0).then(|| view! {
                <p class="meta" data-testid="conditions-note">
                    {format!(
                        "Ran under different conditions: {} {}",
                        conditions_changed,
                        if conditions_changed == 1 { "flow" } else { "flows" }
                    )}
                </p>
            })}
            {if sections.is_empty() {
                view! { <EmptyState title="Nothing to compare" description="Neither run has executed a flow." /> }
                    .into_any()
            } else {
                sections.into_iter().map(section).collect_view().into_any()
            }}
        </div>
    }
}

/// One compared run, as a link to its pipeline with its flow count.
#[component]
fn SideLink(side: SideSummary) -> impl IntoView {
    view! {
        <a
            href=side.href
            class="text-sm"
            data-testid="compare-side"
            data-side=side.name.to_lowercase()
        >
            <span class="text-muted">{format!("{}: ", side.name)}</span>
            <span class="font-medium">{side.label}</span>
            <span class="block text-sm text-muted" data-testid="compare-side-flows">{side.flows}</span>
        </a>
    }
}

/// One kind of change: its heading and rule on the line, the rows behind a toggle.
fn section(s: Section) -> impl IntoView {
    let title_id = format!("section-{}", s.id);
    let show_why = s.change != ChangeKind::Unchanged;
    let verified = s.verified;
    let heading = format!("{} ({})", s.heading, s.rows.len());
    view! {
        <div class="sec-block">
            <div class="sec-h">
                <h2 id=title_id>{heading}</h2>
                <span class="note">
                    {s.meaning}
                    {verified.then(|| view! {
                        " · results "
                        <ProvenanceBadge provenance=Provenance::Final />
                    })}
                </span>
            </div>
            <details open=s.open data-testid="compare-section" data-section=s.id class="mt-1">
                <summary>
                    <span class="t-hide">"Hide"<span class="sr-only">{format!(" {}", s.heading)}</span></span>
                    <span class="t-show">"Show"<span class="sr-only">{format!(" {}", s.heading)}</span></span>
                </summary>
                <TableBox label=s.heading>
                    <table class="tbl tbl-cards">
                        <caption class="sr-only">{s.heading}</caption>
                        <thead>
                            <tr>
                                <th scope="col">"Flow"</th>
                                <th scope="col">"A"</th>
                                <th scope="col">"B"</th>
                                <th scope="col" class="num">"Duration A to B"</th>
                                {show_why.then(|| view! { <th scope="col">"Why they differ"</th> })}
                            </tr>
                        </thead>
                        <tbody>
                            {s.rows
                                .into_iter()
                                .map(|r| compare_row(r, verified, show_why))
                                .collect_view()}
                        </tbody>
                    </table>
                </TableBox>
            </details>
        </div>
    }
}

fn compare_row(r: Row, verified: bool, show_why: bool) -> impl IntoView {
    view! {
        <tr data-testid="compare-row" data-flow=r.flow.clone() data-change=format!("{:?}", r.change)>
            <td data-label="Flow">{r.flow.clone()}</td>
            <td data-label="A" data-side="A">{side_cell(r.a, !verified)}</td>
            <td data-label="B" data-side="B">{side_cell(r.b, !verified)}</td>
            <td data-label="Duration A to B" class="num">
                {r
                    .duration
                    .map(|d| {
                        view! {
                            <span data-testid="compare-duration">{d.text}</span>
                            {d.caveat.map(|c| view! { <p class="text-xs text-muted" data-testid="duration-caveat">{c}</p> })}
                        }
                    })}
            </td>
            {show_why.then(|| view! { <td data-label="Why they differ">{explanations(r.explanations)}</td> })}
        </tr>
    }
}

/// One run's result for a flow. Its provenance shows when the section does not state it once
/// (`show_provenance`), so no value is left without its label.
fn side_cell(cell: Option<SideCell>, show_provenance: bool) -> AnyView {
    match cell {
        None => view! { <span class="text-muted">"Not executed"</span> }.into_any(),
        Some(c) => view! {
            <div class="flex flex-wrap items-center gap-x-3 gap-y-1">
                <span title=c.finality_note><PillView pill=c.state /></span>
                {show_provenance.then(|| view! { <RowProvenance provenance=c.provenance /> })}
                <a class="text-sm text-primary underline" href=c.href>"Open"</a>
            </div>
            {c.failure_fingerprint.map(|f| view! { <FailureFingerprint fingerprint=f /> })}
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
