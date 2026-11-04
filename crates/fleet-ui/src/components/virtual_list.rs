// SPDX-License-Identifier: AGPL-3.0-only
//! A scroll container that renders only the rows in view.

use leptos::prelude::*;

use crate::virtual_list::window;

/// A list of `count` rows of fixed `row_height` pixels in a viewport `height` pixels tall.
/// `render` draws one row by index; the list supplies the `listitem` wrapper and its position.
#[component]
pub fn VirtualList(
    /// The number of rows.
    #[prop(into)]
    count: Signal<usize>,
    /// The height of every row, in pixels.
    row_height: u64,
    /// The height of the scroll viewport, in pixels.
    height: u64,
    /// Rows rendered beyond the viewport on each side.
    #[prop(default = 5)]
    overscan: usize,
    /// The accessible name of the list.
    #[prop(into)]
    label: String,
    /// Draws the row at an index.
    render: Callback<usize, AnyView>,
) -> impl IntoView {
    let scroll_top = RwSignal::new(0_u64);
    let win =
        Memo::new(move |_| window(scroll_top.get(), height, row_height, count.get(), overscan));
    view! {
        <div
            role="list"
            aria-label=label
            tabindex="0"
            class="overflow-y-auto"
            style:height=format!("{height}px")
            on:scroll=move |ev| {
                let el = event_target::<web_sys::HtmlElement>(&ev);
                scroll_top.set(u64::try_from(el.scroll_top().max(0)).unwrap_or(0));
            }
        >
            <div aria-hidden="true" style:height=move || format!("{}px", win.get().pad_top)></div>
            <For
                each=move || win.get().start..win.get().end
                key=|i| *i
                children=move |i| {
                    view! {
                        <div
                            role="listitem"
                            aria-posinset=i + 1
                            aria-setsize=move || count.get()
                            style:height=format!("{row_height}px")
                        >
                            {render.run(i)}
                        </div>
                    }
                }
            />
            <div aria-hidden="true" style:height=move || format!("{}px", win.get().pad_bottom)></div>
        </div>
    }
}
