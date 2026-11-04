// SPDX-License-Identifier: AGPL-3.0-only
//! The Fleet dashboard (Leptos CSR). A minimal shell built from the `fleet-ui` kit; screens
//! arrive later in M4 (PLAN §10).

use fleet_ui::components::{
    display::EmptyState,
    layout::{Layout, Nav, NavItem},
};
use leptos::prelude::*;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    let nav = || {
        view! {
            <Nav
                title="Fleet"
                items=vec![NavItem {
                    label: "Overview".to_owned(),
                    href: "/".to_owned(),
                    current: true,
                }]
            />
        }
    };
    view! {
        <Layout nav=nav>
            <EmptyState
                title="No runs yet"
                description="Runs appear here as soon as an agent reports in."
            />
        </Layout>
    }
}
