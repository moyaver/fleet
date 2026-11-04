// SPDX-License-Identifier: AGPL-3.0-only
//! The Leptos view of the swimlane timeline: a canvas drawn from [`TimelineLayout`], pointer and
//! keyboard handlers that call the view-model, and an equivalent table. Flow names reach the
//! page only as canvas text and text nodes.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs"
)]

use leptos::prelude::*;
use leptos::tachys::html::event::{Custom, on};
use leptos::wasm_bindgen::{JsCast, JsValue};
use web_sys::{CanvasRenderingContext2d, Element, HtmlCanvasElement, MouseEvent};

use crate::canvas;
use crate::timeline::layout::{BarBox, LayoutConfig, TimelineLayout};
use crate::timeline::model::TimelineData;
use crate::timeline::nav;
use crate::timeline::style::Pattern;
use crate::timeline::table::{TableRow, table_rows};
use crate::timeline::viewport::Viewport;
use crate::tone::Tone;

/// The narrowest window the user can zoom to.
const MIN_SPAN_MS: i64 = 1_000;
/// A press that moves further than this is a drag, not a click.
const CLICK_SLOP_PX: f64 = 3.0;
/// Wheel zoom: one notch of 100 delta units is about 20 %.
const WHEEL_ZOOM_PER_UNIT: f64 = 0.002;
const BUTTON_CLASS: &str = "rounded border border-border px-2 py-0.5 text-xs text-fg hover:bg-tone-neutral-bg focus-visible:outline-2 focus-visible:outline-focus";

/// A wheel listener that may call `preventDefault`. Browsers treat wheel listeners on the
/// document as passive, which is where Leptos's delegated `on:wheel` attaches; zooming must not
/// also scroll the page.
fn non_passive_wheel() -> Custom<web_sys::WheelEvent> {
    let mut wheel = Custom::new("wheel");
    wheel.options_mut().set_passive(false);
    wheel
}

/// Focus moves on the keyboard.
#[derive(Debug, Clone, Copy)]
enum Move {
    Next,
    Previous,
    LaneUp,
    LaneDown,
    First,
    Last,
}

impl Tone {
    /// The CSS custom property of this tone's `fg` or `bg` colour, for canvas drawing.
    fn var(self, part: &str) -> String {
        let role = match self {
            Self::Neutral => "neutral",
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Danger => "danger",
            Self::Accent => "accent",
            Self::Muted => "muted",
        };
        format!("--tone-{role}-{part}")
    }
}

/// Everything the handlers share. All fields are `Copy` handles, so closures copy the struct.
#[derive(Clone, Copy)]
struct State {
    data: Signal<TimelineData>,
    now_ms: Signal<i64>,
    width: f64,
    viewport: RwSignal<Viewport>,
    layout: Memo<TimelineLayout>,
    focus: RwSignal<Option<String>>,
    hover: RwSignal<Option<String>>,
    cursor: RwSignal<Option<i64>>,
    drag_from: RwSignal<Option<f64>>,
    /// How far (client pixels) the current or last drag travelled: past a few pixels the
    /// `click` that follows the pointer release is not a selection.
    drag_travel: RwSignal<f64>,
    on_select: Option<Callback<String>>,
}

impl State {
    /// The pointer in layout pixels, or `None` if the target has no size.
    fn point(&self, ev: &MouseEvent) -> Option<(f64, f64)> {
        let el = ev.current_target()?.dyn_into::<Element>().ok()?;
        let rect = el.get_bounding_client_rect();
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            return None;
        }
        let height = self.layout.read_untracked().height;
        Some((
            (f64::from(ev.client_x()) - rect.left()) * self.width / rect.width(),
            (f64::from(ev.client_y()) - rect.top()) * height / rect.height(),
        ))
    }

    fn plot_width(&self) -> f64 {
        (self.width - LayoutConfig::default().gutter).max(1.0)
    }

    fn mid_time(&self) -> i64 {
        let (a, b) = self.viewport.get_untracked().window();
        a / 2 + b / 2
    }

    fn pointer_down(&self, ev: &web_sys::PointerEvent) {
        self.drag_from.set(Some(f64::from(ev.client_x())));
        self.drag_travel.set(0.0);
        if let Some(el) = ev
            .current_target()
            .and_then(|t| t.dyn_into::<Element>().ok())
        {
            // Keeps the drag going when the pointer leaves the canvas; failing is harmless.
            el.set_pointer_capture(ev.pointer_id()).ok();
        }
    }

    fn pointer_move(&self, ev: &web_sys::PointerEvent) {
        if let Some(from) = self.drag_from.get_untracked() {
            let now_x = f64::from(ev.client_x());
            let scale = self.scale_to_layout(ev);
            self.viewport
                .update(|v| v.pan_px((now_x - from) * scale, self.plot_width()));
            self.drag_travel.update(|t| *t += (now_x - from).abs());
            self.drag_from.set(Some(now_x));
            return;
        }
        let Some((x, y)) = self.point(ev) else { return };
        let layout = self.layout.read_untracked();
        self.hover.set(layout.hit_test(x, y).map(|b| b.id.clone()));
        self.cursor.set(layout.time_at_x(x));
    }

    /// Layout pixels per client pixel.
    fn scale_to_layout(&self, ev: &MouseEvent) -> f64 {
        ev.current_target()
            .and_then(|t| t.dyn_into::<Element>().ok())
            .map_or(1.0, |el| {
                let w = el.get_bounding_client_rect().width();
                if w > 0.0 { self.width / w } else { 1.0 }
            })
    }

    fn pointer_up(&self) {
        self.drag_from.set(None);
    }

    fn pointer_leave(&self) {
        if self.drag_from.get_untracked().is_none() {
            self.hover.set(None);
            self.cursor.set(None);
        }
    }

    fn click(&self, ev: &MouseEvent) {
        if self.drag_travel.get_untracked() > CLICK_SLOP_PX {
            return;
        }
        let Some((x, y)) = self.point(ev) else { return };
        let hit = self
            .layout
            .read_untracked()
            .hit_test(x, y)
            .map(|b| b.id.clone());
        if let Some(id) = hit {
            self.focus.set(Some(id.clone()));
            if let Some(cb) = self.on_select {
                cb.run(id);
            }
        }
    }

    fn wheel(&self, ev: &web_sys::WheelEvent) {
        ev.prevent_default();
        let anchor = self
            .point(ev)
            .and_then(|(x, _)| self.layout.read_untracked().time_at_x(x))
            .unwrap_or_else(|| self.mid_time());
        let factor = (-ev.delta_y() * WHEEL_ZOOM_PER_UNIT).exp();
        self.viewport.update(|v| v.zoom(factor, anchor));
    }

    fn zoom_by(&self, factor: f64) {
        let anchor = self
            .cursor
            .get_untracked()
            .unwrap_or_else(|| self.mid_time());
        self.viewport.update(|v| v.zoom(factor, anchor));
    }

    fn reset_zoom(&self) {
        let now = self.now_ms.get_untracked();
        if let Some(bounds) = self.data.read_untracked().bounds(now) {
            self.viewport.set(Viewport::new(bounds, MIN_SPAN_MS));
        }
    }

    fn move_focus(&self, to: Move) {
        let now = self.now_ms.get_untracked();
        let data = self.data.read_untracked();
        let current = self.focus.get_untracked();
        let current = current.as_deref();
        let bar = match to {
            Move::Next => nav::next(&data, current),
            Move::Previous => nav::previous(&data, current),
            Move::LaneUp => nav::lane_up(&data, current, now),
            Move::LaneDown => nav::lane_down(&data, current, now),
            Move::First => nav::first(&data),
            Move::Last => nav::last(&data),
        };
        if let Some(bar) = bar {
            self.viewport
                .update(|v| v.reveal(bar.start_ms, bar.end_ms(now)));
            self.focus.set(Some(bar.id.clone()));
        }
    }

    fn key_down(&self, ev: &web_sys::KeyboardEvent) {
        let handled = match ev.key().as_str() {
            "ArrowRight" => {
                self.move_focus(Move::Next);
                true
            }
            "ArrowLeft" => {
                self.move_focus(Move::Previous);
                true
            }
            "ArrowDown" => {
                self.move_focus(Move::LaneDown);
                true
            }
            "ArrowUp" => {
                self.move_focus(Move::LaneUp);
                true
            }
            "Home" => {
                self.move_focus(Move::First);
                true
            }
            "End" => {
                self.move_focus(Move::Last);
                true
            }
            "+" | "=" => {
                self.zoom_by(2.0);
                true
            }
            "-" => {
                self.zoom_by(0.5);
                true
            }
            "Enter" | " " => {
                if let (Some(id), Some(cb)) = (self.focus.get_untracked(), self.on_select) {
                    cb.run(id);
                }
                true
            }
            _ => false,
        };
        if handled {
            ev.prevent_default();
        }
    }

    /// The table row of the focused bar, for the live region.
    fn announcement(&self, rows: &[TableRow]) -> String {
        let focus = self.focus.get();
        rows.iter()
            .find(|r| Some(&r.bar_id) == focus.as_ref())
            .map(TableRow::summary)
            .unwrap_or_default()
    }
}

/// Devices as lanes and flows as bars over time, on a canvas. Wheel or `+`/`-` zooms around the
/// cursor, dragging pans, arrow keys walk the flows, and a table shows the same data. `cursor`
/// shares the time under the pointer with charts.
#[component]
pub fn SwimlaneTimeline(
    /// Lanes and bars.
    #[prop(into)]
    data: Signal<TimelineData>,
    /// The current time (unix ms): where running bars end. Update it about once a second.
    #[prop(into)]
    now_ms: Signal<i64>,
    /// Width of the drawing in pixels; it scales down with its container.
    #[prop(default = 960.0)]
    width: f64,
    /// The viewer's offset from UTC in minutes, for time labels.
    #[prop(default = 0)]
    utc_offset_minutes: i32,
    /// A cursor time (unix ms) shared with charts.
    #[prop(optional)]
    cursor: Option<RwSignal<Option<i64>>>,
    /// Called with the flow execution id when a bar is clicked or activated with the keyboard.
    #[prop(optional, into)]
    on_select: Option<Callback<String>>,
) -> impl IntoView {
    let initial = data
        .read_untracked()
        .bounds(now_ms.get_untracked())
        .unwrap_or((0, 1));
    let viewport = RwSignal::new(Viewport::new(initial, MIN_SPAN_MS));
    let layout = Memo::new(move |_| {
        let config = LayoutConfig {
            width,
            now_ms: now_ms.get(),
            utc_offset_minutes,
            ..LayoutConfig::default()
        };
        TimelineLayout::build(&data.read(), viewport.get().window(), &config)
    });
    let state = State {
        data,
        now_ms,
        width,
        viewport,
        layout,
        focus: RwSignal::new(None),
        hover: RwSignal::new(None),
        cursor: cursor.unwrap_or_else(|| RwSignal::new(None)),
        drag_from: RwSignal::new(None),
        drag_travel: RwSignal::new(0.0),
        on_select,
    };
    let table_open = RwSignal::new(false);
    let canvas_ref = NodeRef::<leptos::html::Canvas>::new();

    // Live data widens the bounds; an unzoomed window follows them.
    Effect::new(move |_| {
        if let Some(bounds) = data.read().bounds(now_ms.get()) {
            viewport.update(|v| v.set_bounds(bounds));
        }
    });
    Effect::new(move |_| {
        let layout = state.layout.get();
        let marks = Marks {
            focus: state.focus.get(),
            hover: state.hover.get(),
            cursor_x: state.cursor.get().and_then(|t| layout.x_for_time(t)),
            width,
        };
        if let Some(canvas) = canvas_ref.get() {
            draw(&canvas, &layout, &marks);
        }
    });
    let rows = Memo::new(move |_| table_rows(&data.read(), now_ms.get(), utc_offset_minutes));

    view! {
        <section class="space-y-2" aria-label="Flow timeline">
            <Toolbar state=state table_open=table_open />
            <Show
                when=move || !data.read().bars().is_empty()
                fallback=|| view! { <p class="text-sm text-muted">"No flows yet."</p> }
            >
                <Show when=move || !table_open.get() fallback=move || view! { <TimelineTable rows=rows state=state /> }>
                    <canvas
                        node_ref=canvas_ref
                        class="block h-auto w-full touch-none rounded border border-border focus-visible:outline-2 focus-visible:outline-focus"
                        tabindex="0"
                        role="group"
                        aria-label="Flows by device over time. Arrow keys move between flows, plus and minus zoom."
                        on:pointerdown=move |ev| state.pointer_down(&ev)
                        on:pointermove=move |ev| state.pointer_move(&ev)
                        on:pointerup=move |_| state.pointer_up()
                        on:pointercancel=move |_| state.pointer_up()
                        on:pointerleave=move |_| state.pointer_leave()
                        on:click=move |ev| state.click(&ev)
                        on:keydown=move |ev| state.key_down(&ev)
                        on:focus=move |_| {
                            if state.focus.get_untracked().is_none() {
                                state.move_focus(Move::First);
                            }
                        }
                        {..on(non_passive_wheel(), move |ev: web_sys::WheelEvent| state.wheel(&ev))}
                    ></canvas>
                    <p class="min-h-5 text-xs text-muted" aria-hidden="true">
                        {move || {
                            let hover = state.hover.get();
                            rows.read()
                                .iter()
                                .find(|r| Some(&r.bar_id) == hover.as_ref())
                                .map_or_else(
                                    || "Hover or focus a flow for details.".to_owned(),
                                    TableRow::summary,
                                )
                        }}
                    </p>
                    <p class="sr-only" aria-live="polite">{move || state.announcement(&rows.read())}</p>
                </Show>
            </Show>
        </section>
    }
}

/// Zoom buttons for people without a wheel, and the table toggle.
#[component]
fn Toolbar(state: State, table_open: RwSignal<bool>) -> impl IntoView {
    view! {
        <div class="flex flex-wrap items-center gap-2">
            <button type="button" class=BUTTON_CLASS on:click=move |_| state.zoom_by(2.0)>"Zoom in"</button>
            <button type="button" class=BUTTON_CLASS on:click=move |_| state.zoom_by(0.5)>"Zoom out"</button>
            <button type="button" class=BUTTON_CLASS on:click=move |_| state.reset_zoom()>"Reset zoom"</button>
            <button
                type="button"
                class=BUTTON_CLASS
                aria-pressed=move || table_open.get().to_string()
                on:click=move |_| table_open.update(|open| *open = !*open)
            >
                {move || if table_open.get() { "Show timeline" } else { "Show table" }}
            </button>
        </div>
    }
}

/// The timeline as a table: the same flows, one row each.
#[component]
fn TimelineTable(rows: Memo<Vec<TableRow>>, state: State) -> impl IntoView {
    view! {
        <div class="max-h-96 overflow-auto rounded border border-border" tabindex="0">
            <table class="w-full text-left text-xs">
                <caption class="sr-only">"Flows by device, in start order"</caption>
                <thead class="sticky top-0 bg-surface text-muted">
                    <tr>
                        {["Device", "Flow", "Start", "End", "Duration", "Outcome", "Provenance"]
                            .map(|h| view! { <th scope="col" class="px-2 py-1 font-medium">{h}</th> })}
                    </tr>
                </thead>
                <tbody>
                    {move || {
                        let focus = state.focus.get();
                        rows.read()
                            .iter()
                            .map(|r| {
                                let current = Some(&r.bar_id) == focus.as_ref();
                                view! {
                                    <tr
                                        class="border-t border-border"
                                        aria-current=current.then_some("true")
                                    >
                                        <td class="px-2 py-0.5">{r.device.clone()}</td>
                                        <td class="px-2 py-0.5">{r.flow.clone()}</td>
                                        <td class="px-2 py-0.5 tabular-nums">{r.start.clone()}</td>
                                        <td class="px-2 py-0.5 tabular-nums">{r.end.clone()}</td>
                                        <td class="px-2 py-0.5 tabular-nums">{r.duration.clone()}</td>
                                        <td class="px-2 py-0.5">{r.outcome.clone()}</td>
                                        <td class="px-2 py-0.5">{r.provenance.clone()}</td>
                                    </tr>
                                }
                            })
                            .collect_view()
                    }}
                </tbody>
            </table>
        </div>
    }
}

/// What is drawn on top of the bars.
struct Marks {
    focus: Option<String>,
    hover: Option<String>,
    cursor_x: Option<f64>,
    width: f64,
}

/// Paints the whole timeline. Colours are read from the theme's custom properties each time, so
/// a theme switch shows on the next redraw.
fn draw(canvas: &HtmlCanvasElement, layout: &TimelineLayout, marks: &Marks) {
    let Some(ctx) = canvas::prepare(canvas, marks.width, layout.height) else {
        return;
    };
    let color = |name: &str, fallback: &str| canvas::css_var(canvas, name, fallback);
    let gutter = LayoutConfig::default().gutter;
    let axis_height = LayoutConfig::default().axis_height;
    let (fg, muted, border) = (
        color("--fg", "#0f172a"),
        color("--muted", "#475569"),
        color("--border", "#cbd5e1"),
    );
    ctx.set_font("12px system-ui, sans-serif");
    ctx.set_text_baseline("middle");

    for lane in &layout.lanes {
        ctx.set_stroke_style_str(&border);
        line(
            &ctx,
            0.0,
            lane.y + lane.height,
            marks.width,
            lane.y + lane.height,
        );
        ctx.set_fill_style_str(&fg);
        clipped_text(&ctx, &lane.label, 8.0, lane.y, gutter - 12.0, lane.height);
    }
    ctx.set_fill_style_str(&muted);
    ctx.set_text_align("center");
    for tick in &layout.x_ticks {
        ctx.set_stroke_style_str(&border);
        line(&ctx, tick.x, axis_height, tick.x, layout.height);
        let _ = ctx.fill_text(&tick.label, tick.x, axis_height / 2.0);
    }
    ctx.set_text_align("left");

    for bar in &layout.bars {
        draw_bar(&ctx, canvas, bar, marks);
    }
    if let Some(x) = marks.cursor_x {
        ctx.set_stroke_style_str(&fg);
        set_dash(&ctx, &[3.0, 3.0]);
        line(&ctx, x, axis_height, x, layout.height);
        set_dash(&ctx, &[]);
    }
}

fn draw_bar(
    ctx: &CanvasRenderingContext2d,
    canvas: &HtmlCanvasElement,
    bar: &BarBox,
    marks: &Marks,
) {
    let r = bar.rect;
    let tone = bar.style.tone;
    let strong = canvas::css_var(canvas, &tone.var("fg"), "#334155");
    let soft = canvas::css_var(canvas, &tone.var("bg"), "#f1f5f9");
    ctx.set_line_width(1.5);
    match bar.style.pattern {
        Pattern::Solid => {
            ctx.set_fill_style_str(&strong);
            ctx.fill_rect(r.x, r.y, r.w, r.h);
        }
        Pattern::Hatched => {
            ctx.set_fill_style_str(&soft);
            ctx.fill_rect(r.x, r.y, r.w, r.h);
            hatch(ctx, bar, &strong);
            ctx.set_stroke_style_str(&strong);
            ctx.stroke_rect(r.x, r.y, r.w, r.h);
        }
        Pattern::Dashed => {
            ctx.set_fill_style_str(&soft);
            ctx.fill_rect(r.x, r.y, r.w, r.h);
            ctx.set_stroke_style_str(&strong);
            set_dash(ctx, &[4.0, 3.0]);
            ctx.stroke_rect(r.x, r.y, r.w, r.h);
            set_dash(ctx, &[]);
        }
    }
    if bar.running {
        // An open end: the bar is still growing.
        ctx.set_fill_style_str(&soft);
        ctx.fill_rect((r.x + r.w - 4.0).max(r.x), r.y, 4.0_f64.min(r.w), r.h);
    }
    if r.w > 36.0 {
        let on_bar = if matches!(bar.style.pattern, Pattern::Solid) {
            &soft
        } else {
            &strong
        };
        ctx.set_fill_style_str(on_bar);
        clipped_text(ctx, &bar.label, r.x + 4.0, r.y, r.w - 8.0, r.h);
    }
    let focused = marks.focus.as_deref() == Some(bar.id.as_str());
    let hovered = marks.hover.as_deref() == Some(bar.id.as_str());
    if focused || hovered {
        ctx.set_stroke_style_str(&canvas::css_var(
            canvas,
            if focused { "--focus" } else { "--fg" },
            "#1d4ed8",
        ));
        ctx.set_line_width(if focused { 2.5 } else { 1.0 });
        ctx.stroke_rect(r.x - 2.0, r.y - 2.0, r.w + 4.0, r.h + 4.0);
    }
}

/// Diagonal stripes inside a bar: "not final yet".
fn hatch(ctx: &CanvasRenderingContext2d, bar: &BarBox, color: &str) {
    let r = bar.rect;
    ctx.save();
    ctx.begin_path();
    ctx.rect(r.x, r.y, r.w, r.h);
    ctx.clip();
    ctx.set_stroke_style_str(color);
    ctx.set_line_width(1.0);
    ctx.begin_path();
    let mut x = r.x - r.h;
    while x < r.x + r.w {
        ctx.move_to(x, r.y + r.h);
        ctx.line_to(x + r.h, r.y);
        x += 6.0;
    }
    ctx.stroke();
    ctx.restore();
}

fn line(ctx: &CanvasRenderingContext2d, x0: f64, y0: f64, x1: f64, y1: f64) {
    ctx.set_line_width(1.0);
    ctx.begin_path();
    ctx.move_to(x0, y0);
    ctx.line_to(x1, y1);
    ctx.stroke();
}

fn set_dash(ctx: &CanvasRenderingContext2d, pattern: &[f64]) {
    let array: js_sys::Array = pattern.iter().map(|v| JsValue::from_f64(*v)).collect();
    // An invalid dash list is ignored by the canvas; there is nothing to recover.
    ctx.set_line_dash(&array).ok();
}

/// Text inside a clipped box, vertically centred, so a long label never spills over.
fn clipped_text(ctx: &CanvasRenderingContext2d, text: &str, x: f64, y: f64, w: f64, h: f64) {
    ctx.save();
    ctx.begin_path();
    ctx.rect(x, y, w.max(0.0), h);
    ctx.clip();
    let _ = ctx.fill_text(text, x, y + h / 2.0);
    ctx.restore();
}
