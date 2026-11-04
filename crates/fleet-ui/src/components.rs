// SPDX-License-Identifier: AGPL-3.0-only
//! The Leptos views of the kit: thin wrappers over the view-model modules. Native elements
//! first, WAI-ARIA APG patterns for anything custom, text nodes only.

#![expect(
    clippy::must_use_candidate,
    clippy::needless_pass_by_value,
    reason = "Leptos's #[component] macro generates the props structs and takes props by value"
)]

pub mod controls;
pub mod display;
pub mod forms;
pub mod layout;
pub mod overlays;
pub mod table;
pub mod virtual_list;
