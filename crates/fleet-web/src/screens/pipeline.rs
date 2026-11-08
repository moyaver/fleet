// SPDX-License-Identifier: AGPL-3.0-only
//! Screen B: one pipeline attempt (spec §8, the redesign's Pipeline page): the verdict derived
//! from the flows, readouts that filter them, failures grouped by message, flows grouped by job
//! and device with the plan's shards and the timeline, and the activity while the run is open.
//!
//! The view-models are pure: [`model`] (the tree, the header, the verdict, failures), [`activity`]
//! (the live event stream derived from what changed), [`plans`] (the shards' planned against actual
//! times) and [`timeline`] (the REST timeline as the swimlane's data). The page follows one attempt
//! over a live socket: the snapshot and every delta go through [`LiveModel`], whose per-entity
//! signals keep a changing flow from re-rendering the whole list.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs and the components return views"
)]

pub mod activity;
pub mod model;
pub mod plans;
pub mod timeline;

#[cfg(test)]
mod fixtures;

use std::{collections::BTreeSet, time::Duration};

use fleet_protocol::live::{FlowView, LiveFrame, PlanView};
use fleet_ui::{
    charts::format::{format_duration, format_timestamp},
    components::{
        display::{
            EmptyState, OriginBadge, ProvenanceBadge, ReadoutStrip, RowProvenance, Skeleton,
            StatusPill, TableBox,
        },
        live::{Announcements, LiveState},
    },
    provenance::Provenance,
    timeline::{model::TimelineData, view::SwimlaneTimeline},
    tone::Tone,
};
use leptos::prelude::*;
use leptos_router::{components::A, hooks::use_params_map};
use uuid::Uuid;

use self::{
    model::{
        FailureRow, LiveInputs, bucket, failure_key, file_warning_notes, live_view, message_groups,
        row_mark,
    },
    plans::{PlanComparison, ShardRow, show, signed},
    timeline::timeline_data,
};
use crate::{
    announce::{self, Fact},
    api,
    app::{ErrorNotice, LiveBar, LiveSlot},
    clock::{now_ms, utc_offset_minutes},
    live::{
        model::LiveModel,
        socket::{Event, Subscription, connect},
    },
    paths::{device_href, flow_href},
    progress::Progress,
    remote::remote,
    screens::parts::Notice,
    status::{flow_provenance, flow_state_pill, job_pill, liveness_pill, source_provenance},
};

/// Screen B at `/p/:project/pipelines/:id`.
#[component]
pub fn PipelineDetail() -> impl IntoView {
    let params = use_params_map();
    move || {
        let (project, attempt) = params.with(|p| {
            (
                p.get("project"),
                p.get("id").and_then(|id| id.parse::<Uuid>().ok()),
            )
        });
        match (project, attempt) {
            // Rebuilt when the address changes: the old page's socket closes with its owner.
            (Some(project), Some(attempt)) => view! { <PipelineBody project attempt /> }.into_any(),
            _ => view! {
                <EmptyState
                    title="Not a pipeline address"
                    description="The address should end in a pipeline's id."
                />
            }
            .into_any(),
        }
    }
}

/// The flow states the readouts count, in the order they are shown. A state this build does not
/// know counts under Unknown.
const READOUTS: [&str; 7] = [
    "passed",
    "failed",
    "running",
    "interrupted",
    "pending",
    "skipped",
    "unknown",
];

/// How many activity entries the page shows: the newest.
const ACTIVITY_SHOWN: usize = 5;

/// A flow of one device with its number in run order, from 1.
type Numbered = (usize, Uuid);

#[component]
fn PipelineBody(project: String, attempt: Uuid) -> impl IntoView {
    let model = LiveModel::new(attempt);
    // Pause holds the frames as they arrive, unapplied; showing them applies them in order.
    let paused = RwSignal::new(false);
    let held = RwSignal::new(Vec::<LiveFrame>::new());
    let last_update = RwSignal::new(now_ms());
    let live = {
        let project = project.clone();
        connect(
            Subscription {
                project: project.clone(),
                attempt: Some(attempt),
            },
            move |event| match event {
                Event::Frames(frames) => {
                    last_update.set(now_ms());
                    if paused.get_untracked() {
                        held.update(|h| h.extend(frames));
                    } else {
                        model.apply(&project, frames);
                    }
                }
                Event::Lost => {
                    held.set(Vec::new());
                    model.connection_lost();
                }
            },
        )
    };
    let timeline = {
        let project = project.clone();
        remote(1000, move || {
            let project = project.clone();
            async move { api::timeline(&project, attempt).await }
        })
    };
    // The plans' actual shard times are not in the deltas: the plan table refetches them only
    // when a batch may have changed them (see `LiveModel::apply`).
    let plans = {
        let project = project.clone();
        remote(1000, move || {
            let project = project.clone();
            async move { api::plans(&project, attempt).await }
        })
    };
    // The timeline follows the live version.
    {
        let timeline = timeline.clone();
        Effect::new(move |_| {
            if model.version.get() > 0 {
                timeline.request();
            }
        });
    }
    {
        let plans = plans.clone();
        Effect::new(move |_| {
            if model.plan_refreshes.get() > 0 {
                plans.request();
            }
        });
    }
    let now = RwSignal::new(now_ms());
    if let Ok(handle) = set_interval_with_handle(move || now.set(now_ms()), Duration::from_secs(1))
    {
        on_cleanup(move || handle.clear());
    }
    // The top bar's live indicator follows this page while it is shown.
    let bar = LiveBar {
        state: RwSignal::new(LiveState::Connecting),
        detail: RwSignal::new(String::new()),
        action: RwSignal::new(None),
        on_action: {
            let project = project.clone();
            Callback::new(move |()| {
                if paused.get_untracked() {
                    paused.set(false);
                    let frames = held.get_untracked();
                    held.set(Vec::new());
                    if !frames.is_empty() {
                        model.apply(&project, frames);
                    }
                } else {
                    paused.set(true);
                }
            })
        },
    };
    Effect::new(move |_| {
        let at = now.get();
        let settled = model.header.with(|h| h.as_ref().is_some_and(|h| h.settled));
        let view = live_view(LiveInputs {
            connection: live.connection.get(),
            paused: paused.get(),
            held: held.with(Vec::len),
            settled,
            now: at,
            last_update: last_update.get(),
            utc_offset_minutes: utc_offset_minutes(),
        });
        bar.state.set(view.state);
        bar.detail.set(view.detail);
        bar.action.set(view.action);
    });
    let slot = expect_context::<LiveSlot>().0;
    slot.set(Some(bar));
    // Clears the slot only if it still holds this page's bar: a page replaced by another attempt
    // may be cleaned up after its successor has set its own.
    let mine = bar.state;
    on_cleanup(move || {
        slot.update(|current| {
            if current.is_some_and(|b| b.state == mine) {
                *current = None;
            }
        });
    });
    let timeline_view = timeline.data;
    let data = Memo::new(move |_| match timeline_view.get() {
        Some(view) => timeline_data(
            &view,
            |flow| model.flows.cell(flow).map(|c| c.get().agent_run_id),
            |run| model.runs.cell(run).and_then(|c| c.get().liveness),
        ),
        None => TimelineData::new(Vec::new(), Vec::new()),
    });
    let data = Signal::derive(move || data.get());
    let project = StoredValue::new(project);
    let settled =
        Signal::derive(move || model.header.with(|h| h.as_ref().is_some_and(|h| h.settled)));
    let filter: RwSignal<Option<&'static str>> = RwSignal::new(None);
    // The page's one status region (issue #149): the verdict when it changes, and each flow that
    // fails or is interrupted. Cells update silently.
    let announced = announce::region(Signal::derive(move || {
        model.header.with(|h| {
            h.as_ref().map(|h| {
                let verdict = Fact::new(
                    format!("verdict:{}", h.result.label),
                    format!("{}: {}.", h.title, h.result.label),
                );
                let flows = model.failures.with(|rows| {
                    rows.iter()
                        .map(|r| {
                            Fact::new(
                                format!("flow:{}:{}", r.flow_id, r.state.label),
                                format!("Flow {}: {}.", r.name, r.state.label),
                            )
                        })
                        .collect::<Vec<_>>()
                });
                std::iter::once(verdict).chain(flows).collect()
            })
        })
    }));
    let chosen = Memo::new(move |_| {
        filter
            .get()
            .filter(|b| count_in(model.progress.get(), b) > 0)
    });
    view! {
        <div class="stack" data-testid="pipeline-page">
            <Announcements text=announced />
            <ErrorNotice error=timeline.error />
            <ErrorNotice error=plans.error />
            {move || model.error.get().map(|e| view! {
                <p role="alert" data-testid="error" class="banner">{e}</p>
            })}
            <Show when=move || model.awaiting_snapshot.get()>
                <p data-testid="resync" class="note">
                    "Reconnecting to the live feed. This is the last known state; it is updating."
                </p>
            </Show>
            <Show
                when=move || model.header.with(Option::is_some)
                fallback=|| view! { <Skeleton lines=6 /> }
            >
                <PageHeader model=model now=now />
                <Progressline model=model settled=settled />
                <Readouts model=model filter=filter chosen=chosen />
                <div class=move || if settled.get() { "stack" } else { "grid gap-9 min-[720px]:grid-cols-[minmax(0,2.3fr)_minmax(0,1fr)]" }>
                    <div class="stack min-w-0">
                        <Failures model=model project=project settled=settled />
                        <Flows
                            model=model
                            project=project
                            settled=settled
                            chosen=chosen
                            plans=plans.data
                            timeline=data
                            now=now
                        />
                    </div>
                    <Show when=move || !settled.get()>
                        <aside class="min-w-0" aria-label="Activity">
                            <Activity model=model project=project />
                        </aside>
                    </Show>
                </div>
            </Show>
        </div>
    }
}

/// The count a readout shows for one flow state of the progress.
fn count_in(progress: Option<Progress>, state: &str) -> u64 {
    progress.map_or(0, |p| match state {
        "passed" => p.passed,
        "failed" => p.failed,
        "running" => p.running,
        "interrupted" => p.interrupted,
        "pending" => p.pending,
        "skipped" => p.skipped,
        _ => p.unknown,
    })
}

/// Whether a flow state has any flows, so it has a readout.
fn has_flows(progress: Option<Progress>, state: &str) -> bool {
    count_in(progress, state) > 0
}

/// The test id of a readout's count.
fn count_testid(state: &str) -> &'static str {
    match state {
        "passed" => "count-passed",
        "failed" => "count-failed",
        "running" => "count-running",
        "interrupted" => "count-interrupted",
        "pending" => "count-not-started",
        "skipped" => "count-skipped",
        _ => "count-unknown",
    }
}

#[component]
fn PageHeader(model: LiveModel, now: RwSignal<i64>) -> impl IntoView {
    move || {
        model.header.get().map(|header| {
            // The run is open until every job has ended, whatever its verdict says (a failed flow
            // reads Failed while the run is still going, issue #149).
            let running = !header.settled;
            let started = header.started_ms;
            let offset = utc_offset_minutes();
            let facts = {
                let mut parts: Vec<String> = Vec::new();
                parts.extend(header.git_ref.clone());
                parts.extend(header.commit.clone());
                parts.push(format!("attempt {}", header.attempt_number));
                parts.push(format!("started {}", format_timestamp(started, offset)));
                parts.join(" · ")
            };
            let derived = header.result_provenance == Provenance::Derived;
            let flows = header.flows;
            view! {
                <header data-testid="pipeline-header" class="page-h">
                    <div class="min-w-0">
                        <div class="row items-baseline gap-3.5">
                            <h2 class="m-0 text-[26px] font-semibold leading-[1.2] tracking-[-0.015em] max-[720px]:text-[21px]" data-testid="pipeline-title">
                                {header.title}
                            </h2>
                            <span data-testid="pipeline-result">
                                <StatusPill mark=header.result.mark label=header.result.label />
                            </span>
                            <span class="pv-statement">
                                <ProvenanceBadge provenance=Signal::stored(header.result_provenance) />
                                {derived.then(|| format!("from {flows} flows"))}
                            </span>
                        </div>
                        <p class="meta">
                            <span>{facts}</span>
                            {running.then(|| view! {
                                <span>{move || format!("running for {}", format_duration(now.get() - started))}</span>
                            })}
                            {header.ci_url.map(|url| view! {
                                <a class="text-link underline" href=url target="_blank" rel="noopener noreferrer">
                                    "Open the CI run"
                                </a>
                            })}
                        </p>
                    </div>
                </header>
            }
        })
    }
}

/// The progress of an attempt with a plan: a bar with a known total only. While the run is open
/// the sentence is the live progress (and says so when there is no plan); once it has settled, a
/// plan's total is said as accounted for.
#[component]
fn Progressline(model: LiveModel, settled: Signal<bool>) -> impl IntoView {
    view! {
        {move || model.progress.get().and_then(|p| p.fraction().map(|f| view! {
            <progress class="mb-3 block h-1.5 w-full" max="100" value=(f * 100.0).round() aria-label="Test progress"></progress>
        }))}
        <Show when=move || !settled.get()>
            {move || model.progress.get().map(|p| {
                let known = if p.is_uncertain() { "uncertain" } else { "known" };
                view! {
                    <p class="note" data-testid="progress-text" data-progress=known>
                        {p.text()}
                    </p>
                }
            })}
        </Show>
        <Show when=move || settled.get() && model.progress.with(|p| p.is_some_and(|p| !p.is_uncertain()))>
            {move || model.progress.get().map(|p| {
                let finished = p.finished();
                let total = p.total().unwrap_or_default();
                view! {
                    <p class="note" data-testid="plan-progress">
                        {format!("{finished} of {total} planned flows accounted for")}
                    </p>
                }
            })}
        </Show>
    }
}

/// The filter: one readout per flow state that has flows, and "All flows". The chosen readout is
/// a radio, so the flows below show only its state.
#[component]
fn Readouts(
    model: LiveModel,
    filter: RwSignal<Option<&'static str>>,
    chosen: Memo<Option<&'static str>>,
) -> impl IntoView {
    view! {
        <fieldset class="m-0 mb-9 min-w-0 border-0 p-0">
            <legend class="sr-only">"Show flows"</legend>
            <ReadoutStrip>
                <FlowReadout
                    state=None
                    filter=filter
                    chosen=chosen
                    count=Signal::derive(move || model.progress.get().map_or(0, |p| {
                        READOUTS.iter().map(|s| count_in(Some(p), s)).sum()
                    }))
                />
                {READOUTS
                    .into_iter()
                    .map(|state| view! {
                        <Show when=move || has_flows(model.progress.get(), state)>
                            <FlowReadout
                                state=Some(state)
                                filter=filter
                                chosen=chosen
                                count=Signal::derive(move || count_in(model.progress.get(), state))
                            />
                        </Show>
                    })
                    .collect_view()}
            </ReadoutStrip>
        </fieldset>
    }
}

/// One readout: a radio in a label, so the whole cell is the target.
#[component]
fn FlowReadout(
    /// The flow state it selects; `None` for all flows.
    state: Option<&'static str>,
    filter: RwSignal<Option<&'static str>>,
    chosen: Memo<Option<&'static str>>,
    count: Signal<u64>,
) -> impl IntoView {
    let testid = state.map_or("count-all", count_testid);
    let label = match state {
        Some(state) => {
            let pill = flow_state_pill(state);
            view! { <StatusPill mark=pill.mark label=pill.label /> }.into_any()
        }
        None => view! { "All flows" }.into_any(),
    };
    view! {
        <label class="tile">
            <input
                type="radio"
                name="flow-filter"
                prop:checked=move || chosen.get() == state
                on:change=move |_| filter.set(state)
            />
            <span class="tile-l">{label}</span>
            <span class="tile-n" data-testid=testid>{move || count.get()}</span>
        </label>
    }
}

/// The failed and interrupted flows, grouped by their message: the message is the line, and the
/// flows that failed with it sit under it. While the run is open the group says so once.
#[component]
fn Failures(
    model: LiveModel,
    project: StoredValue<String>,
    settled: Signal<bool>,
) -> impl IntoView {
    let groups = move || model.failures.with(|rows| message_groups(rows));
    view! {
        <section class="card" aria-labelledby="fail-h">
            <div class="card-h">
                <h2 id="fail-h">"Failures"</h2>
                <Show when=move || !settled.get()>
                    <span class="pv-statement"><ProvenanceBadge provenance=Signal::stored(Provenance::Provisional) /></span>
                </Show>
                <span class="note">
                    {move || {
                        let n = groups().len();
                        format!("Grouped by message · {n} {}", if n == 1 { "group" } else { "groups" })
                    }}
                </span>
            </div>
            <Show
                when=move || !model.failures.with(Vec::is_empty)
                fallback=move || view! {
                    <p class="note" data-testid="no-failures">
                        {move || if settled.get() { "No failures." } else { "None so far." }}
                    </p>
                }
            >
                <For
                    each=groups
                    key=|(message, rows)| (message.clone(), rows.iter().map(failure_key).collect::<Vec<_>>())
                    children=move |(message, rows)| view! {
                        <MessageGroup message=message rows=rows model=model project=project settled=settled />
                    }
                />
            </Show>
        </section>
    }
}

#[component]
fn MessageGroup(
    message: String,
    rows: Vec<FailureRow>,
    model: LiveModel,
    project: StoredValue<String>,
    settled: Signal<bool>,
) -> impl IntoView {
    let count = rows.len();
    let line = if message.is_empty() {
        "No message reported".to_owned()
    } else {
        message
    };
    view! {
        <div class="row mt-4 items-baseline justify-between border-b border-line pb-2">
            <p class="m-0 min-w-0 break-words font-mono text-[13px]">{line}</p>
            <span class="chip">{format!("{count} {}", if count == 1 { "flow" } else { "flows" })}</span>
        </div>
        <TableBox label="Flows that failed with this message">
            <table class="tbl tbl-cards">
                <caption class="sr-only">"Flows that failed with this message"</caption>
                <thead>
                    <tr>
                        <th scope="col">"Flow"</th>
                        <th scope="col">"Result"</th>
                        <th scope="col">"Device"</th>
                        <th scope="col" class="num">"Duration"</th>
                    </tr>
                </thead>
                <tbody>
                    {rows
                        .into_iter()
                        .map(|row| view! { <FailureLine row=row model=model project=project settled=settled /> })
                        .collect_view()}
                </tbody>
            </table>
        </TableBox>
    }
}

#[component]
fn FailureLine(
    row: FailureRow,
    model: LiveModel,
    project: StoredValue<String>,
    settled: Signal<bool>,
) -> impl IntoView {
    let flow_id = row.flow_id;
    let device = move || {
        model
            .flows
            .cell(flow_id)
            .and_then(|f| f.with(|f| f.device_session_id))
            .and_then(|d| model.devices.cell(d))
            .map_or_else(|| "—".to_owned(), |d| d.with(|d| d.serial.clone()))
    };
    let duration = move || {
        model
            .flows
            .cell(flow_id)
            .and_then(|f| f.with(|f| f.duration_ms))
            .map_or_else(|| "—".to_owned(), format_duration)
    };
    let provenance = row.provenance;
    let mark = move || row_mark(provenance, settled.get());
    view! {
        <tr data-testid="failure">
            <td data-label="Flow">
                <A href=flow_href(&project.get_value(), flow_id) attr:class="cell-title">{row.name.clone()}</A>
            </td>
            <td data-label="Result">
                <StatusPill mark=row.state.mark label=row.state.label />
                {move || mark().map(|p| view! {
                    <span data-testid="flow-provenance"><RowProvenance provenance=Signal::stored(p) /></span>
                })}
                {move || model.flows.cell(flow_id).map(|f| view! {
                    <OriginBadge origin=move || f.with(|f| f.origin) />
                })}
            </td>
            <td data-label="Device">{device}</td>
            <td data-label="Duration" class="num">{duration}</td>
        </tr>
    }
}

/// The flows of one attempt: each job with its devices, the passed flows collapsed, and the
/// timeline at the end.
#[component]
fn Flows(
    model: LiveModel,
    project: StoredValue<String>,
    settled: Signal<bool>,
    chosen: Memo<Option<&'static str>>,
    plans: RwSignal<Option<Vec<PlanView>>>,
    timeline: Signal<TimelineData>,
    now: RwSignal<i64>,
) -> impl IntoView {
    let jobs = move || {
        let epoch = model.epoch.get();
        model
            .tree
            .with(|t| t.jobs.iter().map(|j| (epoch, j.id)).collect::<Vec<_>>())
    };
    // A timeline of two devices or more is worth seeing at once; one device's is a click away.
    let two_devices = move || timeline.with(|t| t.lanes().len() >= 2);
    // A job's outcome the runner observed (its exit) is settled, so its rows carry no mark; the
    // group says once that those outcomes are observed (issue #149).
    let observed_outcome = move || {
        model.tree.with(|t| {
            t.jobs.iter().any(|j| {
                model.jobs.cell(j.id).is_some_and(|c| {
                    let job = c.get();
                    job.outcome.is_some()
                        && source_provenance(job.outcome_source.as_deref()) == Provenance::Observed
                })
            })
        })
    };
    view! {
        <section class="card" aria-labelledby="flows-h">
            <div class="card-h">
                <h2 id="flows-h">"Flows"</h2>
                <span class="note">"Grouped by job and device"</span>
                <p class="pv-statement" data-testid="flows-provenance">
                    {move || if settled.get() {
                        view! { <ProvenanceBadge provenance=Signal::stored(Provenance::Final) /> " unless marked" }.into_any()
                    } else {
                        view! { <ProvenanceBadge provenance=Signal::stored(Provenance::Provisional) /> " until the final report" }.into_any()
                    }}
                </p>
                <Show when=observed_outcome>
                    <p class="pv-statement" data-testid="job-outcomes-provenance">
                        <ProvenanceBadge provenance=Signal::stored(Provenance::Observed) />
                        " job outcomes, as each runner saw them"
                    </p>
                </Show>
            </div>
            <For
                each=jobs
                key=|k| *k
                children=move |(_, id)| view! {
                    <JobGroup model=model project=project id=id plans=plans chosen=chosen settled=settled />
                }
            />
            <details class="mt-6" open=two_devices>
                <summary class="cursor-pointer text-sm text-link underline">"Show timeline"</summary>
                <div class="mt-3">
                    <SwimlaneTimeline data=timeline now_ms=now utc_offset_minutes=utc_offset_minutes() />
                </div>
            </details>
        </section>
    }
}

/// The flows of one job (its suites' flows, then any flow whose suite is unknown), in run order.
fn job_flow_ids(model: &LiveModel, job: Uuid) -> Vec<Uuid> {
    model.tree.with(|t| {
        let mut ids: Vec<Uuid> = t
            .jobs
            .iter()
            .filter(|j| j.id == job)
            .flat_map(|j| j.suites.iter().flat_map(|s| s.flows.iter().copied()))
            .collect();
        ids.extend(t.orphan_flows.iter().copied().filter(|f| {
            model
                .flows
                .cell(*f)
                .is_some_and(|c| c.with(|f| f.job_id == job))
        }));
        ids
    })
}

/// A job's progress: the flows it has seen, with the plan total when every suite has one.
fn job_progress(model: &LiveModel, job: Uuid) -> Progress {
    let flows: Vec<FlowView> = job_flow_ids(model, job)
        .into_iter()
        .filter_map(|f| model.flows.cell(f))
        .map(|c| c.get())
        .collect();
    let suites: Vec<Uuid> = model.tree.with(|t| {
        t.jobs
            .iter()
            .filter(|j| j.id == job)
            .flat_map(|j| j.suites.iter().map(|s| s.id))
            .collect()
    });
    let planned: Option<Vec<i32>> = suites
        .iter()
        .map(|s| {
            model
                .suites
                .cell(*s)
                .and_then(|c| c.with(|s| s.planned_count))
        })
        .collect();
    let total = planned.map(|p| p.iter().map(|n| i64::from(*n)).sum::<i64>());
    Progress::from_flows(&flows, total)
}

#[component]
fn JobGroup(
    model: LiveModel,
    project: StoredValue<String>,
    id: Uuid,
    plans: RwSignal<Option<Vec<PlanView>>>,
    chosen: Memo<Option<&'static str>>,
    settled: Signal<bool>,
) -> impl IntoView {
    let Some(cell) = model.jobs.cell(id) else {
        return ().into_any();
    };
    let job: RwSignal<_> = cell.into();
    let runs = move || {
        model
            .tree
            .with(|t| t.jobs.iter().find(|j| j.id == id).map(|j| j.runs.clone()))
            .unwrap_or_default()
    };
    // The devices the job's flows ran on, by serial, so a device's group keeps its place.
    let devices = Memo::new(move |_| {
        let mut keyed = BTreeSet::new();
        for flow in job_flow_ids(&model, id) {
            let device = model
                .flows
                .cell(flow)
                .and_then(|c| c.with(|f| f.device_session_id));
            let serial = device
                .and_then(|d| model.devices.cell(d))
                .map(|c| c.with(|d| d.serial.clone()))
                .unwrap_or_default();
            keyed.insert((serial, device));
        }
        keyed
            .into_iter()
            .map(|(_, device)| device)
            .collect::<Vec<_>>()
    });
    let keyed = move || {
        let epoch = model.epoch.get();
        devices
            .get()
            .into_iter()
            .map(|d| (epoch, d))
            .collect::<Vec<_>>()
    };
    view! {
        <div class="mt-6 border-t border-line pt-3">
            <div class="row">
                <b class="font-medium">{move || job.with(|j| j.name.clone().unwrap_or_else(|| j.job_key.clone()))}</b>
                {move || {
                    let j = job.get();
                    let pill = job_pill(&j.lifecycle, j.outcome.as_deref());
                    let outcome = j.outcome.is_some().then(|| source_provenance(j.outcome_source.as_deref()));
                    view! {
                        <StatusPill mark=pill.mark label=pill.label />
                        {outcome
                            .filter(|p| *p != Provenance::Final)
                            .map(|p| view! { <RowProvenance provenance=Signal::stored(p) /> })}
                    }
                }}
                <span class="note" data-testid="suite-progress" data-progress=move || if job_progress(&model, id).is_uncertain() { "uncertain" } else { "known" }>
                    {move || job_line(&job_progress(&model, id), settled.get())}
                </span>
                <For
                    each=runs
                    key=|r| *r
                    children=move |run| view! { <RunLiveness model=model id=run /> }
                />
            </div>
            <For
                each=runs
                key=|r| *r
                children=move |run| view! { <RunFileWarnings model=model id=run /> }
            />
            <For
                each=keyed
                key=|k| *k
                children=move |(_, device)| view! {
                    <DeviceGroup model=model project=project job=id device=device plans=plans chosen=chosen settled=settled />
                }
            />
        </div>
    }
    .into_any()
}

/// The job's progress sentence. Once it has settled with no plan, only the flows seen are said.
fn job_line(progress: &Progress, settled: bool) -> String {
    if settled && progress.is_uncertain() {
        format!("{} flows finished", progress.finished())
    } else {
        progress.text()
    }
}

/// What an agent run's own files said, one notice per warning (issue #148): a commands file whose
/// flow the log never started. Such a file has no flow page, so the job's flows are where it is said.
#[component]
fn RunFileWarnings(model: LiveModel, id: Uuid) -> impl IntoView {
    let Some(cell) = model.runs.cell(id) else {
        return ().into_any();
    };
    let run: RwSignal<_> = cell.into();
    view! {
        {move || {
            run.with(|r| file_warning_notes(&r.file_warnings))
                .into_iter()
                .map(|text| view! { <Notice tone=Tone::Warning text=text /> })
                .collect_view()
        }}
    }
    .into_any()
}

#[component]
fn RunLiveness(model: LiveModel, id: Uuid) -> impl IntoView {
    let Some(cell) = model.runs.cell(id) else {
        return ().into_any();
    };
    let run: RwSignal<_> = cell.into();
    view! {
        <span data-testid="run-liveness">
            {move || {
                let pill = liveness_pill(run.get().liveness);
                view! { <StatusPill mark=pill.mark label=pill.label /> }
            }}
        </span>
        <OriginBadge origin=move || run.get().origin />
    }
    .into_any()
}

/// One device of a job: its flows in run order, with the plan's shard for it, the failing and
/// interrupted rows, and the passed ones collapsed under a disclosure.
#[component]
fn DeviceGroup(
    model: LiveModel,
    project: StoredValue<String>,
    job: Uuid,
    device: Option<Uuid>,
    plans: RwSignal<Option<Vec<PlanView>>>,
    chosen: Memo<Option<&'static str>>,
    settled: Signal<bool>,
) -> impl IntoView {
    let device_flows = Memo::new(move |_| {
        job_flow_ids(&model, job)
            .into_iter()
            .filter(|f| {
                model
                    .flows
                    .cell(*f)
                    .is_some_and(|c| c.with(|f| f.device_session_id == device))
            })
            .collect::<Vec<Uuid>>()
    });
    // Numbered by run order on the device, before any filter: a filtered row keeps its number.
    let split = Memo::new(move |_| {
        let chosen = chosen.get();
        let mut shown: Vec<Numbered> = Vec::new();
        let mut passed: Vec<Numbered> = Vec::new();
        for (i, flow) in device_flows.get().into_iter().enumerate() {
            let state = model
                .flows
                .cell(flow)
                .map_or("unknown", |c| c.with(|f| bucket(&f.state)));
            match chosen {
                None if state == "passed" => passed.push((i + 1, flow)),
                None => shown.push((i + 1, flow)),
                Some(wanted) if wanted == state => shown.push((i + 1, flow)),
                Some(_) => {}
            }
        }
        (shown, passed)
    });
    let label = move || {
        device
            .and_then(|d| model.devices.cell(d))
            .map(|c| c.with(|d| (d.serial.clone(), d.state.clone())))
    };
    // The plan's shard that ran this device's flows, planned against its actual time.
    let shard = move || -> Option<ShardRow> {
        let run = device_flows
            .get()
            .first()
            .and_then(|f| model.flows.cell(*f))
            .map(|c| c.with(|f| f.agent_run_id))?;
        plans.with(|plans| {
            plans.as_deref()?.iter().find_map(|plan| {
                let i = plan
                    .shards
                    .iter()
                    .position(|s| s.agent_runs.contains(&run))?;
                PlanComparison::new(plan).shards.get(i).cloned()
            })
        })
    };
    view! {
        <div class="mt-4">
            <p class="row note m-0">
                {move || match (label(), device) {
                    (Some((serial, state)), Some(device)) => view! {
                        <A href=device_href(&project.get_value(), device) attr:class="cell-title">{serial}</A>
                        <span>{state}</span>
                    }
                    .into_any(),
                    _ => view! { <b class="font-medium text-fg">"No device"</b> }.into_any(),
                }}
                <span>{move || format!("{} {}", device_flows.with(Vec::len), if device_flows.with(Vec::len) == 1 { "flow" } else { "flows" })}</span>
            </p>
            {move || shard().map(|row| view! {
                <p class="mt-2 font-mono text-[12.5px] text-muted" data-testid="shard-line">
                    {format!(
                        "Shard {} · planned {} · actual {} · {}",
                        row.shard_id,
                        show(row.planned_ms),
                        show(row.actual_ms),
                        row.difference_ms().map_or_else(|| "—".to_owned(), signed),
                    )}
                </p>
            })}
            <table class="tbl tbl-cards mt-2">
                <caption class="sr-only">"Flows on this device, in run order"</caption>
                <thead>
                    <tr>
                        <th scope="col" class="num">"#"</th>
                        <th scope="col">"Flow"</th>
                        <th scope="col">"Result"</th>
                        <th scope="col" class="num">"Duration"</th>
                    </tr>
                </thead>
                <tbody>
                    <For
                        each=move || split.get().0
                        key=|r| *r
                        children=move |(n, flow)| view! {
                            <FlowRow model=model project=project id=flow number=n settled=settled />
                        }
                    />
                </tbody>
            </table>
            <Show when=move || !split.with(|s| s.1.is_empty())>
                <details class="mt-2">
                    <summary class="cursor-pointer text-sm text-muted">
                        {move || format!("Show {} passed flows", split.with(|s| s.1.len()))}
                    </summary>
                    <table class="tbl tbl-cards mt-2">
                        <caption class="sr-only">"Passed flows on this device"</caption>
                        <tbody>
                            <For
                                each=move || split.get().1
                                key=|r| *r
                                children=move |(n, flow)| view! {
                                    <FlowRow model=model project=project id=flow number=n settled=settled />
                                }
                            />
                        </tbody>
                    </table>
                </details>
            </Show>
        </div>
    }
}

/// One flow's row. Its provenance shows only where it differs from the group's, which the
/// Flows heading states once.
#[component]
fn FlowRow(
    model: LiveModel,
    project: StoredValue<String>,
    id: Uuid,
    number: usize,
    settled: Signal<bool>,
) -> impl IntoView {
    let Some(cell) = model.flows.cell(id) else {
        return ().into_any();
    };
    let flow: RwSignal<_> = cell.into();
    let liveness = move || {
        model
            .runs
            .cell(flow.read().agent_run_id)
            .and_then(|c| c.get().liveness)
    };
    let mark = move || {
        let f = flow.get();
        row_mark(
            flow_provenance(&f.finality, &f.state, liveness()),
            settled.get(),
        )
    };
    view! {
        <tr data-testid="flow-row" data-state=move || flow.get().state>
            <td data-label="#" class="num">{number}</td>
            <td data-label="Flow">
                <A href=move || flow_href(&project.get_value(), id) attr:class="cell-title">
                    {move || flow.get().name}
                </A>
            </td>
            <td data-label="Result">
                {move || {
                    let pill = flow_state_pill(&flow.read().state);
                    view! { <StatusPill mark=pill.mark label=pill.label /> }
                }}
                {move || mark().map(|p| view! {
                    <span data-testid="flow-provenance"><RowProvenance provenance=Signal::stored(p) /></span>
                })}
                <OriginBadge origin=move || flow.get().origin />
            </td>
            <td data-label="Duration" class="num">
                {move || flow.get().duration_ms.map_or_else(|| "—".to_owned(), format_duration)}
            </td>
        </tr>
    }
    .into_any()
}

/// The newest few things that happened. Shown only while the run is open: a settled run's
/// history is its flows.
#[component]
fn Activity(model: LiveModel, project: StoredValue<String>) -> impl IntoView {
    let offset = utc_offset_minutes();
    view! {
        <section class="card" aria-labelledby="ac-h">
            <div class="card-h"><h2 id="ac-h">"Activity"</h2></div>
            <Show
                when=move || model.activity.with(|a| a.entries().next().is_some())
                fallback=|| view! { <p class="note">"Waiting for the next event."</p> }
            >
                <ol class="m-0 list-none p-0" data-testid="activity">
                    <For
                        each=move || model.activity.with(|a| a.entries().take(ACTIVITY_SHOWN).cloned().collect::<Vec<_>>())
                        key=|e| e.id
                        children=move |e| {
                            view! {
                                <li class="grid grid-cols-[70px_minmax(0,1fr)] gap-2.5 border-b border-line py-2 text-[13px]" data-testid="activity-entry">
                                    <span class="note font-mono">{format_timestamp(e.at_ms, offset)}</span>
                                    <span>
                                        {e.text.clone()}
                                        {e.flow_id.map(|f| view! {
                                            " " <A href=flow_href(&project.get_value(), f) attr:class="text-link underline">"Open"</A>
                                        })}
                                    </span>
                                </li>
                            }
                        }
                    />
                </ol>
            </Show>
        </section>
    }
}
