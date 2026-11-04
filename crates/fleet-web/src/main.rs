// SPDX-License-Identifier: AGPL-3.0-only
//! The Fleet dashboard's entry point: mounts [`fleet_web::app::App`] into the page.

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(fleet_web::app::App);
}
