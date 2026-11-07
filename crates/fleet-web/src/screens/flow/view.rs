// SPDX-License-Identifier: AGPL-3.0-only
//! The flow page (screen D): result, failure, steps, every claim with its provenance, the
//! device and CI environment, earlier executions and artifacts.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs"
)]

use fleet_protocol::rest::FlowDetailView;
use fleet_ui::{
    components::display::{Card, ProvenanceBadge, TableBox},
    steps::view::StepTreeView,
    tone::Tone,
};
use leptos::prelude::*;

use crate::{
    app::ErrorNotice,
    screens::{
        common::Ctx,
        device::model::DeviceSessionRow,
        flow::{
            evidence_view::{EvidenceCard, EvidenceFeed},
            model::{
                CiSection, ClaimRow, ClaimsSection, Failure, FailureDetails, FlowHeader, FlowPage,
                PreviousRow, StepRef, StepsSection,
            },
        },
        parts::{FactList, Fetched, Notice, Outcome, PillView, loaded},
    },
};

/// Screen D for one flow execution.
#[component]
pub fn FlowPageView(
    /// The flow and everything about it.
    flow: Fetched<FlowDetailView>,
    /// The project slug of the route.
    #[prop(into)]
    project: String,
    /// The current time (unix ms); update about once a second.
    #[prop(into)]
    now_ms: Signal<i64>,
    /// The viewer's offset from UTC in minutes.
    #[prop(default = 0)]
    utc_offset_minutes: i32,
    /// What the route fetched for the evidence card; picking a step in the tree moves its log.
    evidence: EvidenceFeed,
) -> impl IntoView {
    let on_select_step = Callback::new(move |id: String| evidence.selected_step.set(Some(id)));
    let ctx = Memo::new(move |_| Ctx {
        project: project.clone(),
        now_ms: now_ms.get(),
        utc_offset_minutes,
    });
    // A running flow shows "… so far": derive the header apart from the page's structure, so the
    // page isn't rebuilt every time `now_ms` ticks.
    let flow_error = flow.error;
    let header = Memo::new(move |_| {
        flow.data
            .get()
            .map(|f| FlowPage::build(&f, &ctx.read()).header)
    });
    // The evidence card sits outside the page's `loaded` block, which rebuilds on every refetch
    // of the flow (a live update); this mounts the card once, when the flow first arrives.
    let flow_ready = Memo::new(move |_| flow.data.with(Option::is_some));
    view! {
           <article class="space-y-4" aria-labelledby="flow-title" data-testid="flow-page">
               <ErrorNotice error=flow_error />
               {move || {
                   loaded(
    flow,
                       |f| {
                           let page = FlowPage::build(&f, &ctx.get_untracked());
                           view! {
                               <HeaderView header=header />
                               <FailureCard failure=page.failure />
                               <StepsCard steps=page.steps on_select=Some(on_select_step) />
                               <ClaimsCard claims=page.claims />
                               <div class="grid gap-4 lg:grid-cols-2">
                                   <DeviceCard device=page.device />
                                   <CiCard ci=page.ci />
                               </div>
                               <PreviousCard previous=page.previous />
                           }
                               .into_any()
                       },
                   )
               }}
               {move || flow_ready.get().then(|| view! { <EvidenceCard feed=evidence flow /> })}
           </article>
       }
}

/// Name, path, result and timing.
#[component]
fn HeaderView(
    /// The header, once the flow is loaded.
    header: Memo<Option<FlowHeader>>,
) -> impl IntoView {
    move || {
        header.get().map(|h: FlowHeader| {
            view! {
                <header class="space-y-2" data-testid="flow-header">
                    <div class="flex flex-wrap items-center gap-3">
                        <h2 id="flow-title" data-testid="flow-title" class="text-xl font-semibold">{h.title}</h2>
                        <Outcome pill=h.state provenance=h.provenance finality=h.finality_note />
                    </div>
                    <p class="text-sm text-muted">{h.finality_note}</p>
                    {h.path.map(|p| view! { <p class="font-mono text-sm">{p}</p> })}
                    {h.conflict_warning.map(|w| view! { <Notice tone=Tone::Warning text=w /> })}
                    <FactList facts=h.facts />
                </header>
            }
        })
    }
}

fn step_text(step: Option<StepRef>) -> String {
    step.map_or_else(
        || "Not reported".to_owned(),
        |s| format!("{} (step {})", s.label, s.index),
    )
}

/// Why it failed: message, failed step and last successful step.
#[component]
fn FailureCard(
    /// What is known.
    failure: Failure,
) -> impl IntoView {
    match failure {
        Failure::NotFailed => ().into_any(),
        Failure::Failed(FailureDetails {
            message,
            failed_step,
            last_successful_step,
        }) => view! {
            <section
                aria-labelledby="failure-title"
                class="space-y-2 rounded-lg border border-tone-danger-fg/30 bg-tone-danger-bg p-4 text-tone-danger-fg"
            >
                <h3 id="failure-title" class="font-semibold">"Failure"</h3>
                <p class="font-mono text-sm" data-testid="failure-message">
                    {message.unwrap_or_else(|| "No failure message was reported".to_owned())}
                </p>
                <dl class="grid grid-cols-[max-content_1fr] gap-x-6 gap-y-1 text-sm">
                    <dt>"Failed step"</dt>
                    <dd>{step_text(failed_step)}</dd>
                    <dt>"Last successful step"</dt>
                    <dd>{step_text(last_successful_step)}</dd>
                </dl>
            </section>
        }
        .into_any(),
    }
}

/// The step tree.
#[component]
fn StepsCard(
    /// The steps.
    steps: StepsSection,
    /// Called with a step's id.
    on_select: Option<Callback<String>>,
) -> impl IntoView {
    let StepsSection {
        tree,
        summary,
        note,
    } = steps;
    let body = if tree.is_empty() {
        ().into_any()
    } else {
        match on_select {
            Some(cb) => view! { <StepTreeView tree=tree on_select=cb /> }.into_any(),
            None => view! { <StepTreeView tree=tree /> }.into_any(),
        }
    };
    view! {
        <Card title="Steps">
            <p class="mb-2 text-sm">{summary}</p>
            {note.map(|n| view! { <p class="mb-2 text-xs text-muted">{n}</p> })}
            {body}
        </Card>
    }
}

/// Every claim about the outcome, the winner marked, disagreements highlighted.
#[component]
fn ClaimsCard(
    /// The claims.
    claims: ClaimsSection,
) -> impl IntoView {
    let body = match claims {
        ClaimsSection::Empty(text) => view! { <p class="text-muted">{text}</p> }.into_any(),
        ClaimsSection::Claims(rows) => view! {
            <TableBox label="Outcome claims">
                <table class="w-full text-left text-sm">
                    <caption class="sr-only">"Every source's claim about this flow's outcome"</caption>
                    <thead class="border-b border-border text-muted">
                        <tr>
                            {["Source", "Provenance", "Claimed", "Observed", "Recorded", "Parser", ""]
                                .map(|h| view! { <th scope="col" class="px-2 py-1 font-medium">{h}</th> })}
                        </tr>
                    </thead>
                    <tbody>{rows.into_iter().map(claim_row).collect_view()}</tbody>
                </table>
            </TableBox>
        }
        .into_any(),
    };
    view! { <Card title="Outcome evidence">{body}</Card> }
}

fn claim_row(c: ClaimRow) -> impl IntoView {
    let class = if c.disagrees {
        "border-b border-border bg-tone-warning-bg"
    } else {
        "border-b border-border"
    };
    view! {
        <tr class=class data-testid="claim" data-winning=c.winning.then_some("") data-disagrees=c.disagrees.then_some("")>
            <td class="px-2 py-1">{c.source}</td>
            <td class="px-2 py-1" data-testid="claim-provenance"><ProvenanceBadge provenance=c.provenance /></td>
            <td class="px-2 py-1"><PillView pill=c.outcome /></td>
            <td class="px-2 py-1 tabular-nums">{c.observed.unwrap_or_else(|| "—".to_owned())}</td>
            <td class="px-2 py-1 tabular-nums">{c.recorded}</td>
            <td class="px-2 py-1">{c.parser.unwrap_or_else(|| "—".to_owned())}</td>
            <td class="px-2 py-1">
                {c.winning.then(|| view! { <span class="font-medium">"Decided the outcome"</span> })}
                {c
                    .disagrees
                    .then(|| view! { <span class="font-medium">"Disagrees with the outcome"</span> })}
            </td>
        </tr>
    }
}

/// The device the flow ran on.
#[component]
fn DeviceCard(
    /// The device; `None` when no event named one.
    device: Option<DeviceSessionRow>,
) -> impl IntoView {
    let body = match device {
        None => {
            view! { <p class="text-muted">"No device was reported for this flow."</p> }.into_any()
        }
        Some(d) => view! {
            <div class="space-y-1 text-sm">
                <p>
                    <a class="font-medium underline" href=d.href>{d.serial}</a>
                    <span class="ml-2"><PillView pill=d.state /></span>
                </p>
                <p>{d.profile.unwrap_or_else(|| "Device profile not reported".to_owned())}</p>
            </div>
        }
        .into_any(),
    };
    view! { <Card title="Device">{body}</Card> }
}

/// The CI run, job and runner.
#[component]
fn CiCard(
    /// The facts and links.
    ci: CiSection,
) -> impl IntoView {
    view! {
        <Card title="CI environment">
            <FactList facts=ci.facts />
            <ul class="mt-2 flex gap-4 text-sm">
                {ci
                    .links
                    .into_iter()
                    .map(|l| view! { <li><a class="underline" href=l.href>{l.label}</a></li> })
                    .collect_view()}
            </ul>
        </Card>
    }
}

/// Earlier executions of the same flow.
#[component]
fn PreviousCard(
    /// The rows.
    previous: Vec<PreviousRow>,
) -> impl IntoView {
    let body = if previous.is_empty() {
        view! { <p class="text-muted">"No earlier executions of this flow."</p> }.into_any()
    } else {
        view! {
            <TableBox label="Earlier executions">
                <table class="w-full text-left text-sm">
                    <caption class="sr-only">"Earlier executions of this flow, newest first"</caption>
                    <thead class="border-b border-border text-muted">
                        <tr>
                            {["Started", "Outcome", "Duration", "Pipeline"]
                                .map(|h| view! { <th scope="col" class="px-2 py-1 font-medium">{h}</th> })}
                        </tr>
                    </thead>
                    <tbody>
                        {previous
                            .into_iter()
                            .map(|p| {
                                view! {
                                    <tr class="border-b border-border">
                                        <td class="px-2 py-1 tabular-nums">
                                            <a class="underline" href=p.href>{p.started}</a>
                                        </td>
                                        <td class="px-2 py-1">
                                            <Outcome pill=p.state provenance=p.provenance />
                                        </td>
                                        <td class="px-2 py-1">{p.duration.unwrap_or_else(|| "—".to_owned())}</td>
                                        <td class="px-2 py-1">
                                            <a class="underline" href=p.pipeline_href>"Pipeline"</a>
                                        </td>
                                    </tr>
                                }
                            })
                            .collect_view()}
                    </tbody>
                </table>
            </TableBox>
        }
        .into_any()
    };
    view! { <Card title="Previous executions">{body}</Card> }
}
