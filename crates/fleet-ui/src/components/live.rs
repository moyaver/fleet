// SPDX-License-Identifier: AGPL-3.0-only
//! The live indicator of the top bar: where a page stands on its live feed, in plain text.
//!
//! It is not a live region: "updated 2 s ago" changes every second and must not be read out.
//! Run-level events are announced by a separate, batched status region.

use leptos::prelude::*;

/// What the live indicator says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveState {
    /// The first connection is being made.
    Connecting,
    /// Subscribed: frames are arriving.
    Live,
    /// The socket was lost; the page shows its last data until a new snapshot.
    Reconnecting,
    /// The person paused updates; frames are held until they show them.
    Paused,
    /// The run is final and no further updates will come.
    Settled,
}

impl LiveState {
    /// The word shown and the value of `data-state`.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Connecting => "Connecting",
            Self::Live => "Live",
            Self::Reconnecting => "Reconnecting",
            Self::Paused => "Paused",
            Self::Settled => "Settled",
        }
    }

    /// The lowercase name for `data-state`, which the dot's colour follows.
    #[must_use]
    pub const fn attr(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Live => "live",
            Self::Reconnecting => "reconnecting",
            Self::Paused => "paused",
            Self::Settled => "settled",
        }
    }
}

/// The live state, its detail (such as "updated 2 s ago") and, when there is one, the action of
/// its menu (such as "Pause"). With an action the state is a button that opens a menu; without
/// one it is plain text. Only the text changes each second, so an open menu stays open.
#[component]
pub fn LiveIndicator(
    /// Where the page stands.
    #[prop(into)]
    state: Signal<LiveState>,
    /// What follows the word, such as "updated 2 s ago". Empty shows nothing.
    #[prop(into)]
    detail: Signal<String>,
    /// The menu's one action, such as "Pause" or "Show 12 updates". `None` means no menu.
    #[prop(into)]
    action: Signal<Option<String>>,
    /// Runs the action. Chosen from the menu, which then closes.
    on_action: Callback<()>,
    /// Unique id of the menu on the page.
    #[prop(into)]
    menu_id: String,
) -> impl IntoView {
    let has_menu = Memo::new(move |_| action.with(Option::is_some));
    let word = move || state.get().word();
    let word_attr = move || state.get().attr();
    let detail_text = move || detail.get();
    let has_detail = move || !detail.with(String::is_empty);
    view! {
        <span class="live" data-state=move || state.get().attr()>
            {move || {
                if has_menu.get() {
                    let id = menu_id.clone();
                    view! {
                        <button
                            type="button"
                            class="inline-flex min-h-8 items-center gap-2 text-sm"
                            aria-haspopup="menu"
                            popovertarget=id.clone()
                        >
                            <span class="live-dot" aria-hidden="true"></span>
                            <span data-testid="connection" data-state=word_attr>{word}</span>
                            <Show when=has_detail>
                                <span class="muted">" · " {detail_text}</span>
                            </Show>
                            <span aria-hidden="true" class="muted">" ▾"</span>
                        </button>
                        <div id=id.clone() popover="auto" role="menu" aria-label="Live updates" class="min-w-40 rounded border border-line-strong bg-bg p-1 text-fg">
                            <button
                                type="button"
                                role="menuitem"
                                popovertarget=id.clone()
                                popovertargetaction="hide"
                                class="block w-full rounded px-3 py-1.5 text-left text-sm hover:bg-band"
                                on:click=move |_| on_action.run(())
                            >
                                {move || action.get().unwrap_or_default()}
                            </button>
                        </div>
                    }
                        .into_any()
                } else {
                    view! {
                        <span class="live-dot" aria-hidden="true"></span>
                        <span data-testid="connection" data-state=word_attr>{word}</span>
                        <Show when=has_detail>
                            <span class="muted">" · " {detail_text}</span>
                        </Show>
                    }
                        .into_any()
                }
            }}
        </span>
    }
}

/// A page's one status region for run-level events (issue #149): polite, so it waits for the
/// reader to be idle, and silent until its text changes. The text is set by the page's batcher,
/// which reads out at most once per few seconds; the live indicator above never announces.
#[component]
pub fn Announcements(
    /// The batch to read out, empty when nothing is new.
    #[prop(into)]
    text: Signal<String>,
) -> impl IntoView {
    view! {
        <p class="sr-only" role="status" aria-live="polite" data-testid="announcements">
            {move || text.get()}
        </p>
    }
}
