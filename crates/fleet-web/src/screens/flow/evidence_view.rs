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
    screenshots::view::ScreenshotViewerView,
    tone::Tone,
};
use leptos::prelude::*;
use leptos_router::hooks::use_location;

use crate::screens::{
    flow::{
        evidence::{
            CommandRow, Evidence, EvidenceFiles, EvidenceLink, Hierarchy, Load, LogEvidence,
            LogMount, LogSource, Refused, command_rows,
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

impl EvidenceFeed {
    /// What `f` makes of the flow's files, when its evidence is files (else `None`). Read inside a
    /// closure, it runs again when the evidence does, which includes every renewal of the links.
    fn files_with<T>(&self, f: impl FnOnce(&EvidenceFiles) -> T) -> Option<T> {
        self.evidence.with(|e| match e {
            Evidence::Files(files) => Some(f(files)),
            _ => None,
        })
    }
}

/// The evidence card of screen D.
///
/// A signed-link renewal rebuilds the evidence, but the card mounts its parts only when what it
/// shows changes (see [`EvidenceFiles::mounts`]). Each part reads its links from the feed as they
/// are renewed, so a screenshot's zoom and a log's search survive a renewal.
#[component]
pub fn EvidenceCard(
    /// What the route fetched.
    feed: EvidenceFeed,
    /// The flow, for its name and steps.
    flow: Fetched<FlowDetailView>,
) -> impl IntoView {
    let is_files = Memo::new(move |_| feed.evidence.with(|e| matches!(e, Evidence::Files(_))));
    view! {
        <Card title="Evidence">
            <div class="space-y-4" data-testid="evidence">
                {move || {
                    if is_files.get() {
                        view! { <FilesView feed flow /> }.into_any()
                    } else {
                        match feed.evidence.get() {
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
                            // `is_files` is true for this one.
                            Evidence::Files(_) => ().into_any(),
                        }
                    }
                }}
            </div>
        </Card>
    }
}

/// The files of a flow. Rebuilt only when [`EvidenceFiles::mounts`] changes; the links inside are
/// read from the feed, so a renewal updates them in place.
#[component]
fn FilesView(feed: EvidenceFeed, flow: Fetched<FlowDetailView>) -> impl IntoView {
    let mounts = Memo::new(move |_| feed.files_with(EvidenceFiles::mounts));
    move || {
        mounts.get().map(|m| {
            view! {
                {if m.screenshots {
                    let shots = Signal::derive(move || {
                        feed.files_with(|f| f.screenshots.clone()).unwrap_or_default()
                    });
                    view! {
                        <div data-testid="screenshots">
                            <ScreenshotViewerView shots />
                        </div>
                    }
                    .into_any()
                } else {
                    view! {
                        <p class="text-sm text-muted" data-testid="screenshots-empty">
                            "No failure screenshot was uploaded for this flow."
                        </p>
                    }
                    .into_any()
                }}
                {move || feed.files_with(|f| f.hierarchies.clone()).filter(|h| !h.is_empty()).map(|hierarchies| view! { <Hierarchies hierarchies /> })}
                {move || feed.files_with(|f| f.report.clone()).flatten().map(|r| view! { <ReportLink link=r /> })}
                {match m.log {
                    LogMount::None => ().into_any(),
                    LogMount::Viewer => view! { <LogSection feed flow /> }.into_any(),
                    LogMount::Download => view! { <TooLargeLog feed /> }.into_any(),
                }}
                {m.commands.then(|| view! { <CommandsSection feed /> })}
                {move || feed.files_with(|f| f.downloads.clone()).map(|links| view! { <Downloads links /> })}
                {move || feed.files_with(|f| f.refused.clone()).map(|refused| view! { <RefusedList refused /> })}
            }
        })
    }
}

/// The UI hierarchies of the steps that failed, each named for its step. The failing step's comes
/// first: it is the one the viewer came to diagnose.
#[component]
fn Hierarchies(hierarchies: Vec<Hierarchy>) -> impl IntoView {
    let mut hierarchies = hierarchies;
    hierarchies.sort_by_key(|h| !h.failed);
    view! {
        <section aria-labelledby="hierarchy-title" class="space-y-1">
            <h3 id="hierarchy-title" class="text-sm font-semibold">"UI hierarchy"</h3>
            <ul class="space-y-1 text-sm">
                {hierarchies
                    .into_iter()
                    .map(|h| {
                        let who = if h.failed { "Failing step" } else { "Step" };
                        let step = h.step_label.unwrap_or_else(|| format!("step {}", h.step));
                        view! {
                            <li data-testid="evidence-hierarchy">
                                <span class="text-muted">{format!("{who}: {step}")}</span>
                                <a class="ml-2 underline" href=h.link.href rel="noopener noreferrer">{h.link.name}</a>
                                <span class="ml-2 text-muted">{h.link.size}</span>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        </section>
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

/// The Maestro log, in the viewer.
#[component]
fn LogSection(feed: EvidenceFeed, flow: Fetched<FlowDetailView>) -> impl IntoView {
    view! {
        <section aria-labelledby="log-title" class="space-y-2">
            <h3 id="log-title" class="text-sm font-semibold">"Maestro log"</h3>
            <LogBody feed flow />
        </section>
    }
}

/// The Maestro log when it is too big to hold in the browser: a download link, read from the
/// feed so a renewal updates it.
#[component]
fn TooLargeLog(feed: EvidenceFeed) -> impl IntoView {
    move || {
        feed.files_with(|f| match &f.log {
            Some(LogSource::TooLarge(link)) => Some(link.clone()),
            _ => None,
        })
        .flatten()
        .map(|link| {
            view! {
                <section aria-labelledby="log-title" class="space-y-2">
                    <h3 id="log-title" class="text-sm font-semibold">"Maestro log"</h3>
                    <p class="text-sm" data-testid="log-too-large">
                        {format!("The log is {}: too large to show here. ", link.size)}
                        <a class="underline" href=link.href>"Download it"</a>
                    </p>
                </section>
            }
        })
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
