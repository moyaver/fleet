// SPDX-License-Identifier: AGPL-3.0-only
//! The Fleet dashboard (Leptos CSR). M0 only proves the wasm → embed → axum chain;
//! screens start in M4 (PLAN §10).

use leptos::prelude::*;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    view! {
        <main>
            <h1>"Fleet"</h1>
            <p>"Live observability for Maestro tests in CI. This is the M0 hello page."</p>
        </main>
    }
}
