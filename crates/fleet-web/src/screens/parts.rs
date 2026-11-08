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
        display::{EmptyState, ProvenanceBadge, RowProvenance, Skeleton},
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
        metrics::{ChartBody, Latest, MetricChart, MetricsPanel, RangePreset, RangeSelection},
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
            <RowProvenance provenance=provenance />
        </span>
    }
}

/// The range picker: presets as radio buttons, and a custom range with native date and time
/// inputs. The radios are the page's range; a change requests the metrics again.
#[component]
pub fn RangePicker(
    /// The selection; the page requests metrics whenever it changes.
    range: RwSignal<RangeSelection>,
) -> impl IntoView {
    let parse = |s: Option<String>| s.and_then(|s| OffsetDateTime::parse(&s, &Rfc3339).ok());
    view! {
        <div class="flex flex-wrap items-end gap-4">
            <fieldset class="flex flex-wrap items-center gap-x-4 gap-y-2">
                <legend class="mb-1 text-sm font-medium">"Range"</legend>
                {RangePreset::ALL
                    .into_iter()
                    .map(|p| {
                        let checked = move || range.get() == RangeSelection::Preset(p);
                        view! {
                            <label class="inline-flex min-h-8 items-center gap-2 text-sm">
                                <input
                                    type="radio"
                                    name="metrics-range"
                                    value=p.label()
                                    prop:checked=checked
                                    on:change=move |_| range.set(RangeSelection::Preset(p))
                                />
                                {p.label()}
                            </label>
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

/// One chart: its title and unit, the newest reading in its header, then the drawing, or the
/// word Missing with the reason when the range has no value at all.
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
        body,
        latest,
        ..
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
        ChartBody::Unavailable { reason } => view! {
            <p class="flex flex-wrap items-center gap-2 py-6 text-sm text-muted" data-unavailable>
                <ProvenanceBadge provenance=Signal::stored(Provenance::Missing) />
                <span>{reason.unwrap_or_else(|| "no reason given".to_owned())}</span>
            </p>
        }
        .into_any(),
    };
    let now = match latest {
        Some(Latest::Value(v)) => view! {
            <span class="tabular-nums">{v}<span class="note">" now"</span></span>
        }
        .into_any(),
        Some(Latest::Missing(reason)) => view! {
            <span class="flex flex-wrap items-center gap-2">
                <ProvenanceBadge provenance=Signal::stored(Provenance::Missing) />
                {reason.map(|r| view! { <span class="note">{r}</span> })}
            </span>
        }
        .into_any(),
        None => ().into_any(),
    };
    view! {
        <figure class="space-y-2" data-testid="metric-chart" data-metric=id>
            <figcaption class="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1 text-sm">
                <span class="font-medium">
                    {title}
                    " "
                    <span class="font-normal text-muted">{format!("({unit_symbol})")}</span>
                </span>
                {now}
            </figcaption>
            {body}
        </figure>
    }
}

/// Screen C's correlated charts, grouped by scope under a small heading each: one range, one
/// cursor. The section that holds them states its resolution once, in the page.
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
        truncated_note,
        ..
    } = panel;
    view! {
        <div class="space-y-6">
            {truncated_note.map(|t| view! { <Notice tone=Tone::Warning text=t /> })}
            {if sections.is_empty() {
                view! { <EmptyState title="No metrics" description="The server returned no series for this range." /> }
                    .into_any()
            } else {
                sections
                    .into_iter()
                    .map(|section| {
                        view! {
                            <div class="space-y-3">
                                <h3 class="text-sm font-medium text-muted">{section.heading}</h3>
                                <div class="grid gap-6 md:grid-cols-2">
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
                            </div>
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
