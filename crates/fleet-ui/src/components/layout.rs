// SPDX-License-Identifier: AGPL-3.0-only
//! Page frame: the skip link, the top bar and the main landmark.

use leptos::prelude::*;

/// The page frame: a skip link (the first focus stop), the top bar and the main landmark.
#[component]
pub fn Layout(
    /// The top bar, usually a [`TopBar`].
    #[prop(into)]
    nav: ViewFn,
    /// The page content, rendered inside `<main>`.
    children: Children,
) -> impl IntoView {
    view! {
        <div class="flex min-h-screen flex-col">
            <a href="#main" class="skip">"Skip to content"</a>
            {nav.run()}
            <main id="main" class="wrap flex-1">
                {children()}
            </main>
        </div>
    }
}

/// The top bar: one rule under it, no panel. The children are the brand, the project label,
/// the navigation and the right-hand group, laid out by the caller.
#[component]
pub fn TopBar(
    /// The brand, navigation and account controls.
    children: Children,
) -> impl IntoView {
    view! {
        <header class="top">
            <div class="top-in">{children()}</div>
        </header>
    }
}
