// SPDX-License-Identifier: AGPL-3.0-only
//! Screen A: the fleet overview (spec §8): what is running, what just finished and how the
//! runners are. Live: any frame on a project's socket asks for a fresh overview, at most once a
//! second, so the counts move without a reload (acceptance criterion 2).

pub mod model;

use std::time::Duration;

use fleet_ui::{
    charts::format::format_duration,
    components::display::{Card, EmptyState, ProvenanceBadge, Skeleton, StatTile, StatusPill},
};
use futures_util::future::try_join_all;
use leptos::prelude::*;
use leptos_router::components::A;

use self::model::{ActiveRow, OverviewModel, ProjectOverview, RecentRow, WorkerRow, freshness};
use crate::{
    api::{self, ApiError},
    app::ErrorNotice,
    clock::now_ms,
    live::socket::{Event, Subscription, connect},
    remote::remote,
};

/// Fetches every project's overview.
async fn load() -> Result<Vec<ProjectOverview>, ApiError> {
    let projects = api::projects().await?;
    try_join_all(projects.into_iter().map(|project| async move {
        let overview = api::overview(&project.slug).await?;
        Ok::<_, ApiError>(ProjectOverview { project, overview })
    }))
    .await
}

/// Screen A.
#[component]
pub fn Overview() -> impl IntoView {
    let data = remote(1000, load);
    let model = Memo::new(move |_| data.data.with(|d| d.as_deref().map(OverviewModel::build)));
    let slugs = Memo::new(move |_| {
        data.data.with(|d| {
            d.as_ref()
                .map(|d| d.iter().map(|p| p.project.slug.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    // One socket per project (the server takes one subscription per socket). Any frame, and the
    // snapshot-less subscription's first `subscribed`, ask for a fresh overview.
    {
        let data = data.clone();
        Effect::new(move |_| {
            for project in slugs.get() {
                let data = data.clone();
                connect(
                    Subscription {
                        project,
                        attempt: None,
                    },
                    move |event| {
                        if matches!(event, Event::Frames(_)) {
                            data.request();
                        }
                    },
                );
            }
        });
    }
    let now = RwSignal::new(now_ms());
    if let Ok(handle) = set_interval_with_handle(move || now.set(now_ms()), Duration::from_secs(1))
    {
        on_cleanup(move || handle.clear());
    }
    view! {
        <div class="space-y-6">
            <h2 class="text-xl font-semibold">"Overview"</h2>
            <ErrorNotice error=data.error />
            {move || match model.get() {
                Some(m) => overview_body(m, now).into_any(),
                None if data.error.with(Option::is_some) => ().into_any(),
                None => view! { <Skeleton lines=6 /> }.into_any(),
            }}
        </div>
    }
}

fn overview_body(m: OverviewModel, now: RwSignal<i64>) -> impl IntoView {
    let s = m.summary.clone();
    let cpu = s.average_cpu_pct.map(|v| format!("{v:.0}"));
    view! {
        <section aria-label="Summary" class="grid grid-cols-2 gap-3 md:grid-cols-4 lg:grid-cols-7">
            <div data-testid="tile-active"><StatTile label="Active pipelines" value=Some(s.active_attempts.to_string()) /></div>
            <div data-testid="tile-running-jobs"><StatTile label="Running jobs" value=Some(s.running_jobs.to_string()) /></div>
            <div data-testid="tile-running-flows"><StatTile label="Running flows" value=Some(s.running_flows.to_string()) /></div>
            <div data-testid="tile-passed"><StatTile label="Passed flows" value=Some(s.passed_flows.to_string()) /></div>
            <div data-testid="tile-failed"><StatTile label="Failed flows" value=Some(s.failed_flows.to_string()) /></div>
            <div data-testid="tile-runners">
                <StatTile label="Runners online" value=Some(s.runners.online.to_string()) />
                <p class="mt-1 text-xs text-muted" data-testid="runner-counts">
                    {format!(
                        "{} stale · {} disconnected",
                        s.runners.stale,
                        s.runners.disconnected,
                    )}
                </p>
            </div>
            <div data-testid="tile-cpu">
                <StatTile label="Runner CPU (average)" value=cpu unit="%" />
            </div>
        </section>
        <Card title="Active pipelines">
            {if m.active.is_empty() {
                view! { <EmptyState title="No active pipelines" description="Runs appear here as soon as an agent reports in." /> }
                    .into_any()
            } else {
                active_table(m.active, now).into_any()
            }}
        </Card>
        <Card title="Runner health">
            {if m.workers.is_empty() {
                view! { <EmptyState title="No runners yet" description="A runner shows up when its agent connects." /> }
                    .into_any()
            } else {
                workers_table(m.workers, now).into_any()
            }}
        </Card>
        <Card title="Recently finished">
            {if m.recent.is_empty() {
                view! { <p class="text-muted">"Nothing has finished yet."</p> }.into_any()
            } else {
                recent_table(m.recent).into_any()
            }}
        </Card>
    }
}

const TH: &str = "px-2 py-1 text-left font-semibold";
const TD: &str = "px-2 py-1 align-top";

fn active_table(rows: Vec<ActiveRow>, now: RwSignal<i64>) -> impl IntoView {
    view! {
        <table data-testid="active-table" class="w-full border-collapse text-sm">
            <caption class="sr-only">"Active pipeline runs"</caption>
            <thead class="border-b border-border">
                <tr>
                    <th scope="col" class=TH>"Project"</th>
                    <th scope="col" class=TH>"Pipeline"</th>
                    <th scope="col" class=TH>"Status"</th>
                    <th scope="col" class=TH>"Progress"</th>
                    <th scope="col" class=TH>"Passed"</th>
                    <th scope="col" class=TH>"Failed"</th>
                    <th scope="col" class=TH>"Elapsed"</th>
                    <th scope="col" class=TH>"CI"</th>
                </tr>
            </thead>
            <tbody>
                {rows
                    .into_iter()
                    .map(|r| {
                        let uncertain = if r.progress.is_uncertain() { "uncertain" } else { "known" };
                        let started = r.started_ms;
                        view! {
                            <tr class="border-b border-border" data-testid="active-row" data-attempt=r.attempt_id.to_string()>
                                <td class=TD>{r.project.clone()}</td>
                                <td class=TD>
                                    <A href=r.href.clone() attr:class="font-medium text-primary hover:underline">{r.title.clone()}</A>
                                    <div class="text-xs text-muted">{refs_line(r.git_ref.as_deref(), r.commit.as_deref(), r.attempt_number)}</div>
                                </td>
                                <td class=TD>
                                    <StatusPill tone=r.status.tone label=r.status.label />
                                </td>
                                <td class=TD data-testid="progress-text" data-progress=uncertain>{r.progress.text()}</td>
                                <td class=TD data-testid="passed">{r.progress.passed}</td>
                                <td class=TD data-testid="failed">{r.progress.failed}</td>
                                <td class=TD>{move || format_duration(now.get() - started)}</td>
                                <td class=TD>
                                    {r.ci_url.map(|url| view! {
                                        <a class="text-primary hover:underline" href=url target="_blank" rel="noopener noreferrer">
                                            "Open run"
                                        </a>
                                    })}
                                </td>
                            </tr>
                        }
                    })
                    .collect_view()}
            </tbody>
        </table>
    }
}

fn refs_line(git_ref: Option<&str>, commit: Option<&str>, attempt: i32) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.extend(git_ref.map(str::to_owned));
    parts.extend(commit.map(str::to_owned));
    parts.push(format!("attempt {attempt}"));
    parts.join(" · ")
}

fn workers_table(rows: Vec<WorkerRow>, now: RwSignal<i64>) -> impl IntoView {
    view! {
        <table data-testid="workers-table" class="w-full border-collapse text-sm">
            <caption class="sr-only">"Runner health"</caption>
            <thead class="border-b border-border">
                <tr>
                    <th scope="col" class=TH>"Runner"</th>
                    <th scope="col" class=TH>"Status"</th>
                    <th scope="col" class=TH>"Workload"</th>
                    <th scope="col" class=TH>"Health"</th>
                    <th scope="col" class=TH>"Data freshness"</th>
                </tr>
            </thead>
            <tbody>
                {rows
                    .into_iter()
                    .map(|w| {
                        let at = w.readings_at_ms;
                        let provenance = Signal::derive(move || freshness(at, now.get()));
                        view! {
                            <tr class="border-b border-border" data-testid="runner-row">
                                <td class=TD>
                                    <A href=w.href.clone() attr:class="font-medium text-primary hover:underline">{w.name.clone()}</A>
                                    <div class="text-xs text-muted">{w.platform.clone()}</div>
                                </td>
                                <td class=TD data-testid="runner-liveness">
                                    <StatusPill tone=w.liveness.tone label=w.liveness.label />
                                </td>
                                <td class=TD>{format!("{} active run{}", w.active_runs, if w.active_runs == 1 { "" } else { "s" })}</td>
                                <td class=TD>
                                    <ul class="space-y-0.5">
                                        {w.readings
                                            .into_iter()
                                            .map(|r| {
                                                let class = if r.available { "" } else { "italic text-muted" };
                                                view! {
                                                    <li data-testid="reading" data-available=r.available.to_string()>
                                                        <span class="text-muted">{r.label}": "</span>
                                                        <span class=class title=r.reason.unwrap_or_default()>{r.text}</span>
                                                    </li>
                                                }
                                            })
                                            .collect_view()}
                                    </ul>
                                </td>
                                <td class=TD>
                                    <ProvenanceBadge provenance=provenance />
                                    {at.map(|at| view! {
                                        <span class="ml-2 text-xs text-muted">
                                            {move || format!("{} ago", format_duration((now.get() - at).max(0)))}
                                        </span>
                                    })}
                                </td>
                            </tr>
                        }
                    })
                    .collect_view()}
            </tbody>
        </table>
    }
}

fn recent_table(rows: Vec<RecentRow>) -> impl IntoView {
    view! {
        <table data-testid="recent-table" class="w-full border-collapse text-sm">
            <caption class="sr-only">"Recently finished pipelines"</caption>
            <thead class="border-b border-border">
                <tr>
                    <th scope="col" class=TH>"Project"</th>
                    <th scope="col" class=TH>"Pipeline"</th>
                    <th scope="col" class=TH>"Jobs"</th>
                    <th scope="col" class=TH>"Flows"</th>
                    <th scope="col" class=TH>"Took"</th>
                </tr>
            </thead>
            <tbody>
                {rows
                    .into_iter()
                    .map(|r| {
                        view! {
                            <tr class="border-b border-border" data-testid="recent-row" data-attempt=r.attempt_id.to_string()>
                                <td class=TD>{r.project.clone()}</td>
                                <td class=TD>
                                    <A href=r.href.clone() attr:class="font-medium text-primary hover:underline">{r.title.clone()}</A>
                                    <div class="text-xs text-muted">{refs_line(r.git_ref.as_deref(), r.commit.as_deref(), r.attempt_number)}</div>
                                </td>
                                <td class=TD>
                                    <ul class="space-y-1">
                                        {r.jobs
                                            .into_iter()
                                            .map(|j| view! {
                                                <li class="flex items-center gap-2">
                                                    <span>{j.name}</span>
                                                    <StatusPill tone=j.pill.tone label=j.pill.label />
                                                    <ProvenanceBadge provenance=j.provenance />
                                                </li>
                                            })
                                            .collect_view()}
                                    </ul>
                                </td>
                                <td class=TD>{r.progress.text()}</td>
                                <td class=TD>{format_duration(r.finished_ms - r.started_ms)}</td>
                            </tr>
                        }
                    })
                    .collect_view()}
            </tbody>
        </table>
    }
}
