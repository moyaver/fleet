// SPDX-License-Identifier: AGPL-3.0-only
//! Thin Leptos view over the screenshot viewer: plain `<img>` elements with model URLs.

use leptos::prelude::*;
use leptos::web_sys;

use crate::screenshots::{ScreenshotViewer, Zoom};

/// Screenshot viewer with a step strip and zoom controls.
#[component]
#[expect(
    clippy::must_use_candidate,
    reason = "the component macro rewrites the signature"
)]
pub fn ScreenshotViewerView(
    /// The model.
    viewer: ScreenshotViewer,
) -> impl IntoView {
    let viewer = RwSignal::new(viewer);
    view! {
        <section
            class="rounded border border-border bg-surface p-2 text-fg"
            aria-label="Screenshots"
            on:keydown=move |ev: web_sys::KeyboardEvent| {
                match ev.key().as_str() {
                    "ArrowRight" => viewer.update(ScreenshotViewer::next),
                    "ArrowLeft" => viewer.update(ScreenshotViewer::prev),
                    "+" | "=" => viewer.update(ScreenshotViewer::zoom_in),
                    "-" => viewer.update(ScreenshotViewer::zoom_out),
                    "0" => viewer.update(ScreenshotViewer::reset_zoom),
                    _ => {}
                }
            }
        >
            <Show
                when=move || !viewer.with(ScreenshotViewer::is_empty)
                fallback=|| view! { <p>"No screenshots for this run."</p> }
            >
                <div class="flex items-center gap-2">
                    <button type="button" on:click=move |_| viewer.update(ScreenshotViewer::zoom_out)>
                        "Zoom out"
                    </button>
                    <button type="button" on:click=move |_| viewer.update(ScreenshotViewer::zoom_in)>
                        "Zoom in"
                    </button>
                    <button type="button" on:click=move |_| viewer.update(ScreenshotViewer::reset_zoom)>
                        "Fit"
                    </button>
                    <span aria-live="polite">{move || viewer.with(ScreenshotViewer::position)}</span>
                </div>
                <div class="overflow-auto border border-border" tabindex="0">
                    {move || {
                        viewer
                            .with(|v| {
                                v.selected()
                                    .map(|s| {
                                        let width = match v.zoom() {
                                            Zoom::Fit => "100%".to_owned(),
                                            Zoom::Percent(p) => format!("{p}%"),
                                        };
                                        view! {
                                            <img src=s.url.clone() alt=s.label.clone() style:width=width />
                                        }
                                    })
                            })
                    }}
                </div>
                <ol class="flex gap-1 overflow-x-auto">
                    {move || {
                        viewer
                            .with(ScreenshotViewer::strip)
                            .into_iter()
                            .map(|item| {
                                let index = item.index;
                                view! {
                                    <li>
                                        <button
                                            type="button"
                                            aria-label=item.label.clone()
                                            aria-current=item.selected.then_some("true")
                                            on:click=move |_| viewer.update(|v| v.select(index))
                                        >
                                            <img src=item.url alt="" loading="lazy" />
                                        </button>
                                    </li>
                                }
                            })
                            .collect_view()
                    }}
                </ol>
            </Show>
        </section>
    }
}
