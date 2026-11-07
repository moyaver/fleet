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
use fleet_ui::components::{display::EmptyState, live::LiveState};
use leptos::{prelude::*, task::spawn_local};
use leptos_router::{
    NavigateOptions,
    hooks::{use_navigate, use_params_map, use_query_map},
};
use uuid::Uuid;

use crate::{
    api,
    app::{LiveBar, LiveSlot},
    clock::{now_ms, utc_offset_minutes},
    format::time_of_day,
    live::{
        model::LiveModel,
        socket::{Connection, Event, Subscription, connect},
    },
    paths::flow_href,
    remote::{Remote, remote},
    screens::{
        device::view::DevicePage,
        flow::{
            evidence::{Evidence, Load, LogSource, TextFile, refresh_in_ms},
            evidence_view::EvidenceFeed,
            model::{evidence_steps, is_settled, live_state},
            view::FlowPageView,
        },
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

/// Follows `project`'s live socket while the page is open. Any frame bumps `tick`, which asks the
/// page for fresh data (the requests are throttled), and the top bar's indicator shows how the feed
/// stands. Pause holds the frames as they arrive; resuming bumps `tick` once if any came.
fn follow_project(project: String, tick: RwSignal<u64>, now: RwSignal<i64>) {
    let paused = RwSignal::new(false);
    let held = RwSignal::new(0_usize);
    let last_update = RwSignal::new(now_ms());
    let socket = connect(
        Subscription {
            project,
            attempt: None,
        },
        move |event| {
            if let Event::Frames(frames) = event {
                last_update.set(now_ms());
                if paused.get_untracked() {
                    held.update(|n| *n += frames.len());
                } else {
                    tick.update(|n| *n += 1);
                }
            }
        },
    );
    let bar = LiveBar {
        state: RwSignal::new(LiveState::Connecting),
        detail: RwSignal::new(String::new()),
        action: RwSignal::new(None),
        on_action: Callback::new(move |()| {
            if paused.get_untracked() {
                paused.set(false);
                if held.get_untracked() > 0 {
                    held.set(0);
                    tick.update(|n| *n += 1);
                }
            } else {
                paused.set(true);
            }
        }),
    };
    Effect::new(move |_| {
        let at = now.get();
        let offset = utc_offset_minutes();
        let (state, detail, action) = match (socket.connection.get(), paused.get(), held.get()) {
            (Connection::Connecting, _, _) => (LiveState::Connecting, String::new(), None),
            (Connection::Reconnecting, _, _) => (
                LiveState::Reconnecting,
                format!(
                    "showing data from {}",
                    time_of_day(last_update.get(), offset)
                ),
                None,
            ),
            (Connection::Live, true, 0) => (
                LiveState::Paused,
                String::new(),
                Some("Resume updates".to_owned()),
            ),
            (Connection::Live, true, n) => (
                LiveState::Paused,
                String::new(),
                Some(format!("Show {n} updates")),
            ),
            (Connection::Live, false, _) => (
                LiveState::Live,
                format!("updated {} s ago", ((at - last_update.get()) / 1000).max(0)),
                Some("Pause".to_owned()),
            ),
        };
        bar.state.set(state);
        bar.detail.set(detail);
        bar.action.set(action);
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
    // Any change on the project asks for the runner's data again, and its charts.
    let now = ticking_now();
    let tick = RwSignal::new(0_u64);
    {
        let (worker, metrics) = (worker.clone(), metrics.clone());
        Effect::new(move |_| {
            // The first run is the fetch `remote` already started.
            if tick.get() > 0 {
                worker.request();
                metrics.request();
            }
        });
    }
    follow_project(project.clone(), tick, now);
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
            now_ms=now
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
    let now = ticking_now();
    let tick = RwSignal::new(0_u64);
    {
        let (device, metrics) = (device.clone(), metrics.clone());
        Effect::new(move |_| {
            // The first run is the fetch `remote` already started.
            if tick.get() > 0 {
                device.request();
                metrics.request();
            }
        });
    }
    follow_project(project.clone(), tick, now);
    view! {
        <DevicePage
            device=Fetched::from(&device)
            metrics=Fetched::from(&metrics)
            range=range
            project=project
            now_ms=now
            utc_offset_minutes=utc_offset_minutes()
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
    let feed = evidence_feed(&project, id, &flow);
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
            evidence=feed
        />
    }
}

/// Fetches a flow's evidence: the artifact list (again when the flow's state or its set of
/// artifacts changes, and a minute before its signed links lapse) and, from the usercontent
/// origin, the Maestro log and `commands.json` (once per file, not once per link).
fn evidence_feed(
    project: &str,
    id: Uuid,
    flow: &Remote<fleet_protocol::rest::FlowDetailView>,
) -> EvidenceFeed {
    let artifacts = {
        let project = project.to_owned();
        remote(1000, move || {
            let project = project.clone();
            async move { api::flow_artifacts(&project, id).await }
        })
    };
    let flow_data = flow.data;
    let flow_changed = Memo::new(move |_| {
        flow_data.with(|f| {
            f.as_ref().map(|f| {
                (
                    f.flow.state.clone(),
                    f.artifacts.iter().map(|a| a.id).collect::<Vec<_>>(),
                )
            })
        })
    });
    {
        let again = artifacts.clone();
        Effect::new(move |previous: Option<()>| {
            flow_changed.track();
            // The first run is the fetch `remote` already started.
            if previous.is_some() {
                again.request();
            }
        });
    }
    // Fresh links before the current ones lapse, and soon again when a refresh fails.
    {
        let (again, data, error) = (artifacts.clone(), artifacts.data, artifacts.error);
        Effect::new(move |timer: Option<Option<TimeoutHandle>>| {
            if let Some(Some(handle)) = timer {
                handle.clear();
            }
            let wait =
                data.with(|l| error.with(|e| refresh_in_ms(l.as_ref(), e.as_ref(), now_ms())))?;
            let again = again.clone();
            set_timeout_with_handle(
                move || again.request(),
                Duration::from_millis(u64::try_from(wait).unwrap_or(1000)),
            )
            .ok()
        });
    }
    let (list, list_error) = (artifacts.data, artifacts.error);
    let evidence = Memo::new(move |_| {
        let Some(list) = list.get() else {
            return match list_error.get() {
                Some(error) => Evidence::Unavailable(error.to_string()),
                None => Evidence::Loading,
            };
        };
        let Some(detail) = flow_data.get() else {
            return Evidence::Loading;
        };
        let (steps, failed) = evidence_steps(&detail);
        Evidence::build(&list, id, failed.as_ref(), &steps)
    });
    let log = text_file(Memo::new(move |_| match evidence.get() {
        Evidence::Files(f) => match f.log {
            Some(LogSource::Show(file)) => Some(file),
            _ => None,
        },
        _ => None,
    }));
    let logcat = text_file(Memo::new(move |_| match evidence.get() {
        Evidence::Files(f) => match f.logcat {
            Some(LogSource::Show(file)) => Some(file),
            _ => None,
        },
        _ => None,
    }));
    let commands = text_file(Memo::new(move |_| match evidence.get() {
        Evidence::Files(f) => f.commands,
        _ => None,
    }));
    EvidenceFeed {
        evidence,
        log,
        logcat,
        commands,
        selected_step: RwSignal::new(None),
    }
}

/// Fetches `file` when it first appears and again only when it is a different artifact; a renewed
/// link to the same file does not reload it.
fn text_file(file: Memo<Option<TextFile>>) -> RwSignal<Load<String>> {
    let text = RwSignal::new(Load::Loading);
    let which = Memo::new(move |_| file.with(|f| f.as_ref().map(|f| f.id)));
    Effect::new(move |_| {
        if which.get().is_none() {
            return;
        }
        let Some(url) = file.with_untracked(|f| f.as_ref().map(|f| f.url.clone())) else {
            return;
        };
        text.set(Load::Loading);
        spawn_local(async move {
            text.set(match api::fetch_text(&url).await {
                Ok(body) => Load::Ready(body),
                Err(error) => Load::Failed(error.to_string()),
            });
        });
    });
    text
}

/// Keeps a socket on the attempt open while the flow page is, refetches on every change, and lends
/// the top bar's live indicator: Settled once the outcome is final, otherwise the socket's state
/// and the time since its last frame.
#[component]
fn FlowFollower(
    project: String,
    attempt: Uuid,
    flow: StoredValue<Remote<fleet_protocol::rest::FlowDetailView>, LocalStorage>,
) -> impl IntoView {
    let model = LiveModel::new(attempt);
    let last_frame = RwSignal::new(now_ms());
    let live = connect(
        Subscription {
            project: project.clone(),
            attempt: Some(attempt),
        },
        move |event| match event {
            Event::Frames(frames) => {
                last_frame.set(now_ms());
                model.apply(&project, frames);
            }
            Event::Lost => model.connection_lost(),
        },
    );
    Effect::new(move |_| {
        if model.version.get() > 0 {
            flow.with_value(Remote::request);
        }
    });
    let data = flow.with_value(|f| f.data);
    let now = RwSignal::new(now_ms());
    if let Ok(handle) = set_interval_with_handle(move || now.set(now_ms()), Duration::from_secs(1))
    {
        on_cleanup(move || handle.clear());
    }
    let bar = LiveBar {
        state: RwSignal::new(LiveState::Connecting),
        detail: RwSignal::new(String::new()),
        action: RwSignal::new(None),
        on_action: Callback::new(|()| {}),
    };
    Effect::new(move |_| {
        let settled = data.with(|d| d.as_ref().is_some_and(is_settled));
        let state = live_state(live.connection.get(), settled);
        bar.state.set(state);
        bar.detail.set(if state == LiveState::Live {
            let ago = (now.get() - last_frame.get()) / 1000;
            format!("updated {} s ago", ago.max(0))
        } else {
            String::new()
        });
    });
    let slot = expect_context::<LiveSlot>().0;
    slot.set(Some(bar));
    // Clears the slot only if it still holds this page's bar (see the pipeline page).
    let mine = bar.state;
    on_cleanup(move || {
        slot.update(|current| {
            if current.is_some_and(|b| b.state == mine) {
                *current = None;
            }
        });
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
