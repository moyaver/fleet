// SPDX-License-Identifier: AGPL-3.0-only
//! Page frame and primary navigation.

use leptos::prelude::*;

/// One link in the primary navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavItem {
    /// Link text.
    pub label: String,
    /// Link target.
    pub href: String,
    /// Whether this is the page being shown (`aria-current="page"`).
    pub current: bool,
}

/// The page frame: a skip link, the `nav` slot and the main landmark.
#[component]
pub fn Layout(
    /// The header, usually a [`Nav`].
    #[prop(into)]
    nav: ViewFn,
    /// The page content, rendered inside `<main>`.
    children: Children,
) -> impl IntoView {
    view! {
        <div class="flex min-h-screen flex-col">
            <a
                href="#main"
                class="sr-only focus:not-sr-only focus:absolute focus:left-2 focus:top-2 focus:z-10 focus:rounded focus:bg-primary focus:px-3 focus:py-2 focus:text-primary-fg"
            >
                "Skip to content"
            </a>
            {nav.run()}
            <main id="main" class="mx-auto w-full max-w-7xl flex-1 p-4">
                {children()}
            </main>
        </div>
    }
}

/// The header with the product title (the page's `h1`) and the primary navigation links.
#[component]
pub fn Nav(
    /// The product title.
    #[prop(into)]
    title: String,
    /// The links, in order.
    #[prop(into)]
    items: Vec<NavItem>,
) -> impl IntoView {
    view! {
        <header class="border-b border-border bg-surface">
            <div class="mx-auto flex max-w-7xl items-center gap-6 px-4 py-3">
                <h1 class="text-lg font-semibold">{title}</h1>
                <nav aria-label="Primary">
                    <ul class="flex gap-4">
                        {items
                            .into_iter()
                            .map(|item| {
                                let current = item.current.then_some("page");
                                view! {
                                    <li>
                                        <a
                                            href=item.href
                                            aria-current=current
                                            class="rounded px-2 py-1 text-muted hover:text-fg aria-[current=page]:font-semibold aria-[current=page]:text-fg"
                                        >
                                            {item.label}
                                        </a>
                                    </li>
                                }
                            })
                            .collect_view()}
                    </ul>
                </nav>
            </div>
        </header>
    }
}
