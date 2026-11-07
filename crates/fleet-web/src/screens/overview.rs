// SPDX-License-Identifier: AGPL-3.0-only
//! Screen A: the fleet overview (spec §8). The readouts filter one Pipelines table, under it what
//! needs attention and how the runners are. Live: any frame on a project's socket asks for a
//! fresh overview, at most once a second, so the counts move without a reload (acceptance
//! criterion 2). The top bar's live indicator follows the same sockets and can pause them.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs and the components return views"
)]

pub mod model;

use std::{cell::RefCell, collections::HashMap, rc::Rc, time::Duration};

use fleet_protocol::rest::history::HistoryEntryView;
use fleet_ui::{
    charts::format::format_duration,
    components::{
        display::{
            EmptyState, ProvenanceBadge, ProvenanceStatement, ReadoutChoice, ReadoutStrip,
            Skeleton, StatusPill, TableBox,
        },
        forms::use_url_query,
        live::LiveState,
    },
    mark::Mark,
    provenance::Provenance,
};
use futures_util::future::{join_all, try_join_all};
use leptos::prelude::*;
use leptos_router::components::A;
use uuid::Uuid;

use self::model::{
    AttentionRow, Board, FailedFlow, Filter, OverviewModel, PipelineRow, ProjectOverview, Readouts,
    RunnerCounts, WorkerRow, composition, failed_step, feed, freshness,
};
use crate::{
    api::{self, ApiError},
    app::{ErrorNotice, LiveBar, LiveSlot},
    clock::{now_ms, utc_offset_minutes},
    format::time_of_day,
    live::socket::{Connection, Event, Subscription, connect},
    paths::{escape, query_param, with_query_param},
    remote::{Remote, remote},
    screens::pipeline::model::{LiveInputs, live_view},
};

/// The URL parameter that holds the readout the table shows.
const FILTER_PARAM: &str = "filter";
/// How far back "Needs attention" looks, in milliseconds.
const ATTENTION_WINDOW_MS: f64 = 86_400_000.0;
/// How many failed flows "Needs attention" lists.
const ATTENTION_LIMIT: usize = 10;

/// The step and device of failed flows, by execution id. A failure does not change once it has
/// ended, so each one is asked for once.
type Details = Rc<RefCell<HashMap<Uuid, (Option<String>, Option<String>)>>>;

/// Fetches every project's overview and the newest failed flows of the last day.
async fn load(details: Details) -> Result<Board, ApiError> {
    let projects = api::projects().await?;
    let query = format!(
        "outcome=failed&from={}&limit={ATTENTION_LIMIT}",
        escape(&window_start())
    );
    let per_project = try_join_all(projects.into_iter().map(|project| {
        let query = query.clone();
        async move {
            let slug = project.slug.clone();
            let overview = api::overview(&slug).await?;
            let history = api::history(&slug, &query).await?;
            Ok::<_, ApiError>((ProjectOverview { project, overview }, slug, history.items))
        }
    }))
    .await?;
    let mut projects = Vec::new();
    let mut newest: Vec<(String, HistoryEntryView)> = Vec::new();
    for (overview, slug, items) in per_project {
        newest.extend(items.into_iter().map(|entry| (slug.clone(), entry)));
        projects.push(overview);
    }
    // Each project's list is its own newest, so the newest of all is among them.
    newest.sort_by_key(|(_, e)| std::cmp::Reverse(e.at));
    newest.truncate(ATTENTION_LIMIT);
    let failed = join_all(newest.into_iter().map(|(project, entry)| {
        let details = Rc::clone(&details);
        async move {
            let (step, device) = flow_detail(&details, &project, entry.flow_id).await;
            FailedFlow {
                project,
                entry,
                step,
                device,
            }
        }
    }))
    .await;
    Ok(Board { projects, failed })
}

/// The start of the attention window, in the ISO form the history takes.
fn window_start() -> String {
    let start = js_sys::Date::now() - ATTENTION_WINDOW_MS;
    js_sys::Date::new(&web_sys::wasm_bindgen::JsValue::from_f64(start))
        .to_iso_string()
        .into()
}

/// The step and device of one failed flow. A detail that cannot be read costs its row those two
/// facts, not the page; only a answer that arrived is kept.
async fn flow_detail(
    details: &Details,
    project: &str,
    flow: Uuid,
) -> (Option<String>, Option<String>) {
    let cached = details.borrow().get(&flow).cloned();
    if let Some(found) = cached {
        return found;
    }
    let Ok(view) = api::flow(project, flow).await else {
        return (None, None);
    };
    let found = (
        failed_step(
            view.failure.as_ref().and_then(|f| f.failed_step_index),
            &view.steps,
        ),
        view.device.as_ref().map(|d| d.device.serial.clone()),
    );
    details.borrow_mut().insert(flow, found.clone());
    found
}

/// The overview's socket side: one per project, each asking for a fresh overview on a frame,
/// unless the person paused. Returns the sockets' connection states.
fn follow_sockets(
    data: &Remote<Board>,
    slugs: Memo<Vec<String>>,
    pause: Pause,
) -> RwSignal<Vec<ReadSignal<Connection>>> {
    let sockets = RwSignal::new(Vec::<ReadSignal<Connection>>::new());
    // One socket per project (the server takes one subscription per socket). Any frame asks for a
    // fresh overview, unless the person paused: then the frames are counted and shown on request.
    let data = data.clone();
    Effect::new(move |_| {
        let mut states = Vec::new();
        for project in slugs.get() {
            let data = data.clone();
            let socket = connect(
                Subscription {
                    project,
                    attempt: None,
                },
                move |event| {
                    let Event::Frames(frames) = event else {
                        return;
                    };
                    if pause.paused.get_untracked() {
                        pause.held.update(|n| *n += frames.len());
                    } else {
                        pause.last_update.set(now_ms());
                        data.request();
                    }
                },
            );
            states.push(socket.connection);
        }
        sockets.set(states);
    });
    sockets
}

/// The pause of the live feed, as the top bar's menu chooses it: frames are held while paused,
/// and showing them asks for a fresh overview.
#[derive(Debug, Clone, Copy)]
struct Pause {
    paused: RwSignal<bool>,
    held: RwSignal<usize>,
    last_update: RwSignal<i64>,
}

/// Puts the page's live indicator in the top bar while the page is shown.
fn top_bar(data: &Remote<Board>, feed_state: Memo<Connection>, pause: Pause, now: RwSignal<i64>) {
    // Showing held frames asks for a fresh overview. The bar's callback cannot hold the request
    // (it is not `Send`), so it bumps a counter that an effect answers.
    let shown = RwSignal::new(0_u64);
    {
        let data = data.clone();
        Effect::new(move |_| {
            if shown.get() > 0 {
                data.request();
            }
        });
    }
    let bar = LiveBar {
        state: RwSignal::new(LiveState::Connecting),
        detail: RwSignal::new(String::new()),
        action: RwSignal::new(None),
        on_action: Callback::new(move |()| {
            if pause.paused.get_untracked() {
                pause.paused.set(false);
                pause.held.set(0);
                shown.update(|n| *n += 1);
            } else {
                pause.paused.set(true);
            }
        }),
    };
    Effect::new(move |_| {
        let view = live_view(LiveInputs {
            connection: feed_state.get(),
            paused: pause.paused.get(),
            held: pause.held.get(),
            settled: false,
            now: now.get(),
            last_update: pause.last_update.get(),
            utc_offset_minutes: utc_offset_minutes(),
        });
        bar.state.set(view.state);
        bar.detail.set(view.detail);
        bar.action.set(view.action);
    });
    let slot = expect_context::<LiveSlot>().0;
    slot.set(Some(bar));
    // Clears the slot only if it still holds this page's bar.
    let mine = bar.state;
    on_cleanup(move || {
        slot.update(|current| {
            if current.is_some_and(|b| b.state == mine) {
                *current = None;
            }
        });
    });
}

/// Screen A.
#[component]
pub fn Overview() -> impl IntoView {
    let details: Details = Rc::default();
    let data = remote(1000, move || load(Rc::clone(&details)));
    let model = Memo::new(move |_| data.data.with(|d| d.as_ref().map(OverviewModel::build)));
    let slugs = Memo::new(move |_| {
        data.data.with(|d| {
            d.as_ref()
                .map(|d| {
                    d.projects
                        .iter()
                        .map(|p| p.project.slug.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let pause = Pause {
        paused: RwSignal::new(false),
        held: RwSignal::new(0),
        last_update: RwSignal::new(now_ms()),
    };
    let sockets = follow_sockets(&data, slugs, pause);
    Effect::new(move |_| {
        data.data.track();
        pause.last_update.set(now_ms());
    });
    let loaded = Memo::new(move |_| data.data.with(Option::is_some));
    let feed_state = Memo::new(move |_| {
        let states: Vec<Connection> = sockets.with(|list| list.iter().map(|s| s.get()).collect());
        feed(&states, loaded.get())
    });
    let now = RwSignal::new(now_ms());
    if let Ok(handle) = set_interval_with_handle(move || now.set(now_ms()), Duration::from_secs(1))
    {
        on_cleanup(move || handle.clear());
    }
    top_bar(&data, feed_state, pause, now);

    let (query, set_query) = use_url_query();
    let filter =
        Memo::new(move |_| Filter::parse(query_param(&query.get(), FILTER_PARAM).as_deref()));
    let choose = Callback::new(move |f: Filter| {
        let value = (f != Filter::All).then_some(f.value());
        set_query.run(with_query_param(
            &query.get_untracked(),
            FILTER_PARAM,
            value,
        ));
    });
    view! {
        <div>
            <h2 class="mb-4 text-xl font-semibold">"Overview"</h2>
            <ErrorNotice error=data.error />
            {move || match model.get() {
                Some(m) => overview_body(m, filter, choose, now).into_any(),
                None if data.error.with(Option::is_some) => ().into_any(),
                None => view! { <Skeleton lines=6 /> }.into_any(),
            }}
        </div>
    }
}

/// Link text: the theme's link colour, underlined in a quiet line.
const LINK: &str =
    "text-link underline decoration-line-strong underline-offset-[3px] hover:decoration-fg";

fn overview_body(
    m: OverviewModel,
    filter: Memo<Filter>,
    choose: Callback<Filter>,
    now: RwSignal<i64>,
) -> impl IntoView {
    let pipelines = StoredValue::new(m.pipelines);
    view! {
        {readout_strip(m.readouts, filter, choose)}
        <div class="grid gap-x-10 gap-y-9 min-[721px]:grid-cols-[minmax(0,1fr)_minmax(0,300px)]">
            <div class="stack">
                {attention_section(m.attention)}
                {move || {
                    let f = filter.get();
                    let rows = pipelines.with_value(|all| {
                        all.iter().filter(|r| r.in_filter(f)).cloned().collect::<Vec<_>>()
                    });
                    pipelines_section(rows, f, now)
                }}
            </div>
            <aside class="stack" aria-label="Runners">
                {runners_section(m.runners, m.workers, now)}
            </aside>
        </div>
    }
}

/// "1 flow", "3 flows".
fn flow_count(n: u64) -> String {
    if n == 1 {
        "1 flow".to_owned()
    } else {
        format!("{n} flows")
    }
}

fn readout_strip(r: Readouts, filter: Memo<Filter>, choose: Callback<Filter>) -> impl IntoView {
    let pick = move |f: Filter| Callback::new(move |()| choose.run(f));
    let is = move |f: Filter| Signal::derive(move || filter.get() == f);
    view! {
        <ReadoutStrip>
            <ReadoutChoice
                name="pipelines"
                value=Filter::All.value()
                label="All pipelines"
                count=r.all.to_string()
                checked=is(Filter::All)
                on_select=pick(Filter::All)
                test_id="tile-all"
            />
            <ReadoutChoice
                name="pipelines"
                value=Filter::Running.value()
                label="Running"
                count=r.running.to_string()
                checked=is(Filter::Running)
                on_select=pick(Filter::Running)
                test_id="tile-running"
                note=(r.running_flows > 0).then(|| format!("{} running now", flow_count(r.running_flows)))
            />
            <ReadoutChoice
                name="pipelines"
                value=Filter::Failed.value()
                label="Failed"
                count=r.failed.to_string()
                checked=is(Filter::Failed)
                on_select=pick(Filter::Failed)
                test_id="tile-failed"
                note=(r.failed_flows > 0).then(|| format!("{} failed", flow_count(r.failed_flows)))
            />
            <ReadoutChoice
                name="pipelines"
                value=Filter::NoFailures.value()
                label="No failures"
                count=r.no_failures.to_string()
                checked=is(Filter::NoFailures)
                on_select=pick(Filter::NoFailures)
                test_id="tile-no-failures"
            />
        </ReadoutStrip>
    }
}

fn attention_section(rows: Vec<AttentionRow>) -> impl IntoView {
    let offset = utc_offset_minutes();
    view! {
        <section class="card" aria-labelledby="attn-h" data-testid="needs-attention">
            <div class="card-h">
                <h2 id="attn-h">"Needs attention"</h2>
                <ProvenanceStatement provenance=Provenance::Final>"unless marked"</ProvenanceStatement>
                <span class="note">"Failed flows, last 24 h, newest first"</span>
            </div>
            {if rows.is_empty() {
                view! { <p class="note">"No flow has failed in the last 24 h."</p> }.into_any()
            } else {
                view! {
                    <ul class="list">
                        {rows.into_iter().map(|r| attention_item(r, offset)).collect_view()}
                    </ul>
                }
                    .into_any()
            }}
        </section>
    }
}

fn attention_item(r: AttentionRow, offset: i32) -> impl IntoView {
    let where_it_ran = format!(
        "{} · {}",
        r.step.unwrap_or_else(|| "Step not recorded".to_owned()),
        r.device.unwrap_or_else(|| "device not recorded".to_owned()),
    );
    view! {
        <li data-testid="attention-row">
            <div class="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
                <div class="row min-w-0">
                    <StatusPill mark=Mark::Failed label="Failed" />
                    {r.provenance.map(|p| view! { <ProvenanceBadge provenance=Signal::stored(p) /> })}
                    <A href=r.flow_href attr:class=format!("cell-title {LINK}")>{r.name}</A>
                </div>
                <span class="note num">{time_of_day(r.at_ms, offset)}</span>
            </div>
            <p class="mt-1.5 break-words font-mono text-[13px]">
                {r.message.unwrap_or_else(|| "No message recorded".to_owned())}
            </p>
            <p class="note mt-1">
                {where_it_ran}" · "
                <A href=r.run_href attr:class=LINK>{r.run}</A>
            </p>
        </li>
    }
}

fn pipelines_section(rows: Vec<PipelineRow>, filter: Filter, now: RwSignal<i64>) -> impl IntoView {
    let body = if !rows.is_empty() {
        pipelines_table(rows, now).into_any()
    } else if filter == Filter::All {
        view! {
            <EmptyState
                title=filter.empty_title()
                description="Runs appear here as soon as an agent reports in."
            />
        }
        .into_any()
    } else {
        view! { <EmptyState title=filter.empty_title() /> }.into_any()
    };
    view! {
        <section class="card" aria-labelledby="pl-h">
            <div class="card-h">
                <h2 id="pl-h">"Pipelines"</h2>
                <ProvenanceStatement provenance=Provenance::Final>"unless marked"</ProvenanceStatement>
                <span class="note">"Verdicts"</span>
                <ProvenanceBadge provenance=Signal::stored(Provenance::Derived) />
            </div>
            {body}
        </section>
    }
}

fn pipelines_table(rows: Vec<PipelineRow>, now: RwSignal<i64>) -> impl IntoView {
    let offset = utc_offset_minutes();
    view! {
        <TableBox label="Pipelines">
            <table data-testid="pipelines-table" class="tbl tbl-cards">
                <caption class="sr">"Pipelines, running first, then newest finished"</caption>
                <thead>
                    <tr>
                        <th scope="col">"Verdict"</th>
                        <th scope="col">"Run"</th>
                        <th scope="col">"Results"</th>
                        <th scope="col">"Finished"</th>
                    </tr>
                </thead>
                <tbody>
                    {rows
                        .into_iter()
                        .map(|r| pipeline_row(r, now, offset))
                        .collect_view()}
                </tbody>
            </table>
        </TableBox>
    }
}

fn pipeline_row(r: PipelineRow, now: RwSignal<i64>, offset: i32) -> impl IntoView {
    let testid = if r.open { "active-row" } else { "recent-row" };
    let pill = r.verdict.pill();
    let results_cell = results(&r);
    let attempt = (r.attempt_number > 1).then(|| format!(" · attempt {}", r.attempt_number));
    let started = r.started_ms;
    let finished_cell = match r.finished_ms {
        None => view! {
            <span class="note">
                "Running · "
                <span class="num">{move || format_duration(now.get() - started)}</span>
            </span>
        }
        .into_any(),
        Some(end) => view! {
            <span class="num small">{time_of_day(end, offset)}</span>
            <span class="note">
                {format!(" · {} · ", format_duration(end - started))}
                {move || format!("{} ago", format_duration((now.get() - end).max(0)))}
            </span>
        }
        .into_any(),
    };
    view! {
        <tr data-testid=testid data-attempt=r.attempt_id.to_string()>
            <td data-label="Verdict">
                <StatusPill mark=pill.mark label=pill.label />
            </td>
            <td data-label="Run">
                <A href=r.href attr:class=format!("cell-title {LINK}")>{r.run_key}</A>
                <div class="cell-sub">{r.run}{attempt}</div>
                {r.ci_url.map(|url| view! {
                    <a class="note underline" href=url target="_blank" rel="noopener noreferrer">
                        "Open run"
                    </a>
                })}
            </td>
            <td data-label="Results">{results_cell}</td>
            <td data-label="Finished">{finished_cell}</td>
        </tr>
    }
}

/// The Results cell: the counts with their provenance, then the bar. An open run says its
/// progress; a finished one shows its composition.
fn results(r: &PipelineRow) -> AnyView {
    let mark = r
        .provenance
        .map(|p| view! { <ProvenanceBadge provenance=Signal::stored(p) /> });
    let counts = view! {
        <span class="small">
            <span class="num" data-testid="passed">{r.progress.passed}</span>" passed · "
            <span class="num" data-testid="failed">{r.progress.failed}</span>" failed"
            {(r.open).then(|| view! {
                " · "<span class="num" data-testid="running">{r.progress.running}</span>" running"
            })}
        </span>
    };
    let head = view! { <div class="row">{counts}{mark}</div> };
    if r.open {
        let progress = if r.progress.is_uncertain() {
            "uncertain"
        } else {
            "known"
        };
        view! {
            {head}
            {open_bar(r)}
            <p class="note mt-1" data-testid="progress-text" data-progress=progress>
                {r.progress.text()}
            </p>
        }
        .into_any()
    } else {
        view! {
            {head}
            {composition_bar(r, composition(&r.flows))}
        }
        .into_any()
    }
}

/// The bar of an open run: a fraction of a known total, or one cell per flow seen when no plan
/// says how many there are.
fn open_bar(r: &PipelineRow) -> impl IntoView {
    let p = &r.progress;
    if let Some(total) = p.total() {
        let pct = |n: u64| format!("{}%", percent(n, total));
        let label = format!("{} of {total} flows finished", p.finished());
        view! {
            <div class="bar" role="img" aria-label=label style:width="200px" style:background="var(--line)">
                <i class="b-prov" style:width=pct(p.passed)></i>
                <i class="b-fail" style:width=pct(p.failed)></i>
                <i class="b-run" style:width=pct(p.running)></i>
            </div>
        }
        .into_any()
    } else {
        let cells: Vec<&'static str> = std::iter::repeat_n("b-prov", usize_of(p.passed))
            .chain(std::iter::repeat_n("b-fail", usize_of(p.failed)))
            .chain(std::iter::repeat_n("b-run", usize_of(p.running)))
            .chain(std::iter::repeat_n(
                "b-idle",
                usize_of(p.skipped + p.interrupted + p.unknown),
            ))
            .collect();
        let label = format!(
            "{} passed provisionally, {} failed, {} running; total unknown",
            p.passed, p.failed, p.running
        );
        view! {
            <div class="bar" role="img" aria-label=label>
                {cells
                    .into_iter()
                    .map(|cls| view! { <i class=cls style:width="14px" style:flex="none"></i> })
                    .collect_view()}
            </div>
        }
        .into_any()
    }
}

/// The composition bar of a finished run: each state's share of the flows it has.
fn composition_bar(r: &PipelineRow, label: String) -> impl IntoView {
    let f = &r.flows;
    let (passed, failed) = (count(f.passed), count(f.failed));
    let whole =
        count(f.passed + f.failed + f.skipped + f.interrupted + f.unknown + f.running + f.pending);
    let rest = whole.saturating_sub(passed + failed);
    let pct = |n: u64| format!("{}%", percent(n, whole));
    view! {
        <div class="bar" role="img" aria-label=label style:width="200px">
            <i class="b-pass" style:width=pct(passed)></i>
            <i class="b-fail" style:width=pct(failed)></i>
            <i class="b-idle" style:width=pct(rest)></i>
        </div>
    }
}

/// Integer percent of `whole`, 0 when there is nothing to share.
fn percent(part: u64, whole: u64) -> u64 {
    (part * 100).checked_div(whole).unwrap_or(0)
}

fn count(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn usize_of(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn runners_section(
    counts: RunnerCounts,
    rows: Vec<WorkerRow>,
    now: RwSignal<i64>,
) -> impl IntoView {
    let words = counts.words();
    let counts_text = if words.is_empty() {
        "None seen in the last day".to_owned()
    } else {
        words
    };
    let body = if rows.is_empty() {
        view! { <EmptyState title="No runners yet" description="A runner shows up when its agent connects." /> }
            .into_any()
    } else {
        view! {
            <ul class="list">
                {rows.into_iter().map(|w| runner_item(w, now)).collect_view()}
            </ul>
        }
        .into_any()
    };
    view! {
        <section class="card" aria-labelledby="rn-h">
            <div class="card-h">
                <h2 id="rn-h">"Runners"</h2>
                <span class="note" data-testid="runner-counts">{counts_text}</span>
                <ProvenanceStatement provenance=Provenance::Observed>"as each runner reported them"</ProvenanceStatement>
            </div>
            {body}
        </section>
    }
}

fn runner_item(w: WorkerRow, now: RwSignal<i64>) -> impl IntoView {
    let at = w.readings_at_ms;
    let provenance = Signal::derive(move || freshness(at, now.get()));
    let active = format!(
        "{} active run{}",
        w.active_runs,
        if w.active_runs == 1 { "" } else { "s" }
    );
    let line = [w.platform, Some(active)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    view! {
        <li class="row justify-between" data-testid="runner-row">
            <div class="min-w-0">
                <A href=w.href attr:class=format!("cell-title {LINK}")>{w.name}</A>
                <div class="cell-sub">{line}</div>
            </div>
            <div class="text-right">
                <span data-testid="runner-liveness">
                    <StatusPill mark=w.liveness.mark label=w.liveness.label />
                </span>
                <div class="note mt-1">
                    "CPU "
                    <span class="num">{w.cpu.unwrap_or_else(|| "Unavailable".to_owned())}</span>
                    {move || {
                        let p = provenance.get();
                        let age = at.map(|at| format!(" · {} ago", format_duration((now.get() - at).max(0))));
                        view! {
                            {(p != Provenance::Observed).then(|| view! { <ProvenanceBadge provenance=Signal::stored(p) /> })}
                            {age.unwrap_or_else(|| " · no readings reported".to_owned())}
                        }
                    }}
                </div>
            </div>
        </li>
    }
}
