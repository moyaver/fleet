// SPDX-License-Identifier: AGPL-3.0-only
//! Screen B: one pipeline attempt (spec §8): header, progress, jobs → suites → flows, the
//! timeline, live activity and failures.
//!
//! The view-models are pure: [`model`] (the tree, the header, failures), [`activity`] (the live
//! event stream derived from what changed) and [`timeline`] (the REST timeline as the kit's
//! swimlane data). The page follows one attempt over a live socket: the snapshot and every
//! delta go through [`LiveModel`], whose per-entity signals keep a changing flow from
//! re-rendering the whole list.

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

use std::time::Duration;

use fleet_protocol::live::{LiveFrame, PlanView};
use fleet_ui::{
    charts::format::{format_duration, format_timestamp},
    components::{
        display::{Badge, Card, EmptyState, ProvenanceBadge, Skeleton, StatusPill, TableBox},
        live::LiveState,
    },
    timeline::{model::TimelineData, view::SwimlaneTimeline},
};
use leptos::prelude::*;
use leptos_router::{components::A, hooks::use_params_map};
use uuid::Uuid;

use self::{
    model::{LiveInputs, live_view},
    plans::{PlanComparison, show, signed},
    timeline::timeline_data,
};
use crate::{
    api::{self, ApiError},
    app::{ErrorNotice, LiveBar, LiveSlot},
    clock::{now_ms, utc_offset_minutes},
    live::{
        model::LiveModel,
        socket::{Event, Subscription, connect},
    },
    paths::flow_href,
    progress::Progress,
    remote::remote,
    status::{
        flow_provenance, flow_state_pill, job_pill, liveness_pill, source_provenance, suite_pill,
    },
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

const CARD_GAP: &str = "space-y-4";

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
    let settled_at = RwSignal::new(None::<i64>);
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
        // The first time the attempt is seen settled is when it settled, as far as this page knows.
        let settled_at = settled_at.get_untracked().or_else(|| {
            settled.then(|| {
                settled_at.set(Some(at));
                at
            })
        });
        let view = live_view(LiveInputs {
            connection: live.connection.get(),
            paused: paused.get(),
            held: held.with(Vec::len),
            settled,
            settled_at,
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
    let not_started = move || timeline_view.with(|t| t.as_ref().map_or(0, |t| t.not_started));
    let project = StoredValue::new(project);
    view! {
        <div class=CARD_GAP data-testid="pipeline-page">
            <ErrorNotice error=timeline.error />
            {move || model.error.get().map(|e| view! {
                <p role="alert" data-testid="error" class="rounded-lg border border-tone-danger-fg/30 bg-tone-danger-bg p-3 text-tone-danger-fg">{e}</p>
            })}
            {move || match model.header.get() {
                None => view! { <Skeleton lines=6 /> }.into_any(),
                Some(header) => {
                    view! {
                        <PageHeader header=header now=now />
                        <Show when=move || model.awaiting_snapshot.get()>
                            <p role="status" data-testid="resync" class="rounded border border-border bg-tone-warning-bg p-2 text-sm text-tone-warning-fg">
                                "Reconnecting to the live feed. This is the last known state; it is updating."
                            </p>
                        </Show>
                        <ProgressCard model=model plans=plans.data plans_error=plans.error />
                        <FailuresCard model=model />
                        <Jobs model=model project=project />
                        <Card title="Execution timeline">
                            <SwimlaneTimeline
                                data=data
                                now_ms=now
                                utc_offset_minutes=utc_offset_minutes()
                            />
                            <Show when=move || not_started() != 0>
                                <p class="mt-2 text-sm text-muted" data-testid="not-started">
                                    {move || format!("{} planned flows have not started yet.", not_started())}
                                </p>
                            </Show>
                        </Card>
                        <ActivityCard model=model project=project />
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

#[component]
fn PageHeader(header: model::Header, now: RwSignal<i64>) -> impl IntoView {
    let result = header.result;
    let running = result.label == "Running";
    let started = header.started_ms;
    let facts = {
        let mut parts: Vec<String> = Vec::new();
        parts.extend(header.git_ref.clone());
        parts.extend(header.commit.clone());
        parts.push(format!("attempt {}", header.attempt_number));
        parts.push(format!(
            "started {}",
            format_timestamp(started, utc_offset_minutes())
        ));
        parts.join(" · ")
    };
    view! {
        <header data-testid="pipeline-header" class="space-y-1">
            <div class="flex flex-wrap items-center gap-3">
                <h2 class="text-xl font-semibold" data-testid="pipeline-title">{header.title}</h2>
                <span data-testid="pipeline-result">
                    <StatusPill mark=result.mark label=result.label />
                </span>
                <ProvenanceBadge provenance=header.result_provenance />
            </div>
            <p class="text-sm text-muted">
                {facts}
                {running.then(|| view! {
                    <span>{move || format!(" · running for {}", format_duration(now.get() - started))}</span>
                })}
            </p>
            {header.ci_url.map(|url| view! {
                <a class="text-sm text-primary hover:underline" href=url target="_blank" rel="noopener noreferrer">
                    "Open the CI run"
                </a>
            })}
        </header>
    }
}

#[component]
fn ProgressCard(
    model: LiveModel,
    plans: RwSignal<Option<Vec<PlanView>>>,
    plans_error: RwSignal<Option<ApiError>>,
) -> impl IntoView {
    view! {
        <Card title="Test progress">
            {move || {
                model
                    .progress
                    .get()
                    .map(|p| {
                        let uncertain = if p.is_uncertain() { "uncertain" } else { "known" };
                        view! {
                            <p data-testid="progress-text" data-progress=uncertain class="font-medium">
                                {p.text()}
                            </p>
                            {p.fraction().map(|f| view! {
                                <progress class="mt-2 h-2 w-full" max="100" value=(f * 100.0).round() aria-label="Test progress"></progress>
                            })}
                            <dl class="mt-3 flex flex-wrap gap-x-6 gap-y-1 text-sm">
                                {counts(&p).into_iter().map(|(label, n)| view! {
                                    <div class="flex gap-1.5">
                                        <dt class="text-muted">{label}</dt>
                                        <dd class="tabular-nums font-medium" data-testid=format!("count-{}", label.to_lowercase())>{n}</dd>
                                    </div>
                                }).collect_view()}
                            </dl>
                            <ErrorNotice error=plans_error />
                            <PlanTable plans=plans />
                        }
                    })
            }}
        </Card>
    }
}

/// The attempt's plans: each shard planned against its actual time, and the slowest shard
/// against an equal-count split. Nothing shows for an attempt with no plan.
#[component]
fn PlanTable(plans: RwSignal<Option<Vec<PlanView>>>) -> impl IntoView {
    move || {
        plans.with(|plans| {
            plans
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|plan| {
                    let view = PlanComparison::new(plan);
                    let summary = view.summary();
                    view! {
                        <div class="mt-4 space-y-2" data-testid="plan">
                            <p class="text-xs text-muted">{format!("Plan {}", view.plan_id)}</p>
                            <TableBox label="Plan shards">
                                <table class="w-full text-sm" data-testid="plan-shards">
                                    <thead class="text-left text-xs text-muted">
                                        <tr>
                                            <th class="font-medium">"Shard"</th>
                                            <th class="font-medium">"Flows"</th>
                                            <th class="font-medium">"Planned"</th>
                                            <th class="font-medium">"Actual"</th>
                                            <th class="font-medium">"Difference"</th>
                                        </tr>
                                    </thead>
                                    <tbody class="tabular-nums">
                                        {view.shards.iter().map(|row| view! {
                                            <tr data-testid="plan-shard">
                                                <td>{row.shard_id}</td>
                                                <td>{row.flows}</td>
                                                <td>{show(row.planned_ms)}</td>
                                                <td>{show(row.actual_ms)}</td>
                                                <td>{row.difference_ms().map_or_else(|| "—".to_owned(), signed)}</td>
                                            </tr>
                                        }).collect_view()}
                                    </tbody>
                                </table>
                            </TableBox>
                            <ul class="space-y-1 text-sm text-muted" data-testid="plan-summary">
                                {summary.into_iter().map(|line| view! { <li>{line}</li> }).collect_view()}
                            </ul>
                        </div>
                    }
                })
                .collect_view()
        })
    }
}

fn counts(p: &Progress) -> [(&'static str, u64); 6] {
    [
        ("Passed", p.passed),
        ("Failed", p.failed),
        ("Running", p.running),
        ("Pending", p.pending),
        ("Skipped", p.skipped),
        ("Interrupted", p.interrupted + p.unknown),
    ]
}

#[component]
fn FailuresCard(model: LiveModel) -> impl IntoView {
    view! {
        <Card title="Failures">
            <Show
                when=move || !model.failures.with(Vec::is_empty)
                fallback=|| view! { <p class="text-muted" data-testid="no-failures">"No failures."</p> }
            >
                <ul class="space-y-2" data-testid="failures">
                    <For
                        each=move || model.failures.get()
                        key=model::failure_key
                        children=move |f| {
                            view! {
                                <li class="flex flex-wrap items-center gap-2" data-testid="failure">
                                    <A href=f.href.clone() attr:class="font-medium text-primary hover:underline">{f.name.clone()}</A>
                                    <StatusPill mark=f.state.mark label=f.state.label />
                                    <ProvenanceBadge provenance=f.provenance />
                                    {f.message.clone().map(|m| view! { <span class="text-sm text-muted">{m}</span> })}
                                </li>
                            }
                        }
                    />
                </ul>
            </Show>
        </Card>
    }
}

#[component]
fn Jobs(model: LiveModel, project: StoredValue<String>) -> impl IntoView {
    let keyed = move || {
        let epoch = model.epoch.get();
        model
            .tree
            .with(|t| t.jobs.iter().map(|j| (epoch, j.id)).collect::<Vec<_>>())
    };
    view! {
        <section aria-label="Jobs and devices" class="space-y-3">
            <h3 class="text-sm font-semibold text-muted">"Jobs and devices"</h3>
            <For
                each=keyed
                key=|k| *k
                children=move |(_, id)| view! { <JobCard model=model project=project id=id /> }
            />
            <Show when=move || !model.tree.with(|t| t.orphan_flows.is_empty())>
                <Card title="Flows without a suite">
                    <ul>
                        <For
                            each=move || {
                                let epoch = model.epoch.get();
                                model.tree.with(|t| t.orphan_flows.iter().map(|f| (epoch, *f)).collect::<Vec<_>>())
                            }
                            key=|k| *k
                            children=move |(_, id)| view! { <FlowRow model=model project=project id=id /> }
                        />
                    </ul>
                </Card>
            </Show>
        </section>
    }
}

#[component]
fn JobCard(model: LiveModel, project: StoredValue<String>, id: Uuid) -> impl IntoView {
    let Some(cell) = model.jobs.cell(id) else {
        return ().into_any();
    };
    let job: RwSignal<_> = cell.into();
    let node = Memo::new(move |_| {
        model
            .tree
            .with(|t| t.jobs.iter().find(|j| j.id == id).cloned())
            .unwrap_or_else(|| model::JobNode {
                id,
                runs: Vec::new(),
                suites: Vec::new(),
            })
    });
    let keyed = move |ids: Vec<Uuid>| {
        let epoch = model.epoch.get();
        ids.into_iter().map(|i| (epoch, i)).collect::<Vec<_>>()
    };
    view! {
        <section class="space-y-3 rounded-lg border border-border bg-surface p-4" data-testid="job">
            <header class="flex flex-wrap items-center gap-2">
                <h4 class="font-semibold">{move || job.with(|j| j.name.clone().unwrap_or_else(|| j.job_key.clone()))}</h4>
                {move || {
                    let j = job.get();
                    let pill = job_pill(&j.lifecycle, j.outcome.as_deref());
                    view! {
                        <StatusPill mark=pill.mark label=pill.label />
                        {j.outcome.is_some().then(|| view! {
                            <ProvenanceBadge provenance=source_provenance(j.outcome_source.as_deref()) />
                        })}
                    }
                }}
            </header>
            <For
                each=move || keyed(node.with(|n| n.runs.clone()))
                key=|k| *k
                children=move |(_, run)| view! { <RunRow model=model id=run /> }
            />
            <For
                each=move || keyed(node.with(|n| n.suites.iter().map(|s| s.id).collect()))
                key=|k| *k
                children=move |(_, suite)| view! { <SuiteBlock model=model project=project id=suite /> }
            />
        </section>
    }
    .into_any()
}

#[component]
fn RunRow(model: LiveModel, id: Uuid) -> impl IntoView {
    let Some(cell) = model.runs.cell(id) else {
        return ().into_any();
    };
    let run: RwSignal<_> = cell.into();
    view! {
        <div class="space-y-1" data-testid="run" data-liveness=move || format!("{:?}", run.get().liveness)>
            <div class="flex flex-wrap items-center gap-2 text-sm">
                <span class="font-medium">{move || run.with(|r| r.runner_name.clone().unwrap_or_else(|| "agent".to_owned()))}</span>
                <span data-testid="run-liveness">
                    {move || {
                        let pill = liveness_pill(run.get().liveness);
                        view! { <StatusPill mark=pill.mark label=pill.label /> }
                    }}
                </span>
            </div>
            <ul class="flex flex-wrap gap-2 text-xs text-muted" aria-label="Devices">
                <For
                    each=move || {
                        let epoch = model.epoch.get();
                        model
                            .devices
                            .keys
                            .get()
                            .into_iter()
                            .filter(|(e, _)| *e == epoch)
                            .filter(|(_, d)| {
                                model.devices.cell(*d).is_some_and(|c| c.read_untracked().agent_run_id == id)
                            })
                            .collect::<Vec<_>>()
                    }
                    key=|k| *k
                    children=move |(_, device)| {
                        let cell = model.devices.cell(device);
                        cell.map(|cell| {
                            let device: RwSignal<_> = cell.into();
                            view! {
                                <li data-testid="device">
                                    <Badge tone=fleet_ui::tone::Tone::Neutral>
                                        {move || device.with(|d| format!("{} · {}", d.serial, d.state))}
                                    </Badge>
                                </li>
                            }
                        })
                    }
                />
            </ul>
        </div>
    }
    .into_any()
}

#[component]
fn SuiteBlock(model: LiveModel, project: StoredValue<String>, id: Uuid) -> impl IntoView {
    let Some(cell) = model.suites.cell(id) else {
        return ().into_any();
    };
    let suite: RwSignal<_> = cell.into();
    let flow_ids = Memo::new(move |_| {
        model.tree.with(|t| {
            t.jobs
                .iter()
                .flat_map(|j| &j.suites)
                .find(|s| s.id == id)
                .map(|s| s.flows.clone())
                .unwrap_or_default()
        })
    });
    let progress = Signal::derive(move || {
        let flows: Vec<_> = flow_ids
            .get()
            .iter()
            .filter_map(|f| model.flows.cell(*f))
            .map(|c| c.get())
            .collect();
        Progress::from_flows(&flows, suite.get().planned_count.map(i64::from))
    });
    view! {
        <div class="space-y-1 border-l-2 border-border pl-3" data-testid="suite">
            <div class="flex flex-wrap items-center gap-2 text-sm">
                <span class="font-medium">"Suite"</span>
                {move || {
                    let s = suite.get();
                    let pill = suite_pill(&s.lifecycle, s.outcome.as_deref());
                    view! {
                        <StatusPill mark=pill.mark label=pill.label />
                        {s.outcome.is_some().then(|| view! {
                            <ProvenanceBadge provenance=source_provenance(s.outcome_source.as_deref()) />
                        })}
                    }
                }}
                <span class="text-muted" data-testid="suite-progress" data-progress=move || if progress.get().is_uncertain() { "uncertain" } else { "known" }>
                    {move || progress.get().text()}
                </span>
            </div>
            <ul class="space-y-1">
                <For
                    each=move || {
                        let epoch = model.epoch.get();
                        flow_ids.get().into_iter().map(|f| (epoch, f)).collect::<Vec<_>>()
                    }
                    key=|k| *k
                    children=move |(_, flow)| view! { <FlowRow model=model project=project id=flow /> }
                />
            </ul>
        </div>
    }
    .into_any()
}

#[component]
fn FlowRow(model: LiveModel, project: StoredValue<String>, id: Uuid) -> impl IntoView {
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
    let provenance = Signal::derive(move || {
        let f = flow.get();
        flow_provenance(&f.finality, &f.state, liveness())
    });
    let device = move || {
        flow.read()
            .device_session_id
            .and_then(|d| model.devices.cell(d))
            .map(|c| c.get().serial)
    };
    view! {
        <li class="flex flex-wrap items-center gap-2 text-sm" data-testid="flow-row" data-state=move || flow.get().state>
            <A
                href=move || flow_href(&project.get_value(), id)
                attr:class="font-medium text-primary hover:underline"
            >
                {move || flow.get().name}
            </A>
            {move || {
                let pill = flow_state_pill(&flow.read().state);
                view! { <StatusPill mark=pill.mark label=pill.label /> }
            }}
            <span data-testid="flow-provenance"><ProvenanceBadge provenance=provenance /></span>
            {move || device().map(|d| view! { <span class="text-xs text-muted">{d}</span> })}
            {move || flow.get().duration_ms.map(|ms| view! {
                <span class="text-xs text-muted">{format_duration(ms)}</span>
            })}
            {move || flow.get().failure_message.map(|m| view! { <span class="text-xs text-muted">{m}</span> })}
        </li>
    }
    .into_any()
}

#[component]
fn ActivityCard(model: LiveModel, project: StoredValue<String>) -> impl IntoView {
    let offset = utc_offset_minutes();
    view! {
        <Card title="Live activity">
            <Show
                when=move || model.activity.with(|a| a.entries().next().is_some())
                fallback=|| view! { <p class="text-muted">"Waiting for the next event."</p> }
            >
                <ol class="space-y-1 text-sm" data-testid="activity" aria-live="polite">
                    <For
                        each=move || model.activity.with(|a| a.entries().cloned().collect::<Vec<_>>())
                        key=|e| e.id
                        children=move |e| {
                            view! {
                                <li class="flex flex-wrap items-center gap-2" data-testid="activity-entry">
                                    <span class="tabular-nums text-xs text-muted">{format_timestamp(e.at_ms, offset)}</span>
                                    <Badge tone=e.tone>{e.text.clone()}</Badge>
                                    {e.flow_id.map(|f| view! {
                                        <A href=flow_href(&project.get_value(), f) attr:class="text-xs text-primary hover:underline">"Open"</A>
                                    })}
                                </li>
                            }
                        }
                    />
                </ol>
            </Show>
        </Card>
    }
}
