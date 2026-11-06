// SPDX-License-Identifier: AGPL-3.0-only
//! Dialog, popover, menu, tabs, tooltip and toasts. Native elements do the heavy lifting
//! (`<dialog>`, the Popover API, invoker commands); the view-models handle keyboard patterns.
//! No script, no inline styles, text nodes only.

use leptos::{prelude::*, wasm_bindgen::JsCast};

use crate::{
    components::controls::{Button, ButtonVariant},
    keyboard::Key,
    menu::{Menu, MenuItem},
    tabs::Tabs as TabsModel,
    toast::ToastQueue,
    tone::Tone,
};

/// Moves keyboard focus to the element with this id, if it exists.
fn focus_id(id: &str) {
    if let Some(el) = document()
        .get_element_by_id(id)
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = el.focus();
    }
}

/// A trigger button and the native modal `<dialog>` it opens. The invoker command
/// (`command="show-modal"`) opens it, `<form method="dialog">` closes it, and the browser traps
/// focus, closes on Escape and returns focus to the trigger.
#[component]
pub fn Dialog(
    /// Unique id; the dialog is `{id}` and its title `{id}-title`.
    #[prop(into)]
    id: String,
    /// The trigger button's label.
    #[prop(into)]
    trigger: String,
    /// The dialog's heading, which also names it.
    #[prop(into)]
    title: String,
    /// The dialog body.
    children: Children,
) -> impl IntoView {
    let title_id = format!("{id}-title");
    let labelled_by = title_id.clone();
    view! {
        <button
            type="button"
            command="show-modal"
            commandfor=id.clone()
            class="inline-flex items-center rounded border border-border bg-surface px-3 py-1.5 text-sm font-medium text-fg hover:bg-tone-neutral-bg"
        >
            {trigger}
        </button>
        <dialog
            id=id
            aria-labelledby=labelled_by
            class="m-auto w-full max-w-lg rounded border border-border bg-surface p-4 text-fg backdrop:bg-black/50"
        >
            <h2 id=title_id class="mb-3 text-lg font-semibold">{title}</h2>
            <div class="mb-4">{children()}</div>
            <form method="dialog" class="flex justify-end">
                <button
                    type="submit"
                    class="inline-flex items-center rounded border border-border bg-surface px-3 py-1.5 text-sm font-medium text-fg hover:bg-tone-neutral-bg"
                >
                    "Close"
                </button>
            </form>
        </dialog>
    }
}

/// A trigger button and a Popover API panel (`popover="auto"`: light dismiss, Escape, focus
/// returns to the trigger).
#[component]
pub fn Popover(
    /// Unique id of the panel.
    #[prop(into)]
    id: String,
    /// The trigger button's label.
    #[prop(into)]
    trigger: String,
    /// The panel content.
    children: Children,
) -> impl IntoView {
    view! {
        <button
            type="button"
            popovertarget=id.clone()
            aria-haspopup="dialog"
            class="inline-flex items-center rounded border border-border bg-surface px-3 py-1.5 text-sm font-medium text-fg hover:bg-tone-neutral-bg"
        >
            {trigger}
        </button>
        <div
            id=id
            popover="auto"
            class="rounded border border-border bg-surface p-3 text-fg shadow-lg"
        >
            {children()}
        </div>
    }
}

/// A menu button: a Popover API panel of `role="menuitem"` buttons with APG keyboard support.
/// Choosing an item runs `on_select` with its index and closes the popover (declaratively).
#[component]
pub fn MenuButton(
    /// Unique id of the menu.
    #[prop(into)]
    id: String,
    /// The trigger button's label.
    #[prop(into)]
    label: String,
    /// The items.
    items: Vec<MenuItem>,
    /// Called with the index of the chosen item.
    on_select: Callback<usize>,
) -> impl IntoView {
    let menu = RwSignal::new(Menu::new(items.clone()));
    let item_id = {
        let id = id.clone();
        move |i: usize| format!("{id}-item-{i}")
    };
    let on_keydown = {
        let item_id = item_id.clone();
        move |ev: web_sys::KeyboardEvent| {
            let key = Key::from_dom(&ev.key());
            // Time stamps are fractional milliseconds; whole ones are enough for typeahead.
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a DOM time stamp is a non-negative number of milliseconds"
            )]
            let now = ev.time_stamp() as u64;
            let mut event = crate::menu::MenuEvent::Ignored;
            menu.update(|m| event = m.on_key(key, now));
            if let crate::menu::MenuEvent::Focus(i) = event {
                ev.prevent_default();
                focus_id(&item_id(i));
            }
        }
    };
    let on_toggle = {
        let item_id = item_id.clone();
        move |ev: web_sys::Event| {
            let open = event_target::<web_sys::HtmlElement>(&ev)
                .matches(":popover-open")
                .unwrap_or(false);
            if open {
                let mut first = None;
                menu.update(|m| first = m.open(false));
                if let Some(i) = first {
                    focus_id(&item_id(i));
                }
            }
        }
    };
    view! {
        <button
            type="button"
            popovertarget=id.clone()
            aria-haspopup="menu"
            class="inline-flex items-center rounded border border-border bg-surface px-3 py-1.5 text-sm font-medium text-fg hover:bg-tone-neutral-bg"
        >
            {label.clone()}
        </button>
        <div
            id=id.clone()
            popover="auto"
            role="menu"
            aria-label=label
            class="min-w-40 rounded border border-border bg-surface p-1 text-fg shadow-lg"
            on:keydown=on_keydown
            on:toggle=on_toggle
        >
            {items
                .into_iter()
                .enumerate()
                .map(|(i, item)| {
                    view! {
                        <button
                            type="button"
                            role="menuitem"
                            id=item_id(i)
                            popovertarget=id.clone()
                            popovertargetaction="hide"
                            tabindex=move || menu.with(|m| m.tab_index(i))
                            disabled=item.disabled
                            class="block w-full rounded px-3 py-1.5 text-left text-sm hover:bg-tone-neutral-bg disabled:opacity-50"
                            on:focus=move |_| menu.update(|m| m.set_active(i))
                            on:click=move |_| on_select.run(i)
                        >
                            {item.label}
                        </button>
                    }
                })
                .collect_view()}
        </div>
    }
}

/// One tab and its panel.
#[derive(Clone)]
pub struct TabItem {
    /// The tab's label.
    pub label: String,
    /// The panel content, rendered once and hidden while the tab isn't selected.
    pub panel: ViewFn,
}

impl std::fmt::Debug for TabItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TabItem")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// APG tabs with automatic activation: arrows, Home and End move and select; only the selected
/// tab is a tab stop.
#[component]
pub fn Tabs(
    /// Unique id prefix.
    #[prop(into)]
    id: String,
    /// The accessible name of the tab list.
    #[prop(into)]
    label: String,
    /// The tabs.
    tabs: Vec<TabItem>,
) -> impl IntoView {
    let model = RwSignal::new(TabsModel::new(tabs.len()));
    let tab_id = {
        let id = id.clone();
        move |i: usize| format!("{id}-tab-{i}")
    };
    let panel_id = {
        let id = id.clone();
        move |i: usize| format!("{id}-panel-{i}")
    };
    let on_keydown = {
        let tab_id = tab_id.clone();
        move |ev: web_sys::KeyboardEvent| {
            let mut target = None;
            model.update(|m| target = m.on_key(Key::from_dom(&ev.key())));
            if let Some(i) = target {
                ev.prevent_default();
                focus_id(&tab_id(i));
            }
        }
    };
    let (buttons, panels): (Vec<_>, Vec<_>) = tabs
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            let selected = move || model.with(|m| m.selected() == i);
            let button = view! {
                <button
                    type="button"
                    role="tab"
                    id=tab_id(i)
                    aria-controls=panel_id(i)
                    aria-selected=move || selected().to_string()
                    tabindex=move || model.with(|m| m.tab_index(i))
                    class="border-b-2 px-3 py-1.5 text-sm font-medium aria-selected:border-primary aria-selected:text-fg border-transparent text-muted"
                    on:click=move |_| model.update(|m| m.select(i))
                >
                    {t.label}
                </button>
            };
            let panel = view! {
                <div role="tabpanel" id=panel_id(i) aria-labelledby=tab_id(i) tabindex="0" hidden=move || !selected()>
                    {t.panel.run()}
                </div>
            };
            (button, panel)
        })
        .unzip();
    view! {
        <div>
            <div role="tablist" aria-label=label class="flex border-b border-border" on:keydown=on_keydown>
                {buttons}
            </div>
            {panels}
        </div>
    }
}

const TOOLTIP_BASE: &str = "pointer-events-none invisible absolute left-0 top-full z-10 mt-1 w-max max-w-xs rounded bg-fg px-2 py-1 text-xs text-bg";
const TOOLTIP_SHOWN: &str = "group-hover:visible group-focus-within:visible";

/// The tooltip's classes: shown on hover or focus, unless Escape dismissed it.
#[must_use]
fn tooltip_class(dismissed: bool) -> String {
    if dismissed {
        TOOLTIP_BASE.to_owned()
    } else {
        format!("{TOOLTIP_BASE} {TOOLTIP_SHOWN}")
    }
}

/// A tooltip shown on hover and keyboard focus of its content, with CSS only. Give the focusable
/// child `aria-describedby=id` so assistive technology reads the text. Escape hides a shown
/// tooltip without moving focus (WCAG 1.4.13); moving the pointer or focus away shows it again.
#[component]
pub fn Tooltip(
    /// The id the focusable child references with `aria-describedby`.
    #[prop(into)]
    id: String,
    /// The tooltip text.
    #[prop(into)]
    text: String,
    /// The element the tooltip describes.
    children: Children,
) -> impl IntoView {
    let dismissed = RwSignal::new(false);
    view! {
        <span
            class="group relative inline-block"
            on:keydown=move |ev: web_sys::KeyboardEvent| {
                if Key::from_dom(&ev.key()) == Key::Escape && !dismissed.get_untracked() {
                    dismissed.set(true);
                    ev.prevent_default();
                    ev.stop_propagation();
                }
            }
            on:mouseleave=move |_| dismissed.set(false)
            on:focusout=move |_| dismissed.set(false)
        >
            {children()}
            <span role="tooltip" id=id class=move || tooltip_class(dismissed.get())>
                {text}
            </span>
        </span>
    }
}

/// A handle to push toasts from anywhere under a [`ToastRegion`], read from context.
#[derive(Debug, Clone, Copy)]
pub struct Toasts {
    queue: RwSignal<ToastQueue>,
    clock_ms: RwSignal<u64>,
}

impl Toasts {
    /// Shows a toast (or refreshes an identical one).
    pub fn push(&self, message: impl Into<String>, tone: Tone) {
        let now = self.clock_ms.get_untracked();
        self.queue.update(|q| {
            q.push(message, tone, now);
        });
    }
}

/// How often the toast clock ticks, in milliseconds.
const TOAST_TICK_MS: u64 = 250;

/// The toast stack. Provides [`Toasts`] to its children; hovering or focusing the stack pauses
/// every timer. Errors are announced as alerts, the rest politely.
#[component]
pub fn ToastRegion(
    /// At most this many toasts show at once.
    #[prop(default = 3)]
    max_visible: usize,
    /// The app, which can `use_context::<Toasts>()`.
    children: Children,
) -> impl IntoView {
    let queue = RwSignal::new(ToastQueue::new(max_visible));
    let clock_ms = RwSignal::new(0_u64);
    provide_context(Toasts { queue, clock_ms });
    if let Ok(handle) = set_interval_with_handle(
        move || {
            clock_ms.update(|c| *c += TOAST_TICK_MS);
            let now = clock_ms.get_untracked();
            queue.update(|q| q.tick(now));
        },
        std::time::Duration::from_millis(TOAST_TICK_MS),
    ) {
        on_cleanup(move || handle.clear());
    }
    let pause = move |paused: bool| {
        let now = clock_ms.get_untracked();
        queue.update(|q| q.set_paused(paused, now));
    };
    view! {
        {children()}
        <div
            aria-label="Notifications"
            role="region"
            class="fixed bottom-4 right-4 z-20 flex w-80 flex-col gap-2"
            on:mouseenter=move |_| pause(true)
            on:mouseleave=move |_| pause(false)
            on:focusin=move |_| pause(true)
            on:focusout=move |_| pause(false)
        >
            <For
                each=move || queue.with(|q| q.visible().to_vec())
                key=|t| (t.id, t.count)
                children=move |t| {
                    let id = t.id;
                    let class = format!(
                        "flex items-start justify-between gap-2 rounded border p-3 text-sm {}",
                        t.tone.class()
                    );
                    view! {
                        <div role=t.role() class=class>
                            <span>
                                {t.message.clone()}
                                {(t.count > 1).then(|| format!(" ×{}", t.count))}
                            </span>
                            <Button
                                variant=ButtonVariant::Ghost
                                on_click=Callback::new(move |()| queue.update(|q| q.dismiss(id)))
                            >
                                "Dismiss"
                            </Button>
                        </div>
                    }
                }
            />
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dismissed_tooltip_drops_the_hover_and_focus_rules_so_it_stays_hidden() {
        assert!(tooltip_class(false).contains("group-hover:visible"));
        assert!(tooltip_class(false).contains("group-focus-within:visible"));
        let dismissed = tooltip_class(true);
        assert!(dismissed.contains("invisible"), "{dismissed}");
        assert!(!dismissed.contains("group-"), "{dismissed}");
    }
}
