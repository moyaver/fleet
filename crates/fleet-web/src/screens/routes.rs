// SPDX-License-Identifier: AGPL-3.0-only
//! The routes of screens C and D: `/workers/:id`, `/devices/:id` and `/flows/:execution_id`, each
//! with `?project=<slug>` because REST reads are per project (see [`crate::paths`]).
//!
//! A route reads its address, fetches with [`crate::api`] through [`remote`] and hands the pages
//! their data. The flow page follows its attempt's live socket, so a running flow updates
//! without a reload; the worker and device pages show what the server held when they opened.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs and the components return views"
)]

use std::time::Duration;

use fleet_protocol::rest::{TimelineView, WorkerRunPage};
use fleet_ui::components::display::EmptyState;
use leptos::{prelude::*, task::spawn_local};
use leptos_router::{
    NavigateOptions,
    hooks::{use_navigate, use_params_map, use_query_map},
};
use uuid::Uuid;

use crate::{
    api,
    clock::{now_ms, utc_offset_minutes},
    live::{
        model::LiveModel,
        socket::{Event, Subscription, connect},
    },
    paths::flow_href,
    remote::{Remote, remote},
    screens::{
        device::view::DevicePage,
        flow::view::FlowPageView,
        metrics::{RangePreset, RangeSelection, query_string_at},
        parts::Fetched,
        timeline::TimelineScope,
        worker::{model::NextPage, view::WorkerPage},
    },
};

/// The project and id of a page address, when both are there and the id is a UUID.
fn parse_address(project: Option<&str>, id: Option<&str>) -> Option<(String, Uuid)> {
    let project = project.filter(|p| !p.is_empty())?;
    Some((project.to_owned(), id?.parse().ok()?))
}

/// The address's `?project=` and `:segment`, read together.
fn address(segment: &'static str) -> impl Fn() -> Option<(String, Uuid)> {
    let params = use_params_map();
    let query = use_query_map();
    move || {
        let project = query.with(|q| q.get("project"));
        let id = params.with(|p| p.get(segment));
        parse_address(project.as_deref(), id.as_deref())
    }
}

/// A clock that ticks once a second while the page is open.
fn ticking_now() -> RwSignal<i64> {
    let now = RwSignal::new(now_ms());
    if let Ok(handle) = set_interval_with_handle(move || now.set(now_ms()), Duration::from_secs(1))
    {
        on_cleanup(move || handle.clear());
    }
    now
}

/// Fetches the metrics for the chosen range now and whenever the range changes.
fn metrics_for<F, Fut>(
    range: RwSignal<RangeSelection>,
    fetch: F,
) -> Remote<fleet_protocol::rest::MetricsView>
where
    F: Fn(String) -> Fut + 'static,
    Fut: Future<Output = Result<fleet_protocol::rest::MetricsView, api::ApiError>> + 'static,
{
    let metrics = remote(1000, move || {
        let query = query_string_at(range.get_untracked(), now_ms())
            .map_err(|e| api::ApiError::Decode(e.to_string()));
        let result = query.map(&fetch);
        async move {
            match result {
                Ok(future) => future.await,
                Err(e) => Err(e),
            }
        }
    });
    let again = metrics.clone();
    Effect::new(move |previous: Option<()>| {
        range.track();
        // The first run is the fetch `remote` already started.
        if previous.is_some() {
            again.request();
        }
    });
    metrics
}

/// Loads the timelines of `attempts` into `into`, one after the other; one that fails is skipped
/// (the timeline section says when there is nothing to draw).
fn load_timelines(project: String, attempts: Vec<Uuid>, into: RwSignal<Vec<TimelineView>>) {
    spawn_local(async move {
        let mut loaded = Vec::new();
        for attempt in attempts {
            if let Ok(view) = api::timeline(&project, attempt).await {
                loaded.push(view);
            }
        }
        into.set(loaded);
    });
}

/// Opens a flow's page without reloading.
fn open_flow(project: String) -> Callback<String> {
    let navigate = use_navigate();
    Callback::new(move |id: String| {
        if let Ok(id) = id.parse::<Uuid>() {
            navigate(&flow_href(&project, id), NavigateOptions::default());
        }
    })
}

fn not_an_address(what: &'static str) -> AnyView {
    view! {
        <EmptyState
            title=format!("Not a {what} address")
            description="The address should name the project, as in ?project=<slug>, and end in the id."
        />
    }
    .into_any()
}

/// Screen C for a worker, at `/workers/:id?project=<slug>`.
#[component]
pub fn WorkerRoute() -> impl IntoView {
    let address = address("id");
    // Rebuilt when the address changes.
    move || match address() {
        Some((project, id)) => view! { <WorkerBody project id /> }.into_any(),
        None => not_an_address("worker"),
    }
}

#[component]
fn WorkerBody(project: String, id: Uuid) -> impl IntoView {
    let worker = {
        let project = project.clone();
        remote(1000, move || {
            let project = project.clone();
            async move { api::worker(&project, id).await }
        })
    };
    let range = RwSignal::new(RangeSelection::Preset(RangePreset::LastHour));
    let metrics = {
        let project = project.clone();
        metrics_for(range, move |query| {
            let project = project.clone();
            async move { api::worker_metrics(&project, id, &query).await }
        })
    };
    let timelines = RwSignal::new(Vec::new());
    {
        let (project, data) = (project.clone(), worker.data);
        Effect::new(move |_| {
            if let Some(w) = data.get() {
                let scope = TimelineScope::of_sessions(&w.devices);
                load_timelines(project.clone(), scope.attempts, timelines);
            }
        });
    }
    let more_runs: RwSignal<Vec<WorkerRunPage>> = RwSignal::new(Vec::new());
    let on_more_runs = {
        let project = project.clone();
        Callback::new(move |next: NextPage| {
            let cursor = match next {
                NextPage::Done => return,
                NextPage::First => None,
                NextPage::After(cursor) => Some(cursor),
            };
            let project = project.clone();
            spawn_local(async move {
                if let Ok(page) = api::worker_runs(&project, id, cursor.as_deref()).await {
                    more_runs.update(|pages| pages.push(page));
                }
            });
        })
    };
    view! {
        <WorkerPage
            worker=Fetched::from(&worker)
            more_runs=more_runs
            on_more_runs=on_more_runs
            metrics=Fetched::from(&metrics)
            range=range
            timelines=timelines
            project=project.clone()
            now_ms=ticking_now()
            utc_offset_minutes=utc_offset_minutes()
            on_open_flow=open_flow(project)
        />
    }
}

/// Screen C for a device session, at `/devices/:id?project=<slug>`.
#[component]
pub fn DeviceRoute() -> impl IntoView {
    let address = address("id");
    move || match address() {
        Some((project, id)) => view! { <DeviceBody project id /> }.into_any(),
        None => not_an_address("device"),
    }
}

#[component]
fn DeviceBody(project: String, id: Uuid) -> impl IntoView {
    let device = {
        let project = project.clone();
        remote(1000, move || {
            let project = project.clone();
            async move { api::device(&project, id).await }
        })
    };
    let range = RwSignal::new(RangeSelection::Preset(RangePreset::LastHour));
    let metrics = {
        let project = project.clone();
        metrics_for(range, move |query| {
            let project = project.clone();
            async move { api::device_metrics(&project, id, &query).await }
        })
    };
    let timelines = RwSignal::new(Vec::new());
    {
        let (project, data) = (project.clone(), device.data);
        Effect::new(move |_| {
            if let Some(d) = data.get() {
                let scope = TimelineScope::of_sessions(std::slice::from_ref(&d.session));
                load_timelines(project.clone(), scope.attempts, timelines);
            }
        });
    }
    view! {
        <DevicePage
            device=Fetched::from(&device)
            metrics=Fetched::from(&metrics)
            range=range
            timelines=timelines
            project=project.clone()
            now_ms=ticking_now()
            utc_offset_minutes=utc_offset_minutes()
            on_open_flow=open_flow(project)
        />
    }
}

/// Screen D for a flow execution, at `/flows/:execution_id?project=<slug>`.
#[component]
pub fn FlowRoute() -> impl IntoView {
    let address = address("execution_id");
    move || match address() {
        Some((project, id)) => view! { <FlowBody project id /> }.into_any(),
        None => not_an_address("flow"),
    }
}

#[component]
fn FlowBody(project: String, id: Uuid) -> impl IntoView {
    let flow = {
        let project = project.clone();
        remote(1000, move || {
            let project = project.clone();
            async move { api::flow(&project, id).await }
        })
    };
    // Follow the flow's attempt over the live socket: any change asks for the detail again (the
    // fetch is throttled), so a running flow's steps, claims and outcome update by themselves.
    let attempt = {
        let data = flow.data;
        Memo::new(move |_| data.get().map(|f| f.attempt.id))
    };
    let follower = StoredValue::new_local(flow.clone());
    let live_project = project.clone();
    view! {
        {move || {
            attempt
                .get()
                .map(|attempt| {
                    view! { <FlowFollower project=live_project.clone() attempt flow=follower /> }
                })
        }}
        <FlowPageView
            flow=Fetched::from(&flow)
            project=project
            now_ms=ticking_now()
            utc_offset_minutes=utc_offset_minutes()
        />
    }
}

/// Keeps a socket on the attempt open while the flow page is, and refetches on every change.
#[component]
fn FlowFollower(
    project: String,
    attempt: Uuid,
    flow: StoredValue<Remote<fleet_protocol::rest::FlowDetailView>, LocalStorage>,
) -> impl IntoView {
    let model = LiveModel::new(attempt);
    let _live = connect(
        Subscription {
            project: project.clone(),
            attempt: Some(attempt),
        },
        move |event| match event {
            Event::Frames(frames) => model.apply(&project, frames),
            Event::Lost => model.connection_lost(),
        },
    );
    Effect::new(move |_| {
        if model.version.get() > 0 {
            flow.with_value(Remote::request);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_needs_a_project_and_a_uuid() {
        let id = "00000000-0000-0000-0000-0000000000a1";
        assert_eq!(
            parse_address(Some("app"), Some(id)),
            Some(("app".to_owned(), id.parse().unwrap()))
        );
        assert_eq!(parse_address(None, Some(id)), None, "no ?project=");
        assert_eq!(parse_address(Some(""), Some(id)), None, "empty project");
        assert_eq!(parse_address(Some("app"), Some("not-a-uuid")), None);
        assert_eq!(parse_address(Some("app"), None), None);
    }
}
