// SPDX-License-Identifier: AGPL-3.0-only
//! Thin Leptos view over [`LogModel`]. All text goes in as text nodes, never as raw HTML;
//! colors are set through the CSSOM so the strict CSP (`style-src 'self'`) holds.

use leptos::prelude::*;

use crate::logview::ansi::{self, Span};
use crate::logview::model::{LogModel, anchor};
use crate::logview::window::scroll_to;
use crate::virtual_list::window;

/// Row height in pixels; must match the `leading` of `.log-line` in the stylesheet.
const ROW_PX: u32 = 20;
/// Rows rendered above and below the viewport.
const OVERSCAN: usize = 20;

fn span_view(span: Span) -> impl IntoView {
    let fg = span.style.fg.map(ansi::css_color);
    let bg = span.style.bg.map(ansi::css_color);
    view! {
        <span
            class:font-bold=span.style.bold
            class:italic=span.style.italic
            class:underline=span.style.underline
            style:color=fg
            style:background-color=bg
        >
            {span.text}
        </span>
    }
}

fn lines_view(
    log: StoredValue<LogModel>,
    scroll_top: u64,
    viewport_px: u32,
    selected_line: Signal<Option<usize>>,
) -> impl IntoView {
    let total = log.with_value(|l| l.lines().len());
    let w = window(
        scroll_top,
        u64::from(viewport_px),
        u64::from(ROW_PX),
        total,
        OVERSCAN,
    );
    let rows = log
        .with_value(|l| {
            (w.start..w.end)
                .filter_map(|i| l.line(i).map(|line| (i, line.clone())))
                .collect::<Vec<_>>()
        })
        .into_iter()
        .map(|(i, line)| {
            let selected = move || selected_line.get() == Some(i);
            view! {
                <div
                    id=format!("L{}", i + 1)
                    class="flex whitespace-pre leading-5 text-fg"
                    class:bg-tone-warning-bg=selected
                    aria-current=move || selected().then_some("true")
                    style:height=format!("{ROW_PX}px")
                >
                    <a class="w-14 shrink-0 select-none pr-3 text-right text-muted" href=anchor(i)>
                        {i + 1}
                    </a>
                    {line.spans.into_iter().map(span_view).collect_view()}
                </div>
            }
        })
        .collect_view();
    view! {
        <div style:height=format!("{}px", w.pad_top)></div>
        {rows}
        <div style:height=format!("{}px", w.pad_bottom)></div>
    }
}

/// A virtualized log with search and line anchors. `selected_line` highlights an anchored line.
#[component]
#[expect(
    clippy::must_use_candidate,
    reason = "the component macro rewrites the signature"
)]
pub fn LogViewer(
    /// The parsed log.
    log: LogModel,
    /// Zero-based line to highlight and scroll to (from `#L123`, or a step jump).
    #[prop(into, optional)]
    selected_line: Signal<Option<usize>>,
    /// Viewport height in pixels.
    #[prop(default = 480)]
    viewport_px: u32,
    /// The region's accessible name, when there is more than one log on a page.
    #[prop(into, default = "Log".to_owned())]
    label: String,
) -> impl IntoView {
    let log = StoredValue::new(log);
    let scroll_top = RwSignal::new(0_u64);
    let query = RwSignal::new(String::new());
    let search = RwSignal::new(log.with_value(|l| l.search("")));
    let scroller = NodeRef::<leptos::html::Div>::new();
    let total = log.with_value(|l| l.lines().len());

    let scroll_line = move |line: usize| {
        let top = scroll_to(line.saturating_sub(2), total, ROW_PX, viewport_px);
        scroll_top.set(top);
        if let Some(el) = scroller.get() {
            el.set_scroll_top(i32::try_from(top).unwrap_or(i32::MAX));
        }
    };
    Effect::new(move |_| {
        if let Some(line) = selected_line.get() {
            scroll_line(line);
        }
    });
    let step = move |forward: bool| {
        search.update(|s| {
            let hit = if forward {
                s.select_next()
            } else {
                s.select_prev()
            };
            if let Some(m) = hit {
                scroll_line(m.line);
            }
        });
    };

    view! {
        <section class="border-y border-line" aria-label=label>
            <div class="flex items-center gap-2 border-b border-border p-2">
                <input
                    type="search"
                    aria-label="Search log"
                    prop:value=move || query.get()
                    on:input=move |ev| {
                        let q = event_target_value(&ev);
                        search.set(log.with_value(|l| l.search(&q)));
                        query.set(q);
                    }
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        if ev.key() == "Enter" {
                            step(!ev.shift_key());
                        }
                    }
                />
                <button type="button" on:click=move |_| step(false)>"Previous"</button>
                <button type="button" on:click=move |_| step(true)>"Next"</button>
                <span aria-live="polite">
                    {move || {
                        search
                            .with(|s| match s.position() {
                                Some(p) => format!("{p} of {}", s.count()),
                                None => format!("{} matches", s.count()),
                            })
                    }}
                </span>
            </div>
            <div
                class="overflow-auto bg-surface font-mono text-sm text-fg"
                role="log"
                // A log grows while a run is open: its lines are never announced (the status region
                // announces run-level events only).
                aria-live="off"
                tabindex="0"
                node_ref=scroller
                style:height=format!("{viewport_px}px")
                on:scroll=move |ev| {
                    let el = event_target::<web_sys::HtmlElement>(&ev);
                    scroll_top.set(u64::try_from(el.scroll_top()).unwrap_or(0));
                }
            >
                {move || lines_view(log, scroll_top.get(), viewport_px, selected_line)}
            </div>
        </section>
    }
}
