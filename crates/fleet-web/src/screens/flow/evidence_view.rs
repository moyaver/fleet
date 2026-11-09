// SPDX-License-Identifier: AGPL-3.0-only
//! Screen D's evidence: the failure screenshots and UI hierarchies (the evidence pane), the Maestro
//! log, the device logcat and `commands.json` (the logs pane), the HTML report (in the header) and the
//! downloads (the files card).
//!
//! Every file comes from the usercontent origin through a signed link (invariant I7): images are
//! `<img>` elements, text is fetched by the route and shown as text nodes, and the HTML report is
//! a plain link that opens on that origin, where it is sandboxed. Nothing from an artifact is ever
//! rendered as HTML.
//!
//! The panes mount once, when the flow first arrives, and each part reads its links from the feed as
//! they are renewed: a zoom, a log search or a selected tab survives a renewal and a live update.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs"
)]

use fleet_protocol::rest::FlowDetailView;
use fleet_ui::{
    components::display::{Card, Skeleton, StatusPill, TableBox},
    logview::{
        model::{LogModel, anchored_line},
        view::LogViewer,
    },
    screenshots::view::ScreenshotViewerView,
    tone::Tone,
};
use leptos::prelude::*;
use leptos_router::hooks::use_location;

use crate::screens::{
    flow::{
        evidence::{
            CommandRow, Evidence, EvidenceFiles, EvidenceLink, Hierarchy, Load, LogEvidence,
            LogMount, LogSource, MediaKind, OwnMedia, Refused, command_rows,
        },
        model::evidence_steps,
    },
    parts::{Fetched, Notice},
};

/// What the route fetched for the evidence.
#[derive(Debug, Clone, Copy)]
pub struct EvidenceFeed {
    /// What there is to show, from the artifacts REST.
    pub evidence: Memo<Evidence>,
    /// The Maestro log's text.
    pub log: RwSignal<Load<String>>,
    /// The device logcat's text.
    pub logcat: RwSignal<Load<String>>,
    /// `commands.json`'s text.
    pub commands: RwSignal<Load<String>>,
    /// The step the user picked in the step tree; the log jumps to it.
    pub selected_step: RwSignal<Option<String>>,
}

impl EvidenceFeed {
    /// What `f` makes of the flow's files, when its evidence is files (else `None`). Read inside a
    /// closure, it runs again when the evidence does, which includes every renewal of the links.
    pub(crate) fn files_with<T>(&self, f: impl FnOnce(&EvidenceFiles) -> T) -> Option<T> {
        self.evidence.with(|e| match e {
            Evidence::Files(files) => Some(f(files)),
            _ => None,
        })
    }
}

/// The flow's device logcat excerpt, as a link once the evidence has loaded, or a plain statement
/// when none was uploaded. Read from the feed, so a renewed link updates it in place.
#[component]
pub fn LogcatLink(
    /// What the route fetched.
    feed: EvidenceFeed,
) -> impl IntoView {
    move || {
        // `None` while the evidence loads or cannot be opened: the evidence pane says which.
        let href = feed.evidence.with(|e| match e {
            Evidence::Files(files) => Some(files.logcat.as_ref().map(|l| match l {
                LogSource::Show(text) => text.url.clone(),
                LogSource::TooLarge(link) => link.href.clone(),
            })),
            Evidence::Nothing(_) => Some(None),
            _ => None,
        });
        match href {
            None => ().into_any(),
            Some(None) => {
                view! { <span class="text-muted">"No logcat excerpt was uploaded."</span> }
                    .into_any()
            }
            Some(Some(href)) => view! {
                <a class="underline" href=href rel="noopener noreferrer" data-testid="crash-logcat">
                    "Device logcat excerpt"
                </a>
            }
            .into_any(),
        }
    }
}

/// One tab of a set: the signal holds the chosen one.
fn tab<T>(current: RwSignal<T>, value: T, label: &'static str) -> impl IntoView
where
    T: Copy + PartialEq + Send + Sync + 'static,
{
    view! {
        <button
            type="button"
            role="tab"
            class="border-b-2 border-transparent pb-1 aria-selected:border-fg aria-selected:font-medium"
            aria-selected=move || (current.get() == value).to_string()
            on:click=move |_| current.set(value)
        >
            {label}
        </button>
    }
}

/// The screenshot and the UI hierarchy of the failure, in tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvidenceTab {
    Screenshot,
    Hierarchy,
}

/// The evidence pane: the failure screenshot, or the state of the evidence, and the UI hierarchies.
#[component]
pub fn EvidencePane(
    /// What the route fetched.
    feed: EvidenceFeed,
) -> impl IntoView {
    let tab_now = RwSignal::new(EvidenceTab::Screenshot);
    let mounts = Memo::new(move |_| feed.files_with(EvidenceFiles::mounts));
    view! {
        <section class="card flow-ev" aria-labelledby="ev-h">
            <div class="card-h">
                <h2 id="ev-h">"Evidence"</h2>
                <div role="tablist" aria-label="Evidence" class="row gap-4 text-sm">
                    {tab(tab_now, EvidenceTab::Screenshot, "Screenshot")}
                    {tab(tab_now, EvidenceTab::Hierarchy, "UI hierarchy")}
                </div>
            </div>
            <div role="tabpanel" aria-label="Screenshot" class="card-b space-y-4" hidden=move || tab_now.get() != EvidenceTab::Screenshot>
                {move || match feed.evidence.get() {
                    Evidence::Files(_) => None,
                    other => Some(evidence_state(other)),
                }}
                {move || {
                    mounts
                        .get()
                        .map(|m| {
                            if m.screenshots {
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
                            }
                        })
                }}
            </div>
            <div role="tabpanel" aria-label="UI hierarchy" class="card-b space-y-4" hidden=move || tab_now.get() != EvidenceTab::Hierarchy>
                {move || {
                    feed.files_with(|f| f.hierarchies.clone())
                        .map(|hierarchies| {
                            if hierarchies.is_empty() {
                                view! { <p class="text-sm text-muted">"No UI hierarchy was uploaded for this flow."</p> }
                                    .into_any()
                            } else {
                                view! { <Hierarchies hierarchies /> }.into_any()
                            }
                        })
                }}
            </div>
            <OwnMediaList feed />
        </section>
    }
}

/// Maestro's own screenshots and recordings of the flow, each on the step that made it (issue #133):
/// an image inline, a recording in the page's own player. Both come from the usercontent origin
/// through their signed links, and carry no mark: they are evidence, as the failure screenshots are.
#[component]
fn OwnMediaList(
    /// What the route fetched.
    feed: EvidenceFeed,
) -> impl IntoView {
    let media = move || feed.files_with(|f| f.own_media.clone()).unwrap_or_default();
    move || {
        let items = media();
        (!items.is_empty()).then(|| {
            view! {
                <section class="card-b space-y-4 border-t border-line" aria-labelledby="own-media-h" data-testid="own-media">
                    <h3 id="own-media-h" class="text-sm font-medium">"Maestro's own screenshots and recordings"</h3>
                    <ul class="space-y-6">
                        <For
                            each=move || media()
                            key=|m| m.id
                            children=move |m| view! { <OwnMediaItem feed item=m /> }
                        />
                    </ul>
                </section>
            }
        })
    }
}

/// One of Maestro's own files. Its link is read from the feed, so a renewal changes the `src` of the
/// element it mounted, and nothing else is rebuilt.
#[component]
fn OwnMediaItem(
    /// What the route fetched.
    feed: EvidenceFeed,
    /// The file, as the list had it when it appeared.
    item: OwnMedia,
) -> impl IntoView {
    let url = move || {
        feed.files_with(|f| {
            f.own_media
                .iter()
                .find(|m| m.id == item.id)
                .map(|m| m.url.clone())
        })
        .flatten()
        .unwrap_or_default()
    };
    // The step's description, as the hierarchies name a step; without one, its id as they do.
    let caption = match (&item.step_label, &item.step) {
        (Some(label), _) => label.clone(),
        (None, Some(step)) => format!("step {step}"),
        (None, None) => "Not tied to a step".to_owned(),
    };
    let meta = format!("{} · {}", item.name, item.size);
    let player = match item.kind {
        MediaKind::Screenshot => view! {
            <img
                class="max-w-full h-auto border border-border"
                src=url
                alt=item.name.clone()
                loading="lazy"
                data-testid="own-media-image"
            />
        }
        .into_any(),
        // No script: the browser's own controls play the file, which the usercontent origin serves
        // inline with its sandbox headers (invariant I7).
        MediaKind::Recording => view! {
            <video
                class="max-w-full h-auto border border-border"
                controls=true
                preload="metadata"
                src=url
                aria-label=item.name.clone()
                data-testid="own-media-video"
            >
                <a class="underline" href=url>{item.name.clone()}</a>
            </video>
        }
        .into_any(),
    };
    view! {
        <li class="space-y-2" data-testid="own-media-item">
            <p class="text-sm">
                <span class="font-medium">{caption}</span>
                <span class="ml-2 text-muted">{meta}</span>
            </p>
            {player}
        </li>
    }
}

/// What the evidence says when there are no files to show.
fn evidence_state(e: Evidence) -> AnyView {
    match e {
        Evidence::Loading => view! { <Skeleton lines=3 /> }.into_any(),
        Evidence::Unavailable(why) => {
            view! { <Notice tone=Tone::Warning text=format!("The evidence could not be loaded: {why}") /> }
                .into_any()
        }
        Evidence::Nothing(text) => {
            view! { <p class="text-muted" data-testid="evidence-empty">{text}</p> }.into_any()
        }
        Evidence::Unopenable(text) => {
            view! { <p class="text-muted" data-testid="evidence-unopenable">{text}</p> }.into_any()
        }
        // The caller shows the files instead.
        Evidence::Files(_) => ().into_any(),
    }
}

/// The UI hierarchies of the steps that failed, each named for its step. The failing step's comes
/// first: it is the one the viewer came to diagnose.
#[component]
fn Hierarchies(hierarchies: Vec<Hierarchy>) -> impl IntoView {
    let mut hierarchies = hierarchies;
    hierarchies.sort_by_key(|h| !h.failed);
    view! {
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
    }
}

/// Maestro's HTML report. It opens on the usercontent origin, in a new tab, with no way back to
/// the dashboard's page (`noopener`): the report is sandboxed there and cannot run scripts.
#[component]
pub fn ReportLink(
    /// The report's link.
    link: EvidenceLink,
) -> impl IntoView {
    view! {
        <span class="text-sm">
            <a
                class="font-medium underline"
                href=link.href
                target="_blank"
                rel="noopener noreferrer"
                data-testid="report-link"
            >
                "Maestro report ↗"
            </a>
            <span class="ml-2 text-muted">
                {format!("{} · opens on the usercontent origin, sandboxed", link.size)}
            </span>
        </span>
    }
}

/// The log tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogTab {
    Maestro,
    Logcat,
    Timings,
}

/// The logs pane: the Maestro log with its jump to the failing step, the device logcat and the
/// command timings, in tabs. A picked step brings the Maestro log forward.
#[component]
pub fn LogsPane(
    /// What the route fetched.
    feed: EvidenceFeed,
    /// The flow, for its steps.
    flow: Fetched<FlowDetailView>,
) -> impl IntoView {
    let tab_now = RwSignal::new(LogTab::Maestro);
    Effect::new(move |_| {
        if feed.selected_step.with(Option::is_some) {
            tab_now.set(LogTab::Maestro);
        }
    });
    let mounts = Memo::new(move |_| feed.files_with(EvidenceFiles::mounts));
    view! {
        <section class="card flow-logs" aria-labelledby="lg-h">
            <div class="card-h">
                <h2 id="lg-h">"Logs"</h2>
                <div role="tablist" aria-label="Log" class="row gap-4 text-sm">
                    {tab(tab_now, LogTab::Maestro, "Maestro log")}
                    {tab(tab_now, LogTab::Logcat, "Logcat")}
                    {tab(tab_now, LogTab::Timings, "Command timings")}
                </div>
            </div>
            <div role="tabpanel" aria-label="Maestro log" class="card-b" hidden=move || tab_now.get() != LogTab::Maestro>
                {move || match mounts.get().map(|m| m.log) {
                    Some(LogMount::Viewer) => view! { <LogBody feed flow /> }.into_any(),
                    Some(LogMount::Download) => view! { <TooLargeLog feed /> }.into_any(),
                    Some(LogMount::None) => view! { <p class="text-sm text-muted">"No Maestro log was uploaded for this flow."</p> }.into_any(),
                    None => view! { <p class="text-sm text-muted">{no_files_text(feed)}</p> }.into_any(),
                }}
            </div>
            <div role="tabpanel" aria-label="Logcat" class="card-b" hidden=move || tab_now.get() != LogTab::Logcat>
                {move || match mounts.get().map(|m| m.logcat) {
                    Some(LogMount::Viewer) => view! { <LogcatBody feed /> }.into_any(),
                    Some(LogMount::Download) => view! { <TooLargeLogcat feed /> }.into_any(),
                    Some(LogMount::None) => view! { <p class="text-sm text-muted">"No device logcat was uploaded for this flow."</p> }.into_any(),
                    None => view! { <p class="text-sm text-muted">{no_files_text(feed)}</p> }.into_any(),
                }}
            </div>
            <div role="tabpanel" aria-label="Command timings" class="card-b" hidden=move || tab_now.get() != LogTab::Timings>
                {move || match mounts.get().map(|m| m.commands) {
                    Some(true) => view! { <CommandsSection feed /> }.into_any(),
                    Some(false) => view! { <p class="text-sm text-muted">"No command timings were uploaded for this flow."</p> }.into_any(),
                    None => view! { <p class="text-sm text-muted">{no_files_text(feed)}</p> }.into_any(),
                }}
            </div>
        </section>
    }
}

/// What a log tab says while there are no files to read: loading, or nothing was uploaded.
fn no_files_text(feed: EvidenceFeed) -> &'static str {
    if feed.evidence.with(|e| matches!(e, Evidence::Loading)) {
        "Loading the evidence…"
    } else {
        "No evidence was uploaded for this flow."
    }
}

/// The Maestro log when it is too big to hold in the browser: a download link, read from the feed
/// so a renewal updates it.
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
                <p class="text-sm" data-testid="log-too-large">
                    {format!("The log is {}: too large to show here. ", link.size)}
                    <a class="underline" href=link.href>"Download it"</a>
                </p>
            }
        })
    }
}

/// The device logcat when it is too big to hold in the browser: a download link.
#[component]
fn TooLargeLogcat(feed: EvidenceFeed) -> impl IntoView {
    move || {
        feed.files_with(|f| match &f.logcat {
            Some(LogSource::TooLarge(link)) => Some(link.clone()),
            _ => None,
        })
        .flatten()
        .map(|link| {
            view! {
                <p class="text-sm">
                    {format!("The logcat is {}: too large to show here. ", link.size)}
                    <a class="underline" href=link.href>"Download it"</a>
                </p>
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

/// The device logcat, in the log viewer. It has no steps, so there is nothing to jump to.
#[component]
fn LogcatBody(feed: EvidenceFeed) -> impl IntoView {
    move || {
        match feed.logcat.get() {
        Load::Loading => view! { <p class="text-sm text-muted">"Loading the logcat…"</p> }.into_any(),
        Load::Failed(why) => {
            view! { <Notice tone=Tone::Warning text=format!("The logcat could not be loaded: {why}") /> }
                .into_any()
        }
        Load::Ready(text) => {
            let model = LogModel::parse(&text);
            view! { <LogViewer log=model label="Logcat" /> }.into_any()
        }
    }
    }
}

#[component]
fn CommandsSection(feed: EvidenceFeed) -> impl IntoView {
    move || {
        match feed.commands.get() {
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
        },
    }
    }
}

#[component]
fn CommandTable(rows: Vec<CommandRow>) -> impl IntoView {
    view! {
        <TableBox label="Commands">
            <table class="tbl tbl-cards w-full text-left text-sm" data-testid="command-timings">
                <caption class="sr-only">"Every command Maestro ran for this flow, with how long it took"</caption>
                <thead class="border-b border-line text-muted">
                    <tr>
                        {["#", "Command", "Result", "Duration"]
                            .map(|h| view! { <th scope="col" class="px-2 py-1 font-medium">{h}</th> })}
                    </tr>
                </thead>
                <tbody>
                    {rows
                        .into_iter()
                        .map(|r| {
                            let failed = r.status == "Failed";
                            view! {
                                <tr
                                    class=if failed { "border-b border-line bg-band" } else { "border-b border-line" }
                                    data-testid="command-row"
                                    data-status=r.status
                                >
                                    <td class="px-2 py-1 tabular-nums" data-label="#">{r.sequence + 1}</td>
                                    <td class="px-2 py-1" data-label="Command">
                                        <span style:margin-left=format!("{}rem", r.depth)>{r.label}</span>
                                    </td>
                                    <td class="px-2 py-1" data-label="Result"><StatusPill mark=r.mark label=r.status /></td>
                                    <td class="px-2 py-1 tabular-nums" data-label="Duration">
                                        {r.duration.unwrap_or_else(|| "—".to_owned())}
                                    </td>
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>
        </TableBox>
    }
}

/// The flow's files to save, and the uploads the server refused. A flow with neither has no card.
#[component]
pub fn FilesCard(
    /// What the route fetched.
    feed: EvidenceFeed,
) -> impl IntoView {
    move || {
        feed.files_with(|f| !f.downloads.is_empty() || !f.refused.is_empty())
            .unwrap_or(false)
            .then(|| {
                view! {
                    <Card title="Files">
                        <div class="space-y-4">
                            {move || feed.files_with(|f| f.downloads.clone()).map(|links| view! { <Downloads links /> })}
                            {move || feed.files_with(|f| f.refused.clone()).map(|refused| view! { <RefusedList refused /> })}
                        </div>
                    </Card>
                }
            })
    }
}

#[component]
fn Downloads(links: Vec<EvidenceLink>) -> impl IntoView {
    (!links.is_empty()).then(|| {
        view! {
            <ul class="grid gap-x-8 gap-y-2 text-sm [grid-template-columns:repeat(auto-fill,minmax(220px,1fr))]">
                {links
                    .into_iter()
                    .map(|l| {
                        view! {
                            <li data-testid="evidence-download">
                                <a class="underline" href=l.href rel="noopener noreferrer">{l.name}</a>
                                <div class="text-xs text-muted">{format!("{} · {}", l.kind, l.size)}</div>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
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
