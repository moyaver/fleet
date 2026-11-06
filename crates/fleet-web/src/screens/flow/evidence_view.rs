// SPDX-License-Identifier: AGPL-3.0-only
//! Screen D's evidence card: failure screenshots, the Maestro log with a jump to the failing
//! step, command timings, the HTML report and downloads (spec §16 criterion 8).
//!
//! Every file comes from the usercontent origin through a signed link (invariant I7): images are
//! `<img>` elements, text is fetched by the route and shown as text nodes, and the HTML report is
//! a plain link that opens on that origin, where it is sandboxed. Nothing from an artifact is ever
//! rendered as HTML.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs"
)]

use fleet_protocol::rest::FlowDetailView;
use fleet_ui::{
    components::display::{Card, Skeleton, StatusPill},
    logview::{model::anchored_line, view::LogViewer},
    screenshots::{ScreenshotViewer, view::ScreenshotViewerView},
    tone::Tone,
};
use leptos::prelude::*;
use leptos_router::hooks::use_location;

use crate::screens::{
    flow::{
        evidence::{
            CommandRow, Evidence, EvidenceFiles, EvidenceLink, Load, LogEvidence, LogSource,
            Refused, command_rows,
        },
        model::evidence_steps,
    },
    parts::{Fetched, Notice},
};

/// What the route fetched for the evidence card.
#[derive(Debug, Clone, Copy)]
pub struct EvidenceFeed {
    /// What there is to show, from the artifacts REST.
    pub evidence: Memo<Evidence>,
    /// The Maestro log's text.
    pub log: RwSignal<Load<String>>,
    /// `commands.json`'s text.
    pub commands: RwSignal<Load<String>>,
    /// The step the user picked in the step tree; the log jumps to it.
    pub selected_step: RwSignal<Option<String>>,
}

/// The evidence card of screen D.
#[component]
pub fn EvidenceCard(
    /// What the route fetched.
    feed: EvidenceFeed,
    /// The flow, for its name and steps.
    flow: Fetched<FlowDetailView>,
) -> impl IntoView {
    view! {
        <Card title="Evidence">
            <div class="space-y-4" data-testid="evidence">
                {move || match feed.evidence.get() {
                    Evidence::Loading => view! { <Skeleton lines=3 /> }.into_any(),
                    Evidence::Unavailable(why) => {
                        view! { <Notice tone=Tone::Warning text=format!("The evidence could not be loaded: {why}") /> }
                            .into_any()
                    }
                    Evidence::Nothing(text) => {
                        view! { <p class="text-muted" data-testid="evidence-empty">{text}</p> }.into_any()
                    }
                    Evidence::Unopenable(text) => {
                        view! { <p class="text-muted" data-testid="evidence-unopenable">{text}</p> }
                            .into_any()
                    }
                    Evidence::Files(files) => view! { <FilesView files=*files feed flow /> }.into_any(),
                }}
            </div>
        </Card>
    }
}

#[component]
fn FilesView(
    files: EvidenceFiles,
    feed: EvidenceFeed,
    flow: Fetched<FlowDetailView>,
) -> impl IntoView {
    let EvidenceFiles {
        screenshots,
        log,
        commands,
        report,
        downloads,
        refused,
    } = files;
    let has_commands = commands.is_some();
    let screenshots = if screenshots.is_empty() {
        view! {
            <p class="text-sm text-muted" data-testid="screenshots-empty">
                "No failure screenshot was uploaded for this flow."
            </p>
        }
        .into_any()
    } else {
        view! {
            <div data-testid="screenshots">
                <ScreenshotViewerView viewer=ScreenshotViewer::new(screenshots) />
            </div>
        }
        .into_any()
    };
    view! {
        {screenshots}
        {report.map(|r| view! { <ReportLink link=r /> })}
        {log.map(|source| view! { <LogSection source feed flow /> })}
        {has_commands.then(|| view! { <CommandsSection feed /> })}
        <Downloads links=downloads />
        <RefusedList refused />
    }
}

/// Maestro's HTML report. It opens on the usercontent origin, in a new tab, with no way back to
/// the dashboard's page (`noopener`): the report is sandboxed there and cannot run scripts.
#[component]
fn ReportLink(link: EvidenceLink) -> impl IntoView {
    view! {
        <p class="text-sm">
            <a
                class="font-medium underline"
                href=link.href
                target="_blank"
                rel="noopener noreferrer"
                data-testid="report-link"
            >
                "Open the Maestro HTML report"
            </a>
            <span class="ml-2 text-muted">
                {format!("{} · opens on the usercontent origin, sandboxed", link.size)}
            </span>
        </p>
    }
}

#[component]
fn LogSection(
    source: LogSource,
    feed: EvidenceFeed,
    flow: Fetched<FlowDetailView>,
) -> impl IntoView {
    let body = match source {
        LogSource::TooLarge(link) => view! {
            <p class="text-sm" data-testid="log-too-large">
                {format!("The log is {}: too large to show here. ", link.size)}
                <a class="underline" href=link.href>"Download it"</a>
            </p>
        }
        .into_any(),
        LogSource::Show(_) => view! { <LogBody feed flow /> }.into_any(),
    };
    view! {
        <section aria-labelledby="log-title" class="space-y-2">
            <h3 id="log-title" class="text-sm font-semibold">"Maestro log"</h3>
            {body}
        </section>
    }
}

#[component]
fn LogBody(feed: EvidenceFeed, flow: Fetched<FlowDetailView>) -> impl IntoView {
    move || {
        match feed.log.get() {
        Load::Loading => view! { <p class="text-sm text-muted">"Loading the log…"</p> }.into_any(),
        Load::Failed(why) => {
            view! { <Notice tone=Tone::Warning text=format!("The log could not be loaded: {why}") /> }
                .into_any()
        }
        Load::Ready(text) => {
            let Some(detail) = flow.data.get_untracked() else {
                return ().into_any();
            };
            let (steps, failed) = evidence_steps(&detail);
            let log = LogEvidence::build(&text, &detail.flow.name, &steps);
            let failing = failed.map(|f| f.id);
            let lines = log.model.lines().len();
            let location = use_location();
            let target = {
                let (log, failing) = (log.clone(), failing.clone());
                Signal::derive(move || {
                    // The user's pick wins; then a `#L123` in the address; until either, where the
                    // flow failed.
                    if let Some(id) = feed.selected_step.get() {
                        return log.line_of(&id);
                    }
                    anchored_line(&location.hash.get(), lines)
                        .or_else(|| failing.as_ref().and_then(|id| log.line_of(id)))
                })
            };
            let can_jump = failing.as_ref().is_some_and(|id| log.line_of(id).is_some());
            let jump = move |_| feed.selected_step.set(failing.clone());
            let note = if log.starts.is_empty() {
                Some("This log has no step markers for this flow, so steps cannot be jumped to.")
            } else {
                None
            };
            view! {
                <div class="space-y-2" data-testid="log-viewer">
                    <div class="flex items-center gap-3 text-sm">
                        {can_jump
                            .then(|| {
                                view! {
                                    <button
                                        type="button"
                                        class="underline"
                                        data-testid="log-jump"
                                        on:click=jump.clone()
                                    >
                                        "Jump to the failing step"
                                    </button>
                                }
                            })}
                        {note.map(|n| view! { <span class="text-muted">{n}</span> })}
                    </div>
                    <LogViewer log=log.model selected_line=target />
                </div>
            }
            .into_any()
        }
    }
    }
}

#[component]
fn CommandsSection(feed: EvidenceFeed) -> impl IntoView {
    view! {
        <section aria-labelledby="commands-title" class="space-y-2">
            <h3 id="commands-title" class="text-sm font-semibold">"Command timings"</h3>
            {move || match feed.commands.get() {
                Load::Loading => {
                    view! { <p class="text-sm text-muted">"Loading the command timings…"</p> }.into_any()
                }
                Load::Failed(why) => {
                    view! { <Notice tone=Tone::Warning text=format!("The command timings could not be loaded: {why}") /> }
                        .into_any()
                }
                Load::Ready(text) => match command_rows(&text) {
                    Ok(rows) => view! { <CommandTable rows /> }.into_any(),
                    Err(why) => view! { <Notice tone=Tone::Warning text=why /> }.into_any(),
                }
            }}
        </section>
    }
}

#[component]
fn CommandTable(rows: Vec<CommandRow>) -> impl IntoView {
    view! {
        <table class="w-full text-left text-sm" data-testid="command-timings">
            <caption class="sr-only">"Every command Maestro ran for this flow, with how long it took"</caption>
            <thead class="border-b border-border text-muted">
                <tr>
                    {["#", "Command", "Result", "Duration"]
                        .map(|h| view! { <th scope="col" class="px-2 py-1 font-medium">{h}</th> })}
                </tr>
            </thead>
            <tbody>
                {rows
                    .into_iter()
                    .map(|r| {
                        view! {
                            <tr class="border-b border-border" data-testid="command-row" data-status=r.status>
                                <td class="px-2 py-1 tabular-nums">{r.sequence + 1}</td>
                                <td class="px-2 py-1">
                                    <span style:margin-left=format!("{}rem", r.depth)>{r.label}</span>
                                </td>
                                <td class="px-2 py-1"><StatusPill tone=r.tone label=r.status /></td>
                                <td class="px-2 py-1 tabular-nums">
                                    {r.duration.unwrap_or_else(|| "—".to_owned())}
                                </td>
                            </tr>
                        }
                    })
                    .collect_view()}
            </tbody>
        </table>
    }
}

#[component]
fn Downloads(links: Vec<EvidenceLink>) -> impl IntoView {
    (!links.is_empty()).then(|| {
        view! {
            <section aria-labelledby="downloads-title" class="space-y-1">
                <h3 id="downloads-title" class="text-sm font-semibold">"Files"</h3>
                <ul class="space-y-1 text-sm">
                    {links
                        .into_iter()
                        .map(|l| {
                            view! {
                                <li data-testid="evidence-download">
                                    <a class="underline" href=l.href rel="noopener noreferrer">{l.name}</a>
                                    <span class="ml-2 text-muted">{format!("{} · {}", l.kind, l.size)}</span>
                                </li>
                            }
                        })
                        .collect_view()}
                </ul>
            </section>
        }
    })
}

#[component]
fn RefusedList(refused: Vec<Refused>) -> impl IntoView {
    (!refused.is_empty()).then(|| {
        view! {
            <section aria-labelledby="refused-title" class="space-y-1">
                <h3 id="refused-title" class="text-sm font-semibold">"Refused uploads"</h3>
                <ul class="space-y-1 text-sm">
                    {refused
                        .into_iter()
                        .map(|r| {
                            view! {
                                <li data-testid="evidence-refused">
                                    {format!("{} ({}): refused, {}", r.name, r.kind, r.reason)}
                                </li>
                            }
                        })
                        .collect_view()}
                </ul>
            </section>
        }
    })
}
