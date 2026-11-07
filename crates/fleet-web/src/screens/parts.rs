// SPDX-License-Identifier: AGPL-3.0-only
//! Leptos pieces shared by the pages of screens C and D, built from the `fleet-ui` kit. All user
//! and agent text is rendered as text nodes, never as HTML (PLAN §10.1).

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs"
)]

use fleet_protocol::rest::TimelineView;
use fleet_ui::{
    charts::{series::Series, view::TimeSeriesChart},
    components::{
        display::{Card, EmptyState, ProvenanceBadge, Skeleton},
        forms::DateTimeRange,
    },
    provenance::Provenance,
    timeline::view::SwimlaneTimeline,
    tone::Tone,
};
use leptos::prelude::*;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    api::ApiError,
    remote::Remote,
    screens::{
        common::Fact,
        metrics::{ChartBody, MetricChart, MetricsPanel, RangePreset, RangeSelection},
        timeline::{TimelineScope, timeline_data},
    },
    status::Pill,
};

/// What a page reads of a fetch: the data (kept when a refetch fails) and the last error. It
/// is `Copy`, so closures can capture it freely; the route keeps the [`Remote`] that refreshes it.
#[derive(Debug)]
pub struct Fetched<T: Send + Sync + 'static> {
    /// The last data received.
    pub data: RwSignal<Option<T>>,
    /// Why the last fetch failed.
    pub error: RwSignal<Option<ApiError>>,
}

impl<T: Send + Sync + 'static> Clone for Fetched<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Send + Sync + 'static> Copy for Fetched<T> {}

impl<T: Send + Sync + 'static> From<&Remote<T>> for Fetched<T> {
    fn from(remote: &Remote<T>) -> Self {
        Self {
            data: remote.data,
            error: remote.error,
        }
    }
}

/// A skeleton until the first answer, nothing extra when it failed (the page shows an
/// [`ErrorNotice`](crate::app::ErrorNotice) beside it), and `ready` with the data once there is
/// some. Data that is already on screen stays when a refetch fails.
pub fn loaded<T: Clone + Send + Sync + 'static>(
    fetched: Fetched<T>,
    ready: impl FnOnce(T) -> AnyView,
) -> AnyView {
    match fetched.data.get() {
        Some(value) => ready(value),
        None if fetched.error.get().is_some() => ().into_any(),
        None => view! { <Skeleton lines=4 /> }.into_any(),
    }
}

/// A status word with its colour role.
#[component]
pub fn PillView(
    /// What to show.
    pill: Pill,
) -> impl IntoView {
    view! { <fleet_ui::components::display::StatusPill mark=pill.mark label=pill.label /> }
}

/// Labelled facts as a description list. A fact the server doesn't have reads "Not reported".
#[component]
pub fn FactList(
    /// The facts, in order.
    facts: Vec<Fact>,
) -> impl IntoView {
    view! {
        <dl class="grid grid-cols-[max-content_1fr] gap-x-6 gap-y-1 text-sm">
            {facts
                .into_iter()
                .map(|f| {
                    let missing = f.value.is_none();
                    view! {
                        <dt class="text-muted">{f.label}</dt>
                        <dd class=if missing { "italic text-muted" } else { "" }>{f.shown().to_owned()}</dd>
                    }
                })
                .collect_view()}
        </dl>
    }
}

/// A message that needs attention, in a colour role and in words.
#[component]
pub fn Notice(
    /// The colour role.
    tone: Tone,
    /// The text.
    #[prop(into)]
    text: String,
) -> impl IntoView {
    view! {
        <p role="status" class=format!("rounded border px-3 py-2 text-sm {}", tone.class())>{text}</p>
    }
}

/// A provenance badge with its finality in words beside it.
#[component]
pub fn Outcome(
    /// The state of the flow.
    pill: Pill,
    /// What it rests on.
    provenance: Provenance,
    /// How settled it is.
    #[prop(optional)]
    finality: Option<&'static str>,
) -> impl IntoView {
    view! {
        <span class="inline-flex flex-wrap items-center gap-2" title=finality>
            <PillView pill=pill />
            <ProvenanceBadge provenance=provenance />
        </span>
    }
}

/// The range picker: presets, and a custom range with native date and time inputs.
#[component]
pub fn RangePicker(
    /// The selection; the page requests metrics whenever it changes.
    range: RwSignal<RangeSelection>,
) -> impl IntoView {
    let parse = |s: Option<String>| s.and_then(|s| OffsetDateTime::parse(&s, &Rfc3339).ok());
    view! {
        <div class="flex flex-wrap items-end gap-4">
            <fieldset class="flex flex-wrap items-center gap-2">
                <legend class="mb-1 text-sm font-medium">"Range"</legend>
                {RangePreset::ALL
                    .into_iter()
                    .map(|p| {
                        let selected = move || range.get() == RangeSelection::Preset(p);
                        view! {
                            <button
                                type="button"
                                class="rounded border border-border bg-surface px-2 py-1 text-sm aria-pressed:bg-primary aria-pressed:text-primary-fg"
                                aria-pressed=move || selected().to_string()
                                on:click=move |_| range.set(RangeSelection::Preset(p))
                            >
                                {p.label()}
                            </button>
                        }
                    })
                    .collect_view()}
            </fieldset>
            <DateTimeRange
                id="metrics-range"
                on_change=Callback::new(move |(from, to): (Option<String>, Option<String>)| {
                    if let (Some(from), Some(to)) = (parse(from), parse(to)) {
                        range.set(RangeSelection::Custom { from, to });
                    }
                })
            />
        </div>
    }
}

/// One chart: title and unit, an aggregate note, then the drawing, or "unavailable (reason)".
#[component]
fn MetricChartView(
    /// The chart.
    chart: MetricChart,
    /// The shared x-axis (unix ms).
    domain_ms: (i64, i64),
    /// The shared cursor.
    cursor: RwSignal<Option<i64>>,
    /// The viewer's offset from UTC in minutes.
    utc_offset_minutes: i32,
) -> impl IntoView {
    let MetricChart {
        id,
        title,
        unit_symbol,
        aggregate_note,
        body,
    } = chart;
    let body = match body {
        ChartBody::Series(series) => {
            let series: Series = series;
            view! {
                <TimeSeriesChart
                    series=Signal::derive(move || series.clone())
                    domain_ms=domain_ms
                    utc_offset_minutes=utc_offset_minutes
                    cursor=cursor
                />
            }
            .into_any()
        }
        ChartBody::Unavailable { text } => view! {
            <p class="rounded border border-dashed border-border p-4 italic text-muted" data-unavailable>
                {text}
            </p>
        }
        .into_any(),
    };
    view! {
        <figure class="space-y-1" data-testid="metric-chart" data-metric=id>
            <figcaption class="flex flex-wrap items-baseline gap-2 text-sm">
                <span class="font-medium">{title}</span>
                <span class="text-muted">{format!("({unit_symbol})")}</span>
                {aggregate_note.map(|n| view! { <span class="text-muted">{n}</span> })}
            </figcaption>
            {body}
        </figure>
    }
}

/// Screen C's correlated charts: grouped by scope, one range, one cursor.
#[component]
pub fn MetricsCharts(
    /// The laid-out metrics.
    panel: MetricsPanel,
    /// The shared cursor (unix ms), also given to the timeline.
    cursor: RwSignal<Option<i64>>,
    /// The viewer's offset from UTC in minutes.
    utc_offset_minutes: i32,
) -> impl IntoView {
    let MetricsPanel {
        sections,
        domain_ms,
        resolution_note,
        truncated_note,
    } = panel;
    view! {
        <div class="space-y-4">
            <p class="text-sm text-muted">{resolution_note}</p>
            {truncated_note.map(|t| view! { <Notice tone=Tone::Warning text=t /> })}
            {if sections.is_empty() {
                view! { <EmptyState title="No metrics" description="The server returned no series for this range." /> }
                    .into_any()
            } else {
                sections
                    .into_iter()
                    .map(|section| {
                        view! {
                            <Card title=section.heading>
                                <div class="grid gap-4 lg:grid-cols-2">
                                    {section
                                        .charts
                                        .into_iter()
                                        .map(|chart| {
                                            view! {
                                                <MetricChartView
                                                    chart=chart
                                                    domain_ms=domain_ms
                                                    cursor=cursor
                                                    utc_offset_minutes=utc_offset_minutes
                                                />
                                            }
                                        })
                                        .collect_view()}
                                </div>
                            </Card>
                        }
                    })
                    .collect_view()
                    .into_any()
            }}
        </div>
    }
}

/// The device timeline, sharing its cursor with the charts.
#[component]
pub fn TimelineSection(
    /// The devices to show.
    scope: TimelineScope,
    /// Loaded timelines.
    timelines: Signal<Vec<TimelineView>>,
    /// The current time.
    now_ms: Signal<i64>,
    /// The viewer's offset from UTC.
    utc_offset_minutes: i32,
    /// The shared cursor.
    cursor: RwSignal<Option<i64>>,
    /// Open a flow.
    on_open_flow: Callback<String>,
) -> impl IntoView {
    let data = Memo::new(move |_| timeline_data(&timelines.read(), &scope.lanes));
    view! {
        <section aria-labelledby="timeline-title" class="space-y-2">
            <h3 id="timeline-title" class="text-lg font-semibold">"Flows on these devices"</h3>
            <p class="text-sm text-muted">
                "Move over a chart or a flow to see the same moment on both."
            </p>
            {move || {
                if data.read().bounds(0).is_none() {
                    view! {
                        <EmptyState
                            title="No flows to show"
                            description="No flow has started on these devices, or their timelines are not loaded yet."
                        />
                    }
                        .into_any()
                } else {
                    view! {
                        <SwimlaneTimeline
                            data=data
                            now_ms=now_ms
                            utc_offset_minutes=utc_offset_minutes
                            cursor=cursor
                            on_select=on_open_flow
                        />
                    }
                        .into_any()
                }
            }}
        </section>
    }
}
