// SPDX-License-Identifier: AGPL-3.0-only
//! The device page (screen C for one device session): status, what it is running, correlated
//! charts with the device's own timeline lane, and the flows it ran.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs"
)]

use fleet_protocol::rest::{DeviceDetailView, MetricsView, TimelineView};
use fleet_ui::{
    components::display::{Card, EmptyState},
    tone::Tone,
};
use leptos::prelude::*;

use crate::{
    app::ErrorNotice,
    screens::{
        common::Ctx,
        device::model::{DeviceHeader, ExecutionDetails, FlowRow, FlowRows},
        metrics::{MetricsPanel, RangeSelection},
        parts::{
            FactList, Fetched, MetricsCharts, Notice, Outcome, PillView, RangePicker,
            TimelineSection, loaded,
        },
        timeline::TimelineScope,
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
    /// The timeline of the device's attempt, as far as it is loaded.
    #[prop(into)]
    timelines: Signal<Vec<TimelineView>>,
    /// The project slug of the route.
    #[prop(into)]
    project: String,
    /// The current time (unix ms); update about once a second.
    #[prop(into)]
    now_ms: Signal<i64>,
    /// The viewer's offset from UTC in minutes.
    #[prop(default = 0)]
    utc_offset_minutes: i32,
    /// The cursor time shared by the charts and the timeline; the page makes one when unset.
    #[prop(optional)]
    cursor: Option<RwSignal<Option<i64>>>,
    /// Called with a flow execution id when a bar is activated: open its page.
    #[prop(optional, into)]
    on_open_flow: Option<Callback<String>>,
) -> impl IntoView {
    let cursor = cursor.unwrap_or_else(|| RwSignal::new(None));
    let ctx = Memo::new(move |_| Ctx {
        project: project.clone(),
        now_ms: now_ms.get(),
        utc_offset_minutes,
    });
    // The header holds "last seen … ago": derive it apart from the page's structure, so the page
    // isn't rebuilt every time `now_ms` ticks.
    let device_error = device.error;
    let metrics_error = metrics.error;
    let header = Memo::new(move |_| {
        device
            .data
            .get()
            .map(|d| DeviceHeader::build(&d, &ctx.read()))
    });
    view! {
           <article class="space-y-4" aria-labelledby="device-title" data-testid="device-page">
               <ErrorNotice error=device_error />
               {move || {
                   loaded(
    device,
                       |d| {
                           let ctx = ctx.get_untracked();
                           let execution = ExecutionDetails::build(&d, &ctx);
                           let flows = FlowRows::build(&d, &ctx);
                           let scope = TimelineScope::of_sessions(std::slice::from_ref(&d.session));
                           view! {
                               <HeaderView header=header />
                               <Card title="Execution">
                                   <ExecutionView execution=execution />
                               </Card>
                               <section aria-labelledby="charts-title" class="space-y-3">
                                   <h3 id="charts-title" class="text-lg font-semibold">"Emulator and device over time"</h3>
                                   <RangePicker range=range />
                                   <ErrorNotice error=metrics_error />
                                   {move || {
                                       loaded(
    metrics,
                                           |m| {
                                               view! {
                                                   <MetricsCharts
                                                       panel=MetricsPanel::build(&m)
                                                       cursor=cursor
                                                       utc_offset_minutes=utc_offset_minutes
                                                   />
                                               }
                                                   .into_any()
                                           },
                                       )
                                   }}
                               </section>
                               <TimelineSection
                                   scope=scope
                                   timelines=timelines
                                   now_ms=now_ms
                                   utc_offset_minutes=utc_offset_minutes
                                   cursor=cursor
                                   on_open_flow=on_open_flow.unwrap_or_else(|| Callback::new(|_| {}))
                               />
                               <FlowsSection flows=flows />
                           }
                               .into_any()
                       },
                   )
               }}
           </article>
       }
}

/// The device's name, status and facts.
#[component]
fn HeaderView(
    /// The header, once the device is loaded.
    header: Memo<Option<DeviceHeader>>,
) -> impl IntoView {
    move || {
        header.get().map(|h| {
            view! {
                <header class="space-y-2">
                    <div class="flex flex-wrap items-center gap-3">
                        <h2 id="device-title" data-testid="device-title" class="text-xl font-semibold">{h.title}</h2>
                        <span class="text-muted">{h.subtitle}</span>
                        <PillView pill=h.state />
                        {h
                            .worker_href
                            .map(|href| view! { <a class="underline" href=href>"Worker"</a> })}
                    </div>
                    <FactList facts=h.facts />
                </header>
            }
        })
    }
}

/// What the device is running now.
#[component]
fn ExecutionView(
    /// The details.
    execution: ExecutionDetails,
) -> impl IntoView {
    let current = match execution.current_flow {
        Some(f) => view! {
            <p>
                "Running "
                <a class="font-medium underline" href=f.href>{f.name}</a>
                <span class="ml-2 text-muted">{f.steps_seen}</span>
            </p>
        }
        .into_any(),
        None => view! { <p class="text-muted">"No flow is running on this device."</p> }.into_any(),
    };
    view! {
        <div class="space-y-2">
            {current}
            <FactList facts=execution.facts />
        </div>
    }
}

/// The flows that ran on the device.
#[component]
fn FlowsSection(
    /// The rows.
    flows: FlowRows,
) -> impl IntoView {
    let FlowRows { rows, summary } = flows;
    view! {
        <section aria-labelledby="flows-title" class="space-y-2">
            <h3 id="flows-title" class="text-lg font-semibold">"Flows on this device"</h3>
            <p class="text-sm text-muted">{summary}</p>
            {if rows.is_empty() {
                view! { <EmptyState title="No flows yet" description="Flow executions appear here as they start." /> }
                    .into_any()
            } else {
                view! {
                    <table class="w-full text-left text-sm">
                        <caption class="sr-only">"Flow executions on this device, newest first"</caption>
                        <thead class="border-b border-border text-muted">
                            <tr>
                                {["Flow", "Outcome", "Duration", "Steps", "Failure"]
                                    .map(|h| view! { <th scope="col" class="px-2 py-1 font-medium">{h}</th> })}
                            </tr>
                        </thead>
                        <tbody>{rows.into_iter().map(flow_row).collect_view()}</tbody>
                    </table>
                }
                    .into_any()
            }}
        </section>
    }
}

fn flow_row(r: FlowRow) -> impl IntoView {
    view! {
        <tr class="border-b border-border" data-testid="device-flow">
            <td class="px-2 py-1"><a class="underline" href=r.href>{r.name}</a></td>
            <td class="px-2 py-1">
                <Outcome pill=r.state provenance=r.provenance finality=r.finality_note />
                {r
                    .has_conflict
                    .then(|| {
                        view! {
                            <Notice tone=Tone::Warning text="Sources disagree" />
                        }
                    })}
            </td>
            <td class="px-2 py-1">{r.duration.unwrap_or_else(|| "—".to_owned())}</td>
            <td class="px-2 py-1 tabular-nums">{r.steps}</td>
            <td class="px-2 py-1">{r.failure.unwrap_or_default()}</td>
        </tr>
    }
}
