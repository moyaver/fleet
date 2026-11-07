// SPDX-License-Identifier: AGPL-3.0-only
//! The worker page (screen C): header, health now, correlated charts with the device timeline,
//! runs and devices. Data arrives through props; the router and data layer fetch it.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs"
)]

use fleet_protocol::rest::{MetricsView, TimelineView, WorkerRunPage, WorkerView};
use fleet_ui::{
    charts::series::Reading,
    components::display::{Card, EmptyState, ProvenanceBadge, TableBox},
    tone::Tone,
};
use leptos::prelude::*;

use crate::{
    app::ErrorNotice,
    screens::{
        common::Ctx,
        device::model::DeviceSessionRow,
        metrics::{MetricsPanel, RangeSelection},
        parts::{
            FactList, Fetched, MetricsCharts, Notice, PillView, RangePicker, TimelineSection,
            loaded,
        },
        timeline::TimelineScope,
        worker::model::{HostHealth, NextPage, RunRow, RunsList, WorkerHeader, device_rows},
    },
};

/// Screen C for a worker.
#[component]
pub fn WorkerPage(
    /// The worker, its latest runs and devices.
    worker: Fetched<WorkerView>,
    /// Further pages of the worker's runs, in the order they were fetched.
    #[prop(into)]
    more_runs: Signal<Vec<WorkerRunPage>>,
    /// Called when the user asks for more runs.
    on_more_runs: Callback<NextPage>,
    /// The metric series for `range`.
    metrics: Fetched<MetricsView>,
    /// The chosen range; the data layer refetches the metrics when it changes.
    range: RwSignal<RangeSelection>,
    /// The timelines of the attempts named by [`TimelineScope`], as far as they are loaded.
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
    // What depends on the clock is derived apart from the page's structure, so the page isn't
    // rebuilt (and focus isn't lost) every time `now_ms` ticks.
    let worker_error = worker.error;
    let metrics_error = metrics.error;
    let header = Memo::new(move |_| {
        worker
            .data
            .get()
            .map(|w| WorkerHeader::build(&w, &ctx.read()))
    });
    let health = Memo::new(move |_| {
        worker
            .data
            .get()
            .map(|w| HostHealth::build(&w, &ctx.read()))
    });
    view! {
           <article class="space-y-4" aria-labelledby="worker-title" data-testid="worker-page">
               <ErrorNotice error=worker_error />
               {move || {
                   loaded(
    worker,
                       |w| {
                           let ctx = ctx.get_untracked();
                           let devices = device_rows(&w, &ctx);
                           let scope = TimelineScope::of_sessions(&w.devices);
                           view! {
                               <HeaderView header=header />
                               <HealthCard health=health />
                               <section aria-labelledby="charts-title" class="space-y-3">
                                   <h3 id="charts-title" class="text-lg font-semibold">"Machine health over time"</h3>
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
                               <RunsSection worker=w ctx=ctx more_runs=more_runs on_more_runs=on_more_runs />
                               <DevicesSection devices=devices />
                           }
                               .into_any()
                       },
                   )
               }}
           </article>
       }
}

/// The machine's name, liveness and facts.
#[component]
fn HeaderView(
    /// The header, once the worker is loaded.
    header: Memo<Option<WorkerHeader>>,
) -> impl IntoView {
    move || {
        header.get().map(|h| {
            view! {
                <header class="space-y-2">
                    <div class="flex flex-wrap items-center gap-3">
                        <h2 id="worker-title" class="text-xl font-semibold">{h.title}</h2>
                        <PillView pill=h.liveness />
                    </div>
                    <FactList facts=h.facts />
                </header>
            }
        })
    }
}

/// The newest host sample: every metric with its unit, or why it has none.
#[component]
fn HealthCard(
    /// The health, once the worker is loaded.
    health: Memo<Option<HostHealth>>,
) -> impl IntoView {
    let body = move || match health.get() {
        None => ().into_any(),
        Some(health) => health_body(health),
    };
    view! { <Card title="Health now">{body}</Card> }
}

fn health_body(health: HostHealth) -> AnyView {
    match health {
        HostHealth::NoRecentSample => view! {
            <EmptyState
                title="No recent host sample"
                description="The server has no host sample from the last hour. That says nothing about how the machine is doing."
            />
        }
        .into_any(),
        HostHealth::Sample(s) => view! {
            <p class="mb-2 flex flex-wrap items-center gap-2 text-sm text-muted">
                <span>{format!("Sampled {}", s.taken)}</span>
                <ProvenanceBadge provenance=s.provenance />
            </p>
            <ul class="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
                {s
                    .tiles
                    .into_iter()
                    .map(|t| {
                        let unavailable = matches!(t.reading, Reading::Unavailable { .. });
                        let text = t.text();
                        view! {
                            <li class="rounded border border-border p-3" data-testid="health-tile" data-metric=t.id>
                                <p class="text-sm text-muted">{t.label}</p>
                                <p
                                    class=if unavailable {
                                        "mt-1 italic text-muted"
                                    } else {
                                        "mt-1 text-xl font-semibold tabular-nums"
                                    }
                                    data-testid="health-value"
                                    data-unavailable=unavailable.then_some("")
                                >
                                    {text}
                                </p>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        }
        .into_any(),
    }
}

/// The worker's runs, with a button for older ones.
#[component]
fn RunsSection(
    /// The worker.
    worker: WorkerView,
    /// More pages.
    more_runs: Signal<Vec<WorkerRunPage>>,
    /// Text context.
    ctx: Ctx,
    /// Ask for more.
    on_more_runs: Callback<NextPage>,
) -> impl IntoView {
    let list = Memo::new(move |_| {
        let mut list = RunsList::from_view(&worker, &ctx);
        for page in more_runs.read().iter() {
            list.apply_page(page, &ctx);
        }
        list
    });
    view! {
        <section aria-labelledby="runs-title" class="space-y-2">
            <h3 id="runs-title" class="text-lg font-semibold">"Runs"</h3>
            {move || {
                let list = list.get();
                if list.rows().is_empty() {
                    return view! { <EmptyState title="No runs yet" description="No agent run has used this worker." /> }
                        .into_any();
                }
                let next = list.next_page();
                view! {
                    <TableBox label="Runs on this worker">
                        <table class="w-full text-left text-sm">
                            <caption class="sr-only">"Runs on this worker, newest first"</caption>
                            <thead class="border-b border-border text-muted">
                                <tr>
                                    {["Job", "State", "Started", "Duration", "Exit code", "Data gaps"]
                                        .map(|h| view! { <th scope="col" class="px-2 py-1 font-medium">{h}</th> })}
                                </tr>
                            </thead>
                            <tbody>{list.rows().iter().map(run_row).collect_view()}</tbody>
                        </table>
                    </TableBox>
                    {(next != NextPage::Done)
                        .then(|| {
                            view! {
                                <button
                                    type="button"
                                    class="rounded border border-border bg-surface px-3 py-1.5 text-sm"
                                    on:click=move |_| on_more_runs.run(next.clone())
                                >
                                    "Load older runs"
                                </button>
                            }
                        })}
                }
                    .into_any()
            }}
        </section>
    }
}

fn run_row(r: &RunRow) -> impl IntoView + use<> {
    view! {
        <tr class="border-b border-border">
            <td class="px-2 py-1"><a class="underline" href=r.href.clone()>{r.job_key.clone()}</a></td>
            <td class="px-2 py-1">
                <span class="mr-2">{r.state.clone()}</span>
                <PillView pill=r.liveness />
            </td>
            <td class="px-2 py-1 tabular-nums">{r.started.clone()}</td>
            <td class="px-2 py-1">{r.duration.clone().unwrap_or_else(|| "—".to_owned())}</td>
            <td class="px-2 py-1">
                {r.exit_code.map_or_else(|| "—".to_owned(), |c| c.to_string())}
            </td>
            <td class="px-2 py-1">
                {match r.gaps.clone() {
                    Some(g) => view! { <Notice tone=Tone::Warning text=g /> }.into_any(),
                    None => view! { <span class="text-muted">"None"</span> }.into_any(),
                }}
            </td>
        </tr>
    }
}

/// The devices the worker hosted.
#[component]
fn DevicesSection(
    /// The rows.
    devices: Vec<DeviceSessionRow>,
) -> impl IntoView {
    view! {
        <section aria-labelledby="devices-title" class="space-y-2">
            <h3 id="devices-title" class="text-lg font-semibold">"Devices"</h3>
            {if devices.is_empty() {
                view! { <EmptyState title="No devices" description="This worker has not reported any device." /> }
                    .into_any()
            } else {
                view! {
                    <TableBox label="Devices on this runner">
                        <table class="w-full text-left text-sm">
                            <caption class="sr-only">"Devices hosted by this worker"</caption>
                            <thead class="border-b border-border text-muted">
                                <tr>
                                    {["Device", "State", "Profile", "Job", "Last seen"]
                                        .map(|h| view! { <th scope="col" class="px-2 py-1 font-medium">{h}</th> })}
                                </tr>
                            </thead>
                            <tbody>
                                {devices
                                    .into_iter()
                                    .map(|d| {
                                        view! {
                                            <tr class="border-b border-border">
                                                <td class="px-2 py-1">
                                                    <a class="underline" href=d.href>{d.serial}</a>
                                                </td>
                                                <td class="px-2 py-1"><PillView pill=d.state /></td>
                                                <td class="px-2 py-1">
                                                    {d.profile.unwrap_or_else(|| "Not reported".to_owned())}
                                                </td>
                                                <td class="px-2 py-1">{d.job_key}</td>
                                                <td class="px-2 py-1">{d.last_seen}</td>
                                            </tr>
                                        }
                                    })
                                    .collect_view()}
                            </tbody>
                        </table>
                    </TableBox>
                }
                    .into_any()
            }}
        </section>
    }
}
