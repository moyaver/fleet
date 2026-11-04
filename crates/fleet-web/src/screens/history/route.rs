// SPDX-License-Identifier: AGPL-3.0-only
//! The `/history` route: reads the filters from the URL, fetches with [`crate::api`] and hands the
//! view-model's pieces to the views. Every filter edit rewrites the URL (without a reload) and
//! asks again, so a reload or a shared link shows the same history.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs and the components return views"
)]

use fleet_protocol::rest::history::HistoryPage;
use fleet_ui::components::{display::Skeleton, forms::use_url_query};
use leptos::{prelude::*, task::spawn_local};

use crate::{
    api,
    app::ErrorNotice,
    clock::{now_ms, utc_offset_minutes},
    paths::with_query_param,
    remote::remote,
    screens::{
        common::Ctx,
        history::{
            model::{HistoryQuery, HistoryTable, STATS_PARAM, StatsPanel},
            view::{HistoryFilters, HistoryTableView, StatsView},
        },
        parts::{Fetched, loaded},
        project::InProject,
    },
};

/// Screen E, at `/history?project=<slug>` and the filters as further parameters.
#[component]
pub fn HistoryRoute() -> impl IntoView {
    view! {
        <div class="space-y-4" data-testid="history-page">
            <h2 class="text-xl font-semibold">"Test history"</h2>
            <InProject
                path="/history"
                body=Callback::new(|project: String| view! { <HistoryBody project /> }.into_any())
            />
        </div>
    }
}

fn ctx(project: &str) -> Ctx {
    Ctx {
        project: project.to_owned(),
        now_ms: now_ms(),
        utc_offset_minutes: utc_offset_minutes(),
    }
}

#[component]
fn HistoryBody(project: String) -> impl IntoView {
    let (query, set_query) = use_url_query();
    let parsed = Memo::new(move |_| HistoryQuery::parse(&query.get()));
    // Only what the server is asked changes the request: opening a stats panel does not.
    let rest = Memo::new(move |_| parsed.with(|p| p.rest_query(None)));
    let history = {
        let project = project.clone();
        remote(1000, move || {
            let project = project.clone();
            let q = rest.get_untracked();
            async move { api::history(&project, &q).await }
        })
    };
    let more: RwSignal<Vec<HistoryPage>> = RwSignal::new(Vec::new());
    {
        let history = history.clone();
        Effect::new(move |previous: Option<()>| {
            rest.track();
            // The first run is the fetch `remote` already started.
            if previous.is_some() {
                more.set(Vec::new());
                history.request();
            }
        });
    }
    let loading_more = RwSignal::new(false);
    let on_more = {
        let project = project.clone();
        Callback::new(move |cursor: String| {
            if loading_more.get_untracked() {
                return;
            }
            loading_more.set(true);
            let project = project.clone();
            let q = parsed.with_untracked(|p| p.rest_query(Some(&cursor)));
            spawn_local(async move {
                if let Ok(page) = api::history(&project, &q).await {
                    more.update(|pages| pages.push(page));
                }
                loading_more.set(false);
            });
        })
    };
    let table = {
        let project = project.clone();
        Memo::new(move |_| {
            history.data.with(|first| {
                first.as_ref().map(|first| {
                    more.with(|more| HistoryTable::build(first, more, &ctx(&project), &query.get()))
                })
            })
        })
    };
    let history_fetched = Fetched::from(&history);
    let stats_flow = Memo::new(move |_| parsed.with(|p| p.stats.clone()));
    let window = Memo::new(move |_| parsed.with(HistoryQuery::window_query));
    let on_stats = Callback::new(move |q: String| set_query.run(q));
    let on_close = Callback::new(move |()| {
        set_query.run(with_query_param(&query.get_untracked(), STATS_PARAM, None));
    });
    view! {
        <HistoryFilters query=query on_change=set_query />
        <ErrorNotice error=history_fetched.error />
        {move || {
            let table = table.get();
            match table {
                Some(t) => view! { <HistoryTableView table=t on_more=on_more on_stats=on_stats /> }.into_any(),
                None if history_fetched.error.with(Option::is_some) => ().into_any(),
                None => view! { <Skeleton lines=6 /> }.into_any(),
            }
        }}
        {
            let project = project.clone();
            move || {
                let project = project.clone();
                stats_flow
                    .get()
                    .map(|flow| view! { <StatsBody project flow window on_close /> })
            }
        }
    }
}

#[component]
fn StatsBody(
    project: String,
    flow: String,
    window: Memo<String>,
    on_close: Callback<()>,
) -> impl IntoView {
    let stats = {
        let (project, flow) = (project.clone(), flow.clone());
        remote(1000, move || {
            let (project, flow) = (project.clone(), flow.clone());
            let q = window.get_untracked();
            async move { api::flow_stats(&project, &flow, &q).await }
        })
    };
    {
        let stats = stats.clone();
        Effect::new(move |previous: Option<()>| {
            window.track();
            if previous.is_some() {
                stats.request();
            }
        });
    }
    let fetched = Fetched::from(&stats);
    let utc = utc_offset_minutes();
    view! {
        <ErrorNotice error=fetched.error />
        {move || {
            let project = project.clone();
            loaded(
                fetched,
                move |view| {
                    let panel = StatsPanel::build(&view, &ctx(&project));
                    view! { <StatsView panel utc_offset_minutes=utc on_close /> }.into_any()
                },
            )
        }}
    }
}
