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
pub mod timeline;

#[cfg(test)]
mod fixtures;

use std::time::Duration;

use fleet_ui::{
    charts::format::{format_duration, format_timestamp},
    components::display::{Badge, Card, EmptyState, ProvenanceBadge, Skeleton, StatusPill},
    timeline::{model::TimelineData, view::SwimlaneTimeline},
};
use leptos::prelude::*;
use leptos_router::{components::A, hooks::use_params_map};
use uuid::Uuid;

use self::timeline::timeline_data;
use crate::{
    api,
    app::ErrorNotice,
    clock::now_ms,
    live::{
        model::LiveModel,
        socket::{Connection, Event, Subscription, connect},
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
    let live = {
        let project = project.clone();
        connect(
            Subscription {
                project: project.clone(),
                attempt: Some(attempt),
            },
            move |event| match event {
                Event::Frames(frames) => model.apply(&project, frames),
                Event::Lost => model.connection_lost(),
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
    // The timeline follows the live version (its times are not in the deltas).
    {
        let timeline = timeline.clone();
        Effect::new(move |_| {
            if model.version.get() > 0 {
                timeline.request();
            }
        });
    }
    let now = RwSignal::new(now_ms());
    if let Ok(handle) = set_interval_with_handle(move || now.set(now_ms()), Duration::from_secs(1))
    {
        on_cleanup(move || handle.clear());
    }
    let timeline_view = timeline.data;
    let data = Memo::new(move |_| match timeline_view.get() {
        Some(view) => timeline_data(&view, |run| {
            model.runs.cell(run).and_then(|c| c.get().liveness)
        }),
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
                        <PageHeader header=header connection=live.connection now=now />
                        <Show when=move || model.awaiting_snapshot.get()>
                            <p role="status" data-testid="resync" class="rounded border border-border bg-tone-warning-bg p-2 text-sm text-tone-warning-fg">
                                "Reconnecting to the live feed. This is the last known state; it is updating."
                            </p>
                        </Show>
                        <ProgressCard model=model />
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

/// The viewer's offset from UTC in minutes.
fn utc_offset_minutes() -> i32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a time-zone offset is a few hundred minutes"
    )]
    let minutes = -(js_sys::Date::new_0().get_timezone_offset() as i32);
    minutes
}

#[component]
fn PageHeader(
    header: model::Header,
    connection: ReadSignal<Connection>,
    now: RwSignal<i64>,
) -> impl IntoView {
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
                    <StatusPill tone=result.tone label=result.label />
                </span>
                <ProvenanceBadge provenance=header.result_provenance />
                <span
                    data-testid="connection"
                    data-state=move || connection.get().label()
                    class="ml-auto text-sm text-muted"
                >
                    {move || connection.get().label()}
                </span>
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
fn ProgressCard(model: LiveModel) -> impl IntoView {
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
                        }
                    })
            }}
        </Card>
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
                        key=|f| (f.flow_id, f.message.clone(), f.state.label)
                        children=move |f| {
                            view! {
                                <li class="flex flex-wrap items-center gap-2" data-testid="failure">
                                    <A href=f.href.clone() attr:class="font-medium text-primary hover:underline">{f.name.clone()}</A>
                                    <StatusPill tone=f.state.tone label=f.state.label />
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
                        <StatusPill tone=pill.tone label=pill.label />
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
                        view! { <StatusPill tone=pill.tone label=pill.label /> }
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
                        <StatusPill tone=pill.tone label=pill.label />
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
                view! { <StatusPill tone=pill.tone label=pill.label /> }
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
