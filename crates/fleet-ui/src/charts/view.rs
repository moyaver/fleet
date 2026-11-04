// SPDX-License-Identifier: AGPL-3.0-only
//! The Leptos views of the charts: thin wrappers over [`SparklineModel`] and
//! [`TimeSeriesModel`]. All text is rendered as text nodes. Colours come from the theme's
//! Tailwind utilities (SVG) and CSS custom properties (canvas), so light and dark themes work.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs"
)]

use leptos::prelude::*;
use leptos::wasm_bindgen::JsCast;
use web_sys::{Element, HtmlCanvasElement};

use crate::canvas;
use crate::charts::series::Series;
use crate::charts::sparkline::SparklineModel;
use crate::charts::time_series::{ChartConfig, Crosshair, Renderer, TimeSeriesModel};
use crate::virtual_list::window;

/// A small inline line chart with the latest value beside it. An unavailable stretch is a
/// hatched gap; an unavailable latest reading reads "unavailable", never the last good value.
#[component]
pub fn Sparkline(
    /// The readings to draw.
    #[prop(into)]
    series: Signal<Series>,
    /// Width of the line in pixels.
    #[prop(default = 96.0)]
    width: f64,
    /// Height of the line in pixels.
    #[prop(default = 24.0)]
    height: f64,
) -> impl IntoView {
    let model = Memo::new(move |_| SparklineModel::build(&series.read(), width, height));
    view! {
        <span class="inline-flex items-center gap-2">
            <svg
                viewBox=format!("0 0 {width} {height}")
                width=width
                height=height
                role="img"
                aria-label=move || model.read().description.clone()
                class="overflow-visible"
            >
                {move || {
                    model
                        .read()
                        .gaps
                        .iter()
                        .map(|g| {
                            view! {
                                <rect
                                    x=g.x0
                                    y="0"
                                    width=(g.x1 - g.x0).max(1.5)
                                    height=height
                                    class="fill-tone-muted-bg"
                                >
                                    <title>{g.annotation.clone()}</title>
                                </rect>
                            }
                        })
                        .collect_view()
                }}
                {move || {
                    model
                        .read()
                        .segments
                        .iter()
                        .map(|s| {
                            view! {
                                <path
                                    d=s.path_d()
                                    fill="none"
                                    stroke-width="1.5"
                                    stroke-linecap="round"
                                    stroke-linejoin="round"
                                    class="stroke-tone-info-fg"
                                />
                            }
                        })
                        .collect_view()
                }}
            </svg>
            <span class="text-xs tabular-nums text-muted">{move || model.read().latest_label.clone()}</span>
        </span>
    }
}

/// A time-series chart: SVG up to about 5 000 readings, a canvas above (axes and annotations
/// stay SVG). Unavailable readings are hatched, annotated gaps. Hovering moves a crosshair, and
/// `cursor` shares its time with other charts and the timeline. A button switches to an
/// equivalent data table.
#[component]
pub fn TimeSeriesChart(
    /// The readings to draw, with the metric's name and unit.
    #[prop(into)]
    series: Signal<Series>,
    /// Width of the drawing in pixels; it scales down with its container.
    #[prop(default = 640.0)]
    width: f64,
    /// Height of the drawing in pixels.
    #[prop(default = 240.0)]
    height: f64,
    /// The visible time range (unix ms); the series' own range when unset.
    #[prop(optional, into)]
    domain_ms: MaybeProp<(i64, i64)>,
    /// The viewer's offset from UTC in minutes, for time labels.
    #[prop(default = 0)]
    utc_offset_minutes: i32,
    /// A cursor time (unix ms) shared with other charts and the timeline.
    #[prop(optional)]
    cursor: Option<RwSignal<Option<i64>>>,
) -> impl IntoView {
    let model = Memo::new(move |_| {
        TimeSeriesModel::build(
            series.get(),
            ChartConfig {
                width,
                height,
                utc_offset_minutes,
                domain_ms: domain_ms.get(),
                ..ChartConfig::default()
            },
        )
    });
    let cursor = cursor.unwrap_or_else(|| RwSignal::new(None));
    let readout = Memo::new(move |_| cursor.get().and_then(|t| model.read().crosshair_at_time(t)));
    let table_open = RwSignal::new(false);
    let canvas_ref = NodeRef::<leptos::html::Canvas>::new();

    Effect::new(move |_| {
        let m = model.get();
        if let Some(canvas) = canvas_ref.get() {
            draw_lines(&canvas, &m, width, height);
        }
    });

    let on_move = move |ev: web_sys::PointerEvent| {
        let Some(svg) = ev
            .current_target()
            .and_then(|t| t.dyn_into::<Element>().ok())
        else {
            return;
        };
        let rect = svg.get_bounding_client_rect();
        if rect.width() <= 0.0 {
            return;
        }
        let x = (f64::from(ev.client_x()) - rect.left()) * width / rect.width();
        let m = model.read();
        cursor.set((x >= m.plot.left && x <= m.plot.right).then(|| m.time_for_x(x)));
    };

    view! {
        <figure class="space-y-1">
            <div class="flex items-center justify-between gap-2">
                <figcaption class="text-sm font-medium text-fg">{move || model.read().y_axis.label.clone()}</figcaption>
                <button
                    type="button"
                    class="rounded border border-border px-2 py-0.5 text-xs text-fg hover:bg-tone-neutral-bg focus-visible:outline-2 focus-visible:outline-focus"
                    aria-pressed=move || table_open.get().to_string()
                    on:click=move |_| table_open.update(|open| *open = !*open)
                >
                    {move || if table_open.get() { "Show chart" } else { "Show data table" }}
                </button>
            </div>
            <Show
                when=move || !table_open.get()
                fallback=move || view! { <DataTable model=model /> }
            >
                <div class="relative">
                    <Show when=move || model.read().renderer == Renderer::Canvas>
                        <canvas
                            node_ref=canvas_ref
                            class="pointer-events-none absolute inset-0 h-full w-full"
                            aria-hidden="true"
                        ></canvas>
                    </Show>
                    <svg
                        viewBox=format!("0 0 {width} {height}")
                        class="relative block h-auto w-full touch-none select-none"
                        role="img"
                        aria-label=move || model.read().summary()
                        on:pointermove=on_move
                        on:pointerleave=move |_| cursor.set(None)
                    >
                        <defs>
                            <pattern
                                id="fleet-gap-hatch"
                                width="6"
                                height="6"
                                patternUnits="userSpaceOnUse"
                                patternTransform="rotate(45)"
                            >
                                <line x1="0" y1="0" x2="0" y2="6" stroke-width="2" class="stroke-tone-muted-fg" opacity="0.5"></line>
                            </pattern>
                        </defs>
                        <Axes model=model />
                        <GapBands model=model />
                        <Lines model=model />
                        <CrosshairMark model=model readout=readout />
                    </svg>
                </div>
                <p class="min-h-5 text-xs tabular-nums text-muted">
                    {move || match readout.get() {
                        Some(c) => format!("{} · {}", c.time_label, c.value_label),
                        None => "Hover the chart for exact values.".to_owned(),
                    }}
                </p>
            </Show>
        </figure>
    }
}

/// Grid lines, y-axis tick labels and x-axis tick labels.
#[component]
fn Axes(model: Memo<TimeSeriesModel>) -> impl IntoView {
    view! {
        {move || {
            let m = model.read();
            let (left, right) = (m.plot.left, m.plot.right);
            m.y_axis
                .ticks
                .iter()
                .map(|t| {
                    view! {
                        <g>
                            <line x1=left x2=right y1=t.y y2=t.y stroke-width="1" class="stroke-border"></line>
                            <text x=left - 6.0 y=t.y dy="0.32em" text-anchor="end" font-size="10" class="fill-muted">
                                {t.label.clone()}
                            </text>
                        </g>
                    }
                })
                .collect_view()
        }}
        {move || {
            let m = model.read();
            let y = m.plot.bottom + 14.0;
            m.x_ticks
                .iter()
                .map(|t| {
                    view! {
                        <text x=t.x y=y text-anchor="middle" font-size="10" class="fill-muted">
                            {t.label.clone()}
                        </text>
                    }
                })
                .collect_view()
        }}
    }
}

/// Hatched bands for unavailable stretches, labelled `unavailable (reason)` where they fit.
#[component]
fn GapBands(model: Memo<TimeSeriesModel>) -> impl IntoView {
    view! {
        {move || {
            let m = model.read();
            let (top, h) = (m.plot.top, m.plot.bottom - m.plot.top);
            m.gaps
                .iter()
                .map(|g| {
                    let w = (g.x1 - g.x0).max(2.0);
                    view! {
                        <g>
                            <rect x=g.x0 y=top width=w height=h fill="url(#fleet-gap-hatch)">
                                <title>{g.annotation.clone()}</title>
                            </rect>
                            {(w > 90.0)
                                .then(|| {
                                    view! {
                                        <text x=g.x0 + 4.0 y=top + 12.0 font-size="10" class="fill-muted">
                                            {g.annotation.clone()}
                                        </text>
                                    }
                                })}
                        </g>
                    }
                })
                .collect_view()
        }}
    }
}

/// The polylines as SVG paths; dense series draw on the canvas instead.
#[component]
fn Lines(model: Memo<TimeSeriesModel>) -> impl IntoView {
    view! {
        {move || {
            let m = model.read();
            (m.renderer == Renderer::Svg)
                .then(|| {
                    m.segments
                        .iter()
                        .map(|s| {
                            view! {
                                <path
                                    d=s.path_d()
                                    fill="none"
                                    stroke-width="1.5"
                                    stroke-linecap="round"
                                    stroke-linejoin="round"
                                    class="stroke-tone-info-fg"
                                />
                            }
                        })
                        .collect_view()
                })
        }}
    }
}

/// The vertical cursor line and the dot on the nearest reading.
#[component]
fn CrosshairMark(model: Memo<TimeSeriesModel>, readout: Memo<Option<Crosshair>>) -> impl IntoView {
    view! {
        {move || {
            let (top, bottom) = {
                let m = model.read();
                (m.plot.top, m.plot.bottom)
            };
            readout
                .get()
                .map(|c| {
                    view! {
                        <g>
                            <line
                                x1=c.x
                                x2=c.x
                                y1=top
                                y2=bottom
                                stroke-width="1"
                                stroke-dasharray="3 3"
                                class="stroke-fg"
                            ></line>
                            {c.y.map(|y| view! { <circle cx=c.x cy=y r="3" class="fill-tone-info-fg"></circle> })}
                        </g>
                    }
                })
        }}
    }
}

/// Row height of the data table, in pixels (fixed, so the list can be windowed).
const ROW_PX: u64 = 24;
/// Height of the data table's scroll area, in pixels.
const TABLE_PX: u64 = 288;

/// The chart's readings as a table: the accessible alternative to the drawing. Only the rows in
/// view are rendered ([`window`]); `aria-rowcount` and `aria-rowindex` keep the full size known.
#[component]
fn DataTable(model: Memo<TimeSeriesModel>) -> impl IntoView {
    let scroll_top = RwSignal::new(0_u64);
    let rows = Memo::new(move |_| model.read().table_rows());
    let win = Memo::new(move |_| window(scroll_top.get(), TABLE_PX, ROW_PX, rows.read().len(), 5));
    view! {
        <div
            class="overflow-auto rounded border border-border"
            tabindex="0"
            style:height=format!("{TABLE_PX}px")
            on:scroll=move |ev| {
                let el = event_target::<web_sys::HtmlElement>(&ev);
                scroll_top.set(u64::try_from(el.scroll_top().max(0)).unwrap_or(0));
            }
        >
            <table class="w-full text-left text-xs" aria-rowcount=move || rows.read().len() + 1>
                <caption class="sr-only">{move || format!("{} over time", model.read().y_axis.label)}</caption>
                <thead class="sticky top-0 bg-surface text-muted">
                    <tr aria-rowindex="1">
                        <th scope="col" class="px-2 py-1 font-medium">"Time"</th>
                        <th scope="col" class="px-2 py-1 font-medium">"Value"</th>
                    </tr>
                </thead>
                <tbody>
                    <tr aria-hidden="true" style:height=move || format!("{}px", win.get().pad_top)></tr>
                    {move || {
                        let w = win.get();
                        rows.read()
                            .iter()
                            .enumerate()
                            .skip(w.start)
                            .take(w.end - w.start)
                            .map(|(i, r)| {
                                view! {
                                    <tr class="h-6 border-t border-border" aria-rowindex=i + 2>
                                        <td class="px-2 py-0.5 tabular-nums">{r.time.clone()}</td>
                                        <td class="px-2 py-0.5 tabular-nums">{r.value.clone()}</td>
                                    </tr>
                                }
                            })
                            .collect_view()
                    }}
                    <tr aria-hidden="true" style:height=move || format!("{}px", win.get().pad_bottom)></tr>
                </tbody>
            </table>
        </div>
    }
}

/// Draws the polylines of a dense series on the canvas under the SVG axes.
fn draw_lines(canvas: &HtmlCanvasElement, model: &TimeSeriesModel, width: f64, height: f64) {
    let Some(ctx) = canvas::prepare(canvas, width, height) else {
        return;
    };
    ctx.set_stroke_style_str(&canvas::css_var(canvas, "--tone-info-fg", "#2563eb"));
    ctx.set_line_width(1.5);
    ctx.set_line_join("round");
    ctx.set_line_cap("round");
    for segment in &model.segments {
        ctx.begin_path();
        for (i, p) in segment.points.iter().enumerate() {
            if i == 0 {
                ctx.move_to(p.x, p.y);
            } else {
                ctx.line_to(p.x, p.y);
            }
        }
        if segment.points.len() == 1
            && let Some(p) = segment.points.first()
        {
            ctx.line_to(p.x, p.y);
        }
        ctx.stroke();
    }
}
