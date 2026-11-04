// SPDX-License-Identifier: AGPL-3.0-only
//! Buttons and pagination.

use leptos::prelude::*;

use crate::pagination::{PageItem, Pages};

/// The visual weight of a [`Button`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonVariant {
    /// The main action on a page.
    Primary,
    /// A normal action.
    #[default]
    Secondary,
    /// A low-emphasis action.
    Ghost,
}

impl ButtonVariant {
    const fn class(self) -> &'static str {
        match self {
            Self::Primary => "bg-primary text-primary-fg hover:opacity-90",
            Self::Secondary => "border border-border bg-surface text-fg hover:bg-tone-neutral-bg",
            Self::Ghost => "text-fg hover:bg-tone-neutral-bg",
        }
    }
}

/// A native `<button type="button">`.
#[component]
pub fn Button(
    /// Visual weight.
    #[prop(optional)]
    variant: ButtonVariant,
    /// Disables the button.
    #[prop(into, optional)]
    disabled: Signal<bool>,
    /// Called on click.
    #[prop(into, optional)]
    on_click: Option<Callback<()>>,
    /// The label.
    children: Children,
) -> impl IntoView {
    let class = format!(
        "inline-flex items-center rounded px-3 py-1.5 text-sm font-medium disabled:cursor-not-allowed disabled:opacity-50 {}",
        variant.class()
    );
    view! {
        <button
            type="button"
            class=class
            disabled=disabled
            on:click=move |_| {
                if let Some(on_click) = on_click {
                    on_click.run(());
                }
            }
        >
            {children()}
        </button>
    }
}

/// Previous/next and numbered page links for a [`Pages`] value.
#[component]
pub fn Pagination(
    /// The current paging state.
    #[prop(into)]
    pages: Signal<Pages>,
    /// Called with the 1-based page the user picked.
    on_select: Callback<usize>,
) -> impl IntoView {
    let go = move |page: usize| move |()| on_select.run(page);
    view! {
        <nav aria-label="Pagination">
            <ul class="flex items-center gap-1">
                <li>
                    <Button
                        variant=ButtonVariant::Ghost
                        disabled=Signal::derive(move || !pages.get().has_prev())
                        on_click=Callback::new(move |()| {
                            let p = pages.get_untracked();
                            on_select.run(p.page().saturating_sub(1));
                        })
                    >
                        "Previous"
                    </Button>
                </li>
                {move || {
                    let current = pages.get().page();
                    pages
                        .get()
                        .items(1)
                        .into_iter()
                        .map(|item| match item {
                            PageItem::Gap => {
                                view! { <li aria-hidden="true" class="px-1 text-muted">"…"</li> }
                                    .into_any()
                            }
                            PageItem::Page(n) => {
                                let variant = if n == current {
                                    ButtonVariant::Primary
                                } else {
                                    ButtonVariant::Ghost
                                };
                                let aria_current = (n == current).then_some("page");
                                view! {
                                    <li aria-current=aria_current>
                                        <Button variant=variant on_click=Callback::new(go(n))>
                                            {n.to_string()}
                                        </Button>
                                    </li>
                                }
                                    .into_any()
                            }
                        })
                        .collect_view()
                }}
                <li>
                    <Button
                        variant=ButtonVariant::Ghost
                        disabled=Signal::derive(move || !pages.get().has_next())
                        on_click=Callback::new(move |()| {
                            let p = pages.get_untracked();
                            on_select.run(p.page() + 1);
                        })
                    >
                        "Next"
                    </Button>
                </li>
            </ul>
        </nav>
    }
}
