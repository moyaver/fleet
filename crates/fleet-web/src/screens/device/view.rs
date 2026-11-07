// SPDX-License-Identifier: AGPL-3.0-only
//! The device page (screen C for one device session): the device's name, state and profile, what
//! it is running now, its charts, and the flows it ran, the latest first.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs and the components return views"
)]

use fleet_protocol::rest::{DeviceDetailView, MetricsView};
use fleet_ui::{
    components::display::{Card, EmptyState, ProvenanceBadge, TableBox},
    provenance::Provenance,
    tone::Tone,
};
use leptos::prelude::*;
use leptos_router::components::A;

use crate::{
    app::ErrorNotice,
    paths::history_href,
    screens::{
        common::Ctx,
        device::model::{CHARTED, DeviceHeader, ExecutionDetails, FlowRow, FlowRows},
        metrics::{MetricsPanel, RangeSelection},
        parts::{Fetched, MetricsCharts, Notice, PillView, RangePicker, loaded},
    },
};

/// Screen C for a device session.
#[component]
pub fn DevicePage(
    /// The device session and its flows.
    device: Fetched<DeviceDetailView>,
    /// The metric series for `range`.
    metrics: Fetched<MetricsView>,
    /// The chosen range; the data layer refetches the metrics when it changes.
    range: RwSignal<RangeSelection>,
    /// The project slug of the route.
    #[prop(into)]
    project: String,
    /// The current time (unix ms); update about once a second.
    #[prop(into)]
    now_ms: Signal<i64>,
    /// The viewer's offset from UTC in minutes.
    #[prop(default = 0)]
    utc_offset_minutes: i32,
    /// The cursor time the charts share; the page makes one when unset.
    #[prop(optional)]
    cursor: Option<RwSignal<Option<i64>>>,
) -> impl IntoView {
    let cursor = cursor.unwrap_or_else(|| RwSignal::new(None));
    let history = history_href(&project);
    let ctx = Memo::new(move |_| Ctx {
        project: project.clone(),
        now_ms: now_ms.get(),
        utc_offset_minutes,
    });
    // The header holds "last seen … ago" and the flows hold durations: derive them apart from the
    // page's structure, so the page isn't rebuilt every time `now_ms` ticks.
    let device_error = device.error;
    let metrics_error = metrics.error;
    let header = Memo::new(move |_| {
        device
            .data
            .get()
            .map(|d| DeviceHeader::build(&d, &ctx.read()))
    });
    let panel = Memo::new(move |_| {
        metrics
            .data
            .get()
            .map(|m| MetricsPanel::build(&m).only(&CHARTED))
    });
    view! {
        <article class="stack" aria-labelledby="device-title" data-testid="device-page">
            <ErrorNotice error=device_error />
            <HeaderView header=header />
            {move || {
                loaded(
                    device,
                    |d| {
                        let ctx = ctx.get_untracked();
                        let execution = ExecutionDetails::build(&d, &ctx);
                        let flows = FlowRows::build(&d, &ctx);
                        view! {
                            <NowCard execution=execution />
                            <FlowsSection flows=flows />
                        }
                            .into_any()
                    },
                )
            }}
            <Card title="Emulator and device over time">
                {move || {
                    panel
                        .get()
                        .map(|p| {
                            let note = format!("{} · {}", p.window_label(utc_offset_minutes), p.resolution_note);
                            view! {
                                <p class="pv-statement">
                                    <span>{note}</span>
                                    <ProvenanceBadge provenance=Signal::stored(p.provenance) />
                                </p>
                            }
                        })
                }}
                <RangePicker range=range />
                <ErrorNotice error=metrics_error />
                {move || {
                    loaded(
                        metrics,
                        |m| {
                            view! {
                                <MetricsCharts
                                    panel=MetricsPanel::build(&m).only(&CHARTED)
                                    cursor=cursor
                                    utc_offset_minutes=utc_offset_minutes
                                />
                            }
                                .into_any()
                        },
                    )
                }}
            </Card>
            <p class="note">
                <A href=history>"Older executions in History"</A>
            </p>
        </article>
    }
}

/// The device's name, its state, one meta line of what it is, and that the agent observed it.
#[component]
fn HeaderView(
    /// The header, once the device is loaded.
    header: Memo<Option<DeviceHeader>>,
) -> impl IntoView {
    move || {
        header.get().map(|h| {
            view! {
                <header class="space-y-1">
                    <nav class="text-sm text-muted" aria-label="Breadcrumb">
                        <A href="/">"Overview"</A>
                        <span aria-hidden="true">" / "</span>
                        {match h.worker_href.clone() {
                            Some(href) => view! { <A href=href>{h.runner_name.clone()}</A> }.into_any(),
                            None => view! { <span>{h.runner_name.clone()}</span> }.into_any(),
                        }}
                        <span aria-hidden="true">" / "</span>
                        <span aria-current="page">{h.subtitle.clone()}</span>
                    </nav>
                    <div class="row row-head">
                        <h2 id="device-title" data-testid="device-title" class="text-xl font-semibold">{h.title}</h2>
                        <span class="note">{h.subtitle}</span>
                        <PillView pill=h.state />
                    </div>
                    <p class="meta">
                        <span>{h.meta}</span>
                        <ProvenanceBadge provenance=Signal::stored(Provenance::Observed) />
                    </p>
                </header>
            }
        })
    }
}

/// What the device is running now, and the job it serves.
#[component]
fn NowCard(
    /// The details.
    execution: ExecutionDetails,
) -> impl IntoView {
    let (job_key, job_href) = execution.job;
    let current = match execution.current_flow {
        Some(f) => view! {
            <p class="flex flex-wrap items-center gap-x-3 gap-y-1">
                <span>
                    "Running "
                    <a class="font-medium underline" href=f.href>{f.name}</a>
                </span>
                <span class="note">{f.steps_seen}</span>
                <ProvenanceBadge provenance=Signal::stored(Provenance::Provisional) />
            </p>
        }
        .into_any(),
        None => view! { <p class="text-muted">"No flow is running on this device."</p> }.into_any(),
    };
    view! {
        <Card title="Now">
            {current}
            <p class="note mt-1">
                "in job "
                <a class="underline" href=job_href>{job_key}</a>
            </p>
        </Card>
    }
}

/// The flows that ran on the device: the latest ten shown in full, the rest in History.
#[component]
fn FlowsSection(
    /// The rows.
    flows: FlowRows,
) -> impl IntoView {
    let FlowRows { rows, summary } = flows;
    view! {
        <Card title="Flows on this device">
            <p class="pv-statement">
                <ProvenanceBadge provenance=Signal::stored(Provenance::Final) />
                <span>"unless marked · " {summary}</span>
            </p>
            {if rows.is_empty() {
                view! { <EmptyState title="No flows yet" description="Flow executions appear here as they start." /> }
                    .into_any()
            } else {
                view! {
                    <TableBox label="Flow executions on this device">
                        <table class="tbl tbl-cards">
                            <caption class="sr-only">"Flow executions on this device, newest first"</caption>
                            <thead>
                                <tr>
                                    {["Flow", "Result", "Duration", "Steps", "Failure"]
                                        .map(|h| view! { <th scope="col">{h}</th> })}
                                </tr>
                            </thead>
                            <tbody>{rows.into_iter().map(flow_row).collect_view()}</tbody>
                        </table>
                    </TableBox>
                }
                    .into_any()
            }}
        </Card>
    }
}

fn flow_row(r: FlowRow) -> impl IntoView {
    // Settled results are silent; only a result that is not final carries its own mark.
    let mark = (r.provenance != Provenance::Final)
        .then(|| view! { <ProvenanceBadge provenance=Signal::stored(r.provenance) /> });
    view! {
        <tr data-testid="device-flow">
            <td data-label="Flow"><a class="cell-title" href=r.href>{r.name}</a></td>
            <td data-label="Result">
                <span class="flex flex-wrap items-center gap-2" title=r.finality_note>
                    <PillView pill=r.state />
                    {mark}
                    {r.has_conflict.then(|| view! { <Notice tone=Tone::Warning text="Sources disagree" /> })}
                </span>
            </td>
            <td data-label="Duration" class="num">{r.duration.unwrap_or_else(|| "—".to_owned())}</td>
            <td data-label="Steps" class="num">{r.steps}</td>
            <td data-label="Failure" class="mono">{r.failure.unwrap_or_default()}</td>
        </tr>
    }
}
