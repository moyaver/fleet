// SPDX-License-Identifier: AGPL-3.0-only
//! The runner page (screen C for a worker): the machine's name and sample, health now, its charts,
//! the flows on its devices, its runs and its devices. Data arrives through props; the router and
//! data layer fetch it, and keep it fresh while the page is open.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs"
)]

use fleet_protocol::rest::{MetricsView, TimelineView, WorkerRunPage, WorkerView};
use fleet_ui::{
    components::display::{
        Card, EmptyState, ProvenanceBadge, ReadoutStrip, RowProvenance, TableBox,
    },
    provenance::Provenance,
};
use leptos::prelude::*;
use leptos_router::components::A;

use crate::{
    app::ErrorNotice,
    screens::{
        common::Ctx,
        device::model::DeviceSessionRow,
        metrics::{MetricsPanel, RangeSelection},
        parts::{Fetched, MetricsCharts, PillView, RangePicker, TimelineSection, loaded},
        timeline::TimelineScope,
        worker::model::{
            CHARTED, HealthCell, HealthValue, NextPage, RunRow, RunsList, WorkerHeader,
            device_rows, health_cells,
        },
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
    let panel = Memo::new(move |_| {
        metrics
            .data
            .get()
            .map(|m| MetricsPanel::build(&m).only(&CHARTED))
    });
    let health = Memo::new(move |_| {
        worker
            .data
            .get()
            .map(|w| health_cells(&w, panel.read().as_ref()))
    });
    view! {
        <article class="stack" aria-labelledby="worker-title" data-testid="worker-page">
            <ErrorNotice error=worker_error />
            <HeaderView header=header />
            <HealthCard health=health />
            <section class="card" aria-labelledby="charts-title">
                <div class="card-h"><h2 id="charts-title">"Machine health over time"</h2></div>
                <div class="card-b space-y-4">
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
                </div>
            </section>
            {move || {
                loaded(
                    worker,
                    |w| {
                        let ctx = ctx.get_untracked();
                        let devices = device_rows(&w, &ctx);
                        let scope = TimelineScope::of_sessions(&w.devices);
                        view! {
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

/// The runner's name, its liveness, when it was last sampled, and its facts on one meta line.
#[component]
fn HeaderView(
    /// The header, once the worker is loaded.
    header: Memo<Option<WorkerHeader>>,
) -> impl IntoView {
    move || {
        header.get().map(|h| {
            let (sampled_now, stale) = (h.sampled.is_some(), h.provenance == Provenance::Stale);
            view! {
                <header class="space-y-1">
                    <nav class="text-sm text-muted" aria-label="Breadcrumb">
                        <A href="/">"Overview"</A>
                        <span aria-hidden="true">" / "</span>
                        <span aria-current="page">{h.title.clone()}</span>
                    </nav>
                    <div class="row row-head">
                        <h2 id="worker-title" class="text-xl font-semibold">{h.title}</h2>
                        <span data-testid="runner-liveness"><PillView pill=h.liveness /></span>
                        {h.sampled.map(|s| view! { <span class="note">{s}</span> })}
                        {(sampled_now || stale)
                            .then(|| view! { <RowProvenance provenance=Signal::stored(h.provenance) /> })}
                    </div>
                    <p class="meta">{h.meta}</p>
                </header>
            }
        })
    }
}

/// Health now: a readout strip of the newest values, each with its unit or the word Missing.
#[component]
fn HealthCard(
    /// The cells, once the worker is loaded.
    health: Memo<Option<Vec<HealthCell>>>,
) -> impl IntoView {
    let body = move || {
        health.get().map(|cells| {
            view! {
                <ReadoutStrip>
                    {cells.into_iter().map(health_cell).collect_view()}
                </ReadoutStrip>
            }
        })
    };
    view! { <Card title="Health now">{body}</Card> }
}

fn health_cell(cell: HealthCell) -> impl IntoView {
    let (value, unavailable) = match cell.value {
        HealthValue::Value(text) => (
            view! { <span class="tile-n tabular-nums">{text}</span> }.into_any(),
            false,
        ),
        HealthValue::Missing(reason) => (
            view! {
                <span class="flex flex-wrap items-center gap-2">
                    <ProvenanceBadge provenance=Signal::stored(Provenance::Missing) />
                    {reason.map(|r| view! { <span class="note">{r}</span> })}
                </span>
            }
            .into_any(),
            true,
        ),
    };
    view! {
        <div class="tile" data-testid="health-tile" data-metric=cell.id>
            <p class="tile-l">{cell.label}</p>
            <p data-testid="health-value" data-unavailable=unavailable.then_some("")>{value}</p>
            {cell.note.map(|n| view! { <p class="note mt-1">{n}</p> })}
        </div>
    }
}

/// The worker's runs, with a button for older ones. A run's verdict comes from its exit status.
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
        <Card title="Runs">
            {move || {
                let list = list.get();
                if list.rows().is_empty() {
                    return view! { <EmptyState title="No runs yet" description="No agent run has used this runner." /> }
                        .into_any();
                }
                let next = list.next_page();
                view! {
                    <p class="pv-statement">
                        <span>"Verdict from each run's Maestro exit status, newest first"</span>
                        <ProvenanceBadge provenance=Signal::stored(Provenance::Derived) />
                    </p>
                    <TableBox label="Runs on this runner">
                        <table class="tbl tbl-cards">
                            <caption class="sr-only">"Runs on this runner, newest first"</caption>
                            <thead>
                                <tr>
                                    {["Run", "State", "Started", "Duration"]
                                        .map(|h| view! { <th scope="col">{h}</th> })}
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
                                    class="btn btn-sm"
                                    on:click=move |_| on_more_runs.run(next.clone())
                                >
                                    "Load older runs"
                                </button>
                            }
                        })}
                }
                    .into_any()
            }}
        </Card>
    }
}

fn run_row(r: &RunRow) -> impl IntoView + use<> {
    let open = r.open.then(|| {
        view! {
            <ProvenanceBadge provenance=Signal::stored(Provenance::Provisional) />
        }
    });
    view! {
        <tr data-testid="run-row">
            <td data-label="Run"><a class="cell-title" href=r.href.clone()>{r.job_key.clone()}</a></td>
            <td data-label="State">
                <span class="flex flex-wrap items-center gap-2">
                    <PillView pill=r.verdict />
                    {open}
                    <span class="note">{r.outcome.clone()}</span>
                </span>
            </td>
            <td data-label="Started" class="num">{r.started.clone()}</td>
            <td data-label="Duration" class="num">{r.duration.clone()}</td>
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
        <Card title="Devices">
            {if devices.is_empty() {
                view! { <EmptyState title="No devices" description="This runner has not reported any device." /> }
                    .into_any()
            } else {
                view! {
                    <TableBox label="Devices on this runner">
                        <table class="tbl tbl-cards">
                            <caption class="sr-only">"Devices hosted by this runner"</caption>
                            <thead>
                                <tr>
                                    {["Device", "State", "Profile", "Job"]
                                        .map(|h| view! { <th scope="col">{h}</th> })}
                                </tr>
                            </thead>
                            <tbody>
                                {devices
                                    .into_iter()
                                    .map(|d| {
                                        view! {
                                            <tr>
                                                <td data-label="Device">
                                                    <a class="cell-title" href=d.href>{d.serial}</a>
                                                </td>
                                                <td data-label="State"><PillView pill=d.state /></td>
                                                <td data-label="Profile">
                                                    {d.profile.unwrap_or_else(|| "Not reported".to_owned())}
                                                </td>
                                                <td data-label="Job">{d.job_key}</td>
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
        </Card>
    }
}
