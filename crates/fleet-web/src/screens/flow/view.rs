// SPDX-License-Identifier: AGPL-3.0-only
//! The flow page (screen D), top to bottom: the header with its result and the Verified label, the
//! failure band (or the current step of a running flow), the History line, three panes (steps,
//! evidence, logs), the claims behind the Verified label, where it ran and the files.
//!
//! On a phone the evidence and the logs come before the steps, and the steps start collapsed.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs"
)]

use fleet_protocol::rest::FlowDetailView;
use fleet_ui::{
    components::display::{Card, ProvenanceBadge, StatusPill, TableBox},
    mark::Mark,
    provenance::Provenance,
    steps::view::StepTreeView,
    tone::Tone,
};
use leptos::prelude::*;

use crate::{
    app::ErrorNotice,
    screens::{
        common::{Ctx, Fact},
        device::model::DeviceSessionRow,
        flow::{
            evidence_view::{EvidenceFeed, EvidencePane, FilesCard, LogsPane, ReportLink},
            model::{
                CiSection, ClaimsSection, CurrentStep, Failure, FlowHeader, FlowPage, HistoryLine,
                StepRef, StepsSection,
            },
        },
        parts::{FactList, Fetched, Notice, PillView, loaded},
    },
};

/// Whether the viewer is on a phone (under 720 px), where the steps start collapsed.
fn phone() -> bool {
    web_sys::window()
        .and_then(|w| w.match_media("(max-width: 720px)").ok().flatten())
        .is_some_and(|m| m.matches())
}

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
    /// What the route fetched for the evidence; picking a step in the tree moves its log.
    evidence: EvidenceFeed,
) -> impl IntoView {
    let on_select_step = Callback::new(move |id: String| evidence.selected_step.set(Some(id)));
    let ctx = Memo::new(move |_| Ctx {
        project: project.clone(),
        now_ms: now_ms.get(),
        utc_offset_minutes,
    });
    // The header and a running flow's current step follow the clock, so they are built on every
    // tick. The sections are built only when the flow changes: a tree's focus, an open disclosure
    // and the evidence viewers' state all survive a tick.
    let build = move || flow.data.get().map(|f| FlowPage::build(&f, &ctx.read()));
    let flow_ready = Memo::new(move |_| flow.data.with(Option::is_some));
    // The steps are collapsed on a phone; the state outlives a refetch, so a reader who opened them
    // keeps them open while the flow updates.
    let steps_open = RwSignal::new(!phone());
    view! {
        <article class="space-y-6" aria-labelledby="flow-title" data-testid="flow-page">
            <ErrorNotice error=flow.error />
            {move || {
                build()
                    .map(|p| {
                        view! { <HeaderView header=p.header device=p.device evidence /> }
                    })
            }}
            {move || {
                loaded(
                    flow,
                    |f| {
                        let page = FlowPage::build(&f, &ctx.get_untracked());
                        view! {
                            <FailureBand failure=page.failure />
                            <HistoryLineView history=page.history />
                        }
                            .into_any()
                    },
                )
            }}
            {move || {
                build()
                    .filter(|p| p.header.state.mark == Mark::Running)
                    .map(|p| view! { <RunBand current=p.current /> })
            }}
            <div class="flow-work">
                {move || {
                    loaded(
                        flow,
                        |f| {
                            let page = FlowPage::build(&f, &ctx.get_untracked());
                            view! {
                                <StepsPane steps=page.steps on_select=on_select_step open=steps_open />
                            }
                                .into_any()
                        },
                    )
                }}
                {move || {
                    flow_ready
                        .get()
                        .then(|| {
                            view! {
                                <EvidencePane feed=evidence />
                                <LogsPane feed=evidence flow />
                            }
                        })
                }}
            </div>
            {move || {
                loaded(
                    flow,
                    |f| {
                        let page = FlowPage::build(&f, &ctx.get_untracked());
                        view! {
                            <ClaimsCard
                                claims=page.claims
                                settled=page.settled
                                finality=page.header.finality_note
                                conflict=page.header.conflict_warning
                            />
                            <WhereItRan device=page.device ci=page.ci />
                        }
                            .into_any()
                    },
                )
            }}
            {move || flow_ready.get().then(|| view! { <FilesCard feed=evidence /> })}
        </article>
    }
}

fn fact(facts: &[Fact], label: &str) -> String {
    facts
        .iter()
        .find(|f| f.label == label)
        .map_or_else(|| "Not reported".to_owned(), |f| f.shown().to_owned())
}

/// Name, result with its provenance (linked to the claims), path, device, start and duration, and
/// the Maestro report.
#[component]
fn HeaderView(
    /// The header, laid out.
    header: FlowHeader,
    /// The device it ran on.
    device: Option<DeviceSessionRow>,
    /// The evidence, for the report link.
    evidence: EvidenceFeed,
) -> impl IntoView {
    let timing = format!(
        "{} · {}",
        fact(&header.facts, "Started"),
        fact(&header.facts, "Duration")
    );
    let FlowHeader {
        title,
        path,
        state,
        provenance,
        finality_note,
        ..
    } = header;
    view! {
        <header class="space-y-2" data-testid="flow-header">
            <h2 id="flow-title" data-testid="flow-title" class="break-words text-xl font-semibold">{title}</h2>
            <div class="row gap-3">
                <StatusPill mark=state.mark label=state.label />
                <a href="#why" class="no-underline" title=finality_note>
                    <ProvenanceBadge provenance=Signal::stored(provenance) />
                </a>
            </div>
            <p class="row gap-x-4 gap-y-1 text-sm text-muted">
                {path
                    .map(|p| {
                        let title = p.clone();
                        view! { <span class="trunc font-mono" title=title>{p}</span> }
                    })}
                {device.map(|d| view! { <span>"Device " <a class="text-fg underline" href=d.href>{d.serial}</a></span> })}
                <span class="tabular-nums">{timing}</span>
            </p>
            {move || {
                evidence
                    .files_with(|f| f.report.clone())
                    .flatten()
                    .map(|link| view! { <p><ReportLink link /></p> })
            }}
        </header>
    }
}

/// The failure, flat: the message in mono, where it failed and a copy button. Nothing is boxed; the
/// band is a tint.
#[component]
fn FailureBand(
    /// Why the flow failed, if it did.
    failure: Failure,
) -> impl IntoView {
    let Failure::Failed(details) = failure else {
        return ().into_any();
    };
    let total = details.total_steps;
    let message = details.message.clone();
    let shown = message
        .clone()
        .unwrap_or_else(|| "No failure message was reported".to_owned());
    let failed_at = match details.failed_step {
        Some(step) => failed_at_text(&step, total),
        None => "The failed step was not reported".to_owned(),
    };
    view! {
        <section class="fail-panel" aria-labelledby="fail-h" data-testid="failure">
            <div class="flex flex-wrap items-start justify-between gap-4">
                <div class="min-w-0 flex-1">
                    <h3 id="fail-h" class="text-sm font-semibold text-fail">"Failure"</h3>
                    <p class="mt-2 whitespace-pre-wrap break-words font-mono text-[15px]" data-testid="failure-message">
                        {shown}
                    </p>
                    <p class="mt-3 text-sm">
                        <span class="text-muted">"Failed at "</span>
                        {failed_at}
                    </p>
                    {details
                        .last_successful_step
                        .map(|s| {
                            view! {
                                <p class="mt-1 text-sm text-muted">
                                    "Last successful step: "
                                    {failed_at_text(&s, total)}
                                </p>
                            }
                        })}
                </div>
                {message.map(|m| view! { <CopyButton text=m /> })}
            </div>
        </section>
    }
    .into_any()
}

/// "Step 6 of 6 · assertVisible … · waited 17.2 s": the number is the flow file's, and Maestro's own
/// setup commands say so instead of a number.
fn failed_at_text(step: &StepRef, total: usize) -> String {
    let at = match &step.number {
        Some(n) => format!("Step {n} of {total}"),
        None => "Setup".to_owned(),
    };
    match &step.duration {
        Some(waited) => format!("{at} · {} · waited {waited}", step.label),
        None => format!("{at} · {}", step.label),
    }
}

/// Copies the failure message to the clipboard.
#[component]
fn CopyButton(
    /// The text to copy.
    text: String,
) -> impl IntoView {
    let copy = move |_| {
        if let Some(window) = web_sys::window() {
            // Fire and forget: a refused clipboard leaves the message where it is.
            let _ = window.navigator().clipboard().write_text(&text);
        }
    };
    view! {
        <button type="button" class="shrink-0 rounded border border-line-strong px-3 py-1 text-sm" on:click=copy>
            "Copy message"
        </button>
    }
}

fn cell_class(mark: Mark) -> &'static str {
    match mark {
        Mark::Passed => "bg-pass",
        Mark::Failed => "bg-fail",
        Mark::Running => "bg-run",
        Mark::Interrupted => "bg-warn",
        Mark::Unknown => "bg-unk",
        _ => "bg-idle",
    }
}

/// The History line: this flow's recent executions as a strip, what it says, and the History page
/// for this flow.
#[component]
fn HistoryLineView(
    /// The line, laid out.
    history: HistoryLine,
) -> impl IntoView {
    let HistoryLine {
        cells,
        strip_label,
        summary,
        href,
    } = history;
    view! {
        <div class="row gap-3 text-sm" data-testid="history-line">
            <span class="text-muted">"History"</span>
            <span class="inline-flex items-end gap-1" role="img" aria-label=strip_label>
                {cells
                    .into_iter()
                    .map(|m| view! { <i class=format!("block h-4 w-2 {}", cell_class(m)) aria-hidden="true"></i> })
                    .collect_view()}
            </span>
            <span>{summary}</span>
            <ProvenanceBadge provenance=Signal::stored(Provenance::Derived) />
            <a class="underline" href=href>"Open in History"</a>
        </div>
    }
}

/// The step a running flow is on. The flow is not failed (yet), so the page says that instead.
#[component]
fn RunBand(
    /// The step that is running, if one has started.
    current: Option<CurrentStep>,
) -> impl IntoView {
    let step = match current {
        Some(c) => {
            let at = match c.number {
                Some(n) => format!("Step {n} · {}", c.label),
                None => c.label,
            };
            view! { <b class="font-medium">{at}</b><span class="text-muted tabular-nums">{format!("for {}", c.elapsed)}</span> }
                .into_any()
        }
        None => view! { <span class="text-muted">"No step has started yet"</span> }.into_any(),
    };
    view! {
        <section class="run-band" aria-label="Current step" data-testid="run-band">
            <p class="row gap-3">
                <StatusPill mark=Mark::Running label="Running" />
                {step}
            </p>
            <p class="mt-2 text-sm text-muted">
                "No failure so far. If a step fails, its message, screenshot and UI hierarchy appear here first."
            </p>
        </section>
    }
}

/// The step tree. Its heading says once how the steps are known; on a phone they start collapsed
/// behind a button.
#[component]
fn StepsPane(
    /// The steps, laid out.
    steps: StepsSection,
    /// Called with a step's id when it is picked.
    on_select: Callback<String>,
    /// Whether the steps are shown.
    open: RwSignal<bool>,
) -> impl IntoView {
    let StepsSection {
        tree,
        summary,
        note,
        provenance,
    } = steps;
    view! {
        <section class="card flow-steps" aria-labelledby="st-h">
            <div class="card-h">
                <h2 id="st-h">"Steps"</h2>
                <span class="note">{summary}</span>
                <ProvenanceBadge provenance=Signal::stored(provenance) />
            </div>
            {note.map(|n| view! { <p class="mb-2 text-xs text-muted">{n}</p> })}
            <button
                type="button"
                class="only-phone min-h-11 text-sm underline"
                aria-controls="steps-list"
                aria-expanded=move || open.get().to_string()
                on:click=move |_| open.update(|o| *o = !*o)
            >
                {move || if open.get() { "Hide the steps" } else { "Show the steps" }}
            </button>
            <div id="steps-list" hidden=move || !open.get()>
                <StepTreeView tree on_select />
            </div>
        </section>
    }
}

/// Every claim about the outcome, the winner marked, disagreements highlighted. Behind a disclosure,
/// open by default when the sources disagree.
#[component]
fn ClaimsCard(
    /// The claims.
    claims: ClaimsSection,
    /// Whether the outcome is final.
    settled: bool,
    /// What the finality means, in words.
    finality: &'static str,
    /// Set when two parsed sources disagree.
    conflict: Option<&'static str>,
) -> impl IntoView {
    let heading = if settled {
        "Why this result is verified"
    } else {
        "Outcome claims so far"
    };
    let disagree =
        matches!(&claims, ClaimsSection::Claims(rows) if rows.iter().any(|r| r.disagrees));
    let agreement = match &claims {
        ClaimsSection::Empty(_) => String::new(),
        ClaimsSection::Claims(_) if disagree => {
            "Sources disagree; the winning claim is marked".to_owned()
        }
        ClaimsSection::Claims(rows) => {
            let n = rows.len();
            format!("{n} {} agree", if n == 1 { "source" } else { "sources" })
        }
    };
    let body = match claims {
        ClaimsSection::Empty(text) => view! { <p class="text-muted">{text}</p> }.into_any(),
        ClaimsSection::Claims(rows) => view! {
            <details open=disagree>
                <summary class="note cursor-pointer pb-2">"Show the sources"</summary>
                <TableBox label="Outcome claims">
                    <table class="tbl tbl-cards w-full text-left text-sm">
                        <caption class="sr-only">"Every source's claim about this flow's outcome"</caption>
                        <thead class="border-b border-line text-muted">
                            <tr>
                                {["Source", "Provenance", "Claimed", "Observed", "Recorded", "Parser", "Note"]
                                    .map(|h| view! { <th scope="col" class="px-2 py-1 font-medium">{h}</th> })}
                            </tr>
                        </thead>
                        <tbody>{rows.into_iter().map(claim_row).collect_view()}</tbody>
                    </table>
                </TableBox>
            </details>
        }
        .into_any(),
    };
    view! {
        <section class="card" id="why" aria-labelledby="why-h">
            <div class="card-h">
                <h2 id="why-h">{heading}</h2>
                <span class="note">{agreement}</span>
            </div>
            <div class="card-b space-y-2">
                <p class="note">{finality}</p>
                {conflict.map(|w| view! { <Notice tone=Tone::Warning text=w /> })}
                {body}
            </div>
        </section>
    }
}

fn claim_row(c: crate::screens::flow::model::ClaimRow) -> impl IntoView {
    let class = if c.disagrees {
        "border-b border-line bg-band"
    } else {
        "border-b border-line"
    };
    view! {
        <tr class=class data-testid="claim" data-winning=c.winning.then_some("") data-disagrees=c.disagrees.then_some("")>
            <td class="px-2 py-1" data-label="Source">{c.source}</td>
            <td class="px-2 py-1" data-label="Provenance" data-testid="claim-provenance">
                <ProvenanceBadge provenance=Signal::stored(c.provenance) />
            </td>
            <td class="px-2 py-1" data-label="Claimed"><PillView pill=c.outcome /></td>
            <td class="px-2 py-1 tabular-nums" data-label="Observed">{c.observed.unwrap_or_else(|| "—".to_owned())}</td>
            <td class="px-2 py-1 tabular-nums" data-label="Recorded">{c.recorded}</td>
            <td class="px-2 py-1" data-label="Parser">{c.parser.unwrap_or_else(|| "—".to_owned())}</td>
            <td class="px-2 py-1" data-label="Note">
                {c.winning.then(|| view! { <span class="font-medium">"Decided the outcome"</span> })}
                {c.disagrees.then(|| view! { <span class="font-medium">"Disagrees with the outcome"</span> })}
            </td>
        </tr>
    }
}

/// Where the flow ran: the device, the CI run, the job and the runner, and the Maestro version.
#[component]
fn WhereItRan(
    /// The device; `None` when no event named one.
    device: Option<DeviceSessionRow>,
    /// The CI facts and links.
    ci: CiSection,
) -> impl IntoView {
    let device = match device {
        None => view! { <p class="text-muted">"No device was reported for this flow."</p> }.into_any(),
        Some(d) => view! {
            <p class="text-sm">
                <a class="underline" href=d.href>{d.serial}</a>
                <span class="ml-2"><PillView pill=d.state /></span>
                <span class="ml-2 text-muted">{d.profile.unwrap_or_else(|| "Device profile not reported".to_owned())}</span>
            </p>
        }
        .into_any(),
    };
    view! {
        <Card title="Where it ran">
            <div class="space-y-3">
                {device}
                <FactList facts=ci.facts />
                <ul class="flex gap-4 text-sm">
                    {ci
                        .links
                        .into_iter()
                        .map(|l| view! { <li><a class="underline" href=l.href>{l.label}</a></li> })
                        .collect_view()}
                </ul>
            </div>
        </Card>
    }
}
