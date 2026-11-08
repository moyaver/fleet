// SPDX-License-Identifier: AGPL-3.0-only
//! Thin Leptos view over the screenshot viewer: plain `<img>` elements with model URLs.

use leptos::prelude::*;

use crate::screenshots::{ScreenshotViewer, Shot, Zoom};

/// The image width for a zoom level.
fn width_of(zoom: Zoom) -> String {
    match zoom {
        Zoom::Fit => "100%".to_owned(),
        Zoom::Percent(p) => format!("{p}%"),
    }
}

/// Screenshot viewer with a step strip and zoom controls.
///
/// `shots` may change while the viewer is open (a renewed link is a new URL for the same shot).
/// The viewer keeps its selection and zoom, and the image element stays in place, so the
/// browser keeps showing the old picture until the new one loads.
#[component]
#[expect(
    clippy::must_use_candidate,
    reason = "the component macro rewrites the signature"
)]
pub fn ScreenshotViewerView(
    /// The shots in capture order.
    #[prop(into)]
    shots: Signal<Vec<Shot>>,
) -> impl IntoView {
    let viewer = RwSignal::new(ScreenshotViewer::new(shots.get_untracked()));
    Effect::new(move |_| {
        let shots = shots.get();
        viewer.update(|v| v.replace(shots));
    });
    view! {
        <section
            class="border-y border-line py-2 text-fg"
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
                    <img
                        src=move || viewer.with(|v| v.selected().map(|s| s.url.clone()))
                        alt=move || viewer.with(|v| v.selected().map(|s| s.label.clone()))
                        style:width=move || viewer.with(|v| width_of(v.zoom()))
                    />
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
